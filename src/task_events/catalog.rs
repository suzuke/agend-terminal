//! Bounded task-state projection for the catalog shadow path.
//!
//! This P2 shadow deliberately has no authority yet: it projects incumbent
//! replay results into O(tasks) state and verifies that every catalog read is
//! fresh before it can be compared with the incumbent answer.

use super::{
    DoneSource, HistoryEntry, InstanceName, PrId, TaskBoardState, TaskEvent, TaskEventEnvelope,
    TaskId, TaskRecord, TaskStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Enough recent activity for the task-board detail view without retaining an
/// unbounded audit timeline in memory.
pub const RECENT_HISTORY_LIMIT: usize = 16;
const MANIFEST_SCHEMA: u32 = 1;
const CHECKPOINT_SCHEMA: u32 = 1;
const MANIFEST_FILE: &str = "MANIFEST.json";
const CHECKPOINT_FILE: &str = "catalog.checkpoint.json";

static CATALOGS: std::sync::LazyLock<
    parking_lot::Mutex<BTreeMap<PathBuf, Arc<StrictTaskCatalog>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(BTreeMap::new()));
static SHADOW_DIVERGENCES: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
std::thread_local! {
    static BOARD_REBUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Return the daemon-local shadow catalog for one AgEnD home.
pub fn for_home(home: &Path) -> Arc<StrictTaskCatalog> {
    let key = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut catalogs = CATALOGS.lock();
    if let Some(catalog) = catalogs.get(&key) {
        return Arc::clone(catalog);
    }

    let catalog = Arc::new(build_catalog(&key));
    catalogs.insert(key, Arc::clone(&catalog));
    catalog
}

pub(crate) fn record_shadow_divergence() {
    SHADOW_DIVERGENCES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn shadow_divergence_count() -> u64 {
    SHADOW_DIVERGENCES.load(Ordering::Relaxed)
}

/// Canonical within-file fold order used by the incumbent replay.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OrderKey {
    timestamp_ns: i64,
    instance: InstanceName,
    seq: u64,
}

impl OrderKey {
    fn from_envelope(env: &TaskEventEnvelope) -> Result<Self, OrderedApplyError> {
        let timestamp_ns = chrono::DateTime::parse_from_rfc3339(&env.timestamp)
            .map_err(|_| OrderedApplyError::InvalidTimestamp(env.timestamp.clone()))?
            .timestamp_nanos_opt()
            .unwrap_or(0);
        Ok(Self {
            timestamp_ns,
            instance: env.instance.clone(),
            seq: env.seq,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OrderedApplyError {
    InvalidTimestamp(String),
    OutOfOrder {
        previous: OrderKey,
        received: OrderKey,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct HotLogStamp {
    inode: u64,
    len: u64,
    mtime_ns: i128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HotLogFreshness {
    Current,
    CatchUp { start: u64, end: u64 },
    Stale,
}

fn classify_hot_log(cursor: HotLogStamp, observed: HotLogStamp) -> HotLogFreshness {
    if observed == cursor {
        HotLogFreshness::Current
    } else if observed.inode == cursor.inode && observed.len > cursor.len {
        HotLogFreshness::CatchUp {
            start: cursor.len,
            end: observed.len,
        }
    } else {
        HotLogFreshness::Stale
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BoardCursor {
    hot_log: HotLogStamp,
    live_offset: u64,
}

impl BoardCursor {
    pub fn from_folded_hot_log(inode: u64, len: u64, mtime_ns: i128) -> Self {
        Self {
            hot_log: HotLogStamp {
                inode,
                len,
                mtime_ns,
            },
            live_offset: len,
        }
    }

    pub fn live_offset(&self) -> u64 {
        self.live_offset
    }

    fn classify_observed(&self, inode: u64, len: u64, mtime_ns: i128) -> HotLogFreshness {
        classify_hot_log(
            self.hot_log,
            HotLogStamp {
                inode,
                len,
                mtime_ns,
            },
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ArchiveManifestV1 {
    schema: u32,
    adopted_at: String,
    archives: Vec<ArchiveManifestEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ArchiveManifestEntry {
    file: String,
    len: u64,
    mtime_ns: i128,
    digest_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CheckpointV1 {
    schema: u32,
    board: String,
    cursor: BoardCursor,
    tasks: Vec<ProjectedTaskRecord>,
    last_seq_per_instance: BTreeMap<InstanceName, u64>,
    events_folded: u64,
    last_order_key: Option<OrderKey>,
    written_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BoardSetFreshness {
    Current,
    New { names: Vec<String> },
    Missing { names: Vec<String> },
}

fn classify_board_set(known: &BTreeSet<String>, observed: &BTreeSet<String>) -> BoardSetFreshness {
    let missing: Vec<_> = known.difference(observed).cloned().collect();
    if !missing.is_empty() {
        return BoardSetFreshness::Missing { names: missing };
    }

    let added: Vec<_> = observed.difference(known).cloned().collect();
    if added.is_empty() {
        BoardSetFreshness::Current
    } else {
        BoardSetFreshness::New { names: added }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Phase {
    Building,
    Ready,
    Unhealthy { since: String, causes: Vec<String> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogRouteError {
    NotFound,
    Unreadable,
    Ambiguous { boards: Vec<String> },
}

pub struct StrictTaskCatalog {
    home: Option<PathBuf>,
    inner: RwLock<CatalogInner>,
}

struct CatalogInner {
    phase: Phase,
    boards: BTreeMap<String, BoardProjection>,
    index: BTreeMap<TaskId, String>,
    duplicates: BTreeMap<TaskId, Vec<String>>,
}

impl StrictTaskCatalog {
    pub fn new(phase: Phase, boards: BTreeMap<String, BoardProjection>) -> Self {
        Self::with_home(None, phase, boards)
    }

    fn with_home(
        home: Option<PathBuf>,
        phase: Phase,
        boards: BTreeMap<String, BoardProjection>,
    ) -> Self {
        let mut index = BTreeMap::new();
        let mut duplicates: BTreeMap<TaskId, Vec<String>> = BTreeMap::new();
        for (board_id, board) in &boards {
            for task_id in board.tasks.keys() {
                if let Some(candidates) = duplicates.get_mut(task_id) {
                    candidates.push(board_id.clone());
                } else if let Some(first) = index.remove(task_id) {
                    duplicates.insert(task_id.clone(), vec![first, board_id.clone()]);
                } else {
                    index.insert(task_id.clone(), board_id.clone());
                }
            }
        }
        Self {
            home,
            inner: RwLock::new(CatalogInner {
                phase,
                boards,
                index,
                duplicates,
            }),
        }
    }

    fn ensure_fresh(&self) -> Result<(), CatalogRouteError> {
        match &self.home {
            Some(home) => self.refresh_all(home),
            None => Ok(()),
        }
    }

    fn refresh_all(&self, home: &Path) -> Result<(), CatalogRouteError> {
        let observed = board_paths(home).map_err(|cause| self.mark_unhealthy(cause))?;
        let observed_names: BTreeSet<_> = observed.keys().cloned().collect();
        let known_names: BTreeSet<_> = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .boards
            .keys()
            .cloned()
            .collect();

        match classify_board_set(&known_names, &observed_names) {
            BoardSetFreshness::Missing { names } => {
                return Err(self.mark_unhealthy(format!("missing boards: {}", names.join(", "))));
            }
            BoardSetFreshness::New { names } => {
                {
                    let mut inner = self
                        .inner
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    inner.phase = Phase::Building;
                }
                let mut additions = Vec::with_capacity(names.len());
                for name in names {
                    let path = &observed[&name];
                    let board = load_board_projection(path, &name)
                        .map_err(|cause| self.mark_unhealthy(format!("{name}: {cause}")))?;
                    additions.push((name, board));
                }
                let mut inner = self
                    .inner
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner.boards.extend(additions);
                rebuild_routes(&mut inner);
                inner.phase = Phase::Ready;
            }
            BoardSetFreshness::Current => {}
        }

        for (name, path) in observed {
            self.refresh_board(&name, &path)?;
        }
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.phase == Phase::Ready {
            Ok(())
        } else {
            Err(CatalogRouteError::Unreadable)
        }
    }

    fn refresh_board(&self, board_id: &str, board_path: &Path) -> Result<(), CatalogRouteError> {
        let observed = hot_log_stamp(board_path)
            .map_err(|cause| self.mark_unhealthy(format!("{board_id}: {cause}")))?;
        let freshness = {
            let inner = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let board = inner
                .boards
                .get(board_id)
                .ok_or(CatalogRouteError::Unreadable)?;
            let cursor = board.cursor().ok_or(CatalogRouteError::Unreadable)?;
            cursor.classify_observed(observed.inode, observed.len, observed.mtime_ns)
        };

        match freshness {
            HotLogFreshness::Current => Ok(()),
            HotLogFreshness::Stale => {
                Err(self.mark_unhealthy(format!("{board_id}: hot log identity or length changed")))
            }
            HotLogFreshness::CatchUp { start, end } => {
                let (envelopes, consumed) = read_tail(board_path, start, end)
                    .map_err(|cause| self.mark_unhealthy(format!("{board_id}: {cause}")))?;
                let mut inner = self
                    .inner
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut next = inner
                    .boards
                    .get(board_id)
                    .cloned()
                    .ok_or(CatalogRouteError::Unreadable)?;
                if let Err(err) = next.apply_ordered_batch(&envelopes) {
                    inner.phase = Phase::Unhealthy {
                        since: chrono::Utc::now().to_rfc3339(),
                        causes: vec![format!("{board_id}: {err:?}")],
                    };
                    return Err(CatalogRouteError::Unreadable);
                }
                let folded_len = start + consumed;
                next.set_cursor(BoardCursor::from_folded_hot_log(
                    observed.inode,
                    folded_len,
                    observed.mtime_ns,
                ));
                inner.boards.insert(board_id.to_string(), next);
                rebuild_routes(&mut inner);
                Ok(())
            }
        }
    }

    fn mark_unhealthy(&self, cause: String) -> CatalogRouteError {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.phase = Phase::Unhealthy {
            since: chrono::Utc::now().to_rfc3339(),
            causes: vec![cause],
        };
        CatalogRouteError::Unreadable
    }

    fn rebuild_from_disk(&self, home: &Path) -> Result<(), CatalogRouteError> {
        let paths = board_paths(home).map_err(|cause| self.mark_unhealthy(cause))?;
        let mut boards = BTreeMap::new();
        for (name, path) in paths {
            let board = load_board_projection(&path, &name)
                .map_err(|cause| self.mark_unhealthy(format!("{name}: {cause}")))?;
            boards.insert(name, board);
        }
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.boards = boards;
        rebuild_routes(&mut inner);
        inner.phase = Phase::Ready;
        Ok(())
    }

    pub fn observe_board_set(
        &self,
        observed: &BTreeSet<String>,
        since: &str,
    ) -> Result<(), CatalogRouteError> {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let known = inner.boards.keys().cloned().collect();
        match classify_board_set(&known, observed) {
            BoardSetFreshness::Current => {
                if inner.phase == Phase::Ready {
                    Ok(())
                } else {
                    Err(CatalogRouteError::Unreadable)
                }
            }
            BoardSetFreshness::New { .. } => {
                inner.phase = Phase::Building;
                Err(CatalogRouteError::Unreadable)
            }
            BoardSetFreshness::Missing { names } => {
                inner.phase = Phase::Unhealthy {
                    since: since.to_string(),
                    causes: vec![format!("missing boards: {}", names.join(", "))],
                };
                Err(CatalogRouteError::Unreadable)
            }
        }
    }

    pub fn route(
        &self,
        task_id: &TaskId,
    ) -> Result<(String, Arc<ProjectedTaskRecord>), CatalogRouteError> {
        self.ensure_fresh()?;
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.phase != Phase::Ready {
            return Err(CatalogRouteError::Unreadable);
        }
        if let Some(boards) = inner.duplicates.get(task_id) {
            return Err(CatalogRouteError::Ambiguous {
                boards: boards.clone(),
            });
        }
        let board_id = inner
            .index
            .get(task_id)
            .ok_or(CatalogRouteError::NotFound)?;
        let task = inner.boards[board_id]
            .task_snapshot(task_id)
            .expect("catalog index must reference its source record");
        Ok((board_id.clone(), task))
    }

    pub fn all_tasks(&self) -> Result<Vec<Arc<ProjectedTaskRecord>>, CatalogRouteError> {
        self.ensure_fresh()?;
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.phase != Phase::Ready {
            return Err(CatalogRouteError::Unreadable);
        }
        Ok(inner
            .boards
            .values()
            .flat_map(BoardProjection::task_snapshots)
            .collect())
    }

    pub fn board(
        &self,
        board_id: &str,
    ) -> Result<Vec<Arc<ProjectedTaskRecord>>, CatalogRouteError> {
        self.ensure_fresh()?;
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.phase != Phase::Ready {
            return Err(CatalogRouteError::Unreadable);
        }
        inner
            .boards
            .get(board_id)
            .map(BoardProjection::task_snapshots)
            .ok_or(CatalogRouteError::NotFound)
    }

    /// Compare one incumbent board replay with the bounded shadow projection.
    pub(crate) fn board_matches_replay(
        &self,
        board_id: &str,
        replay: &TaskBoardState,
    ) -> Result<bool, CatalogRouteError> {
        self.ensure_fresh()?;
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.phase != Phase::Ready {
            return Err(CatalogRouteError::Unreadable);
        }
        inner
            .boards
            .get(board_id)
            .map(|board| board.matches_replay(replay))
            .ok_or(CatalogRouteError::NotFound)
    }

    pub fn statuses(
        &self,
        task_ids: &[TaskId],
    ) -> Result<Vec<(TaskId, Option<TaskStatus>)>, CatalogRouteError> {
        self.ensure_fresh()?;
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.phase != Phase::Ready {
            return Err(CatalogRouteError::Unreadable);
        }

        let mut statuses = Vec::with_capacity(task_ids.len());
        for task_id in task_ids {
            if let Some(boards) = inner.duplicates.get(task_id) {
                return Err(CatalogRouteError::Ambiguous {
                    boards: boards.clone(),
                });
            }
            let status = inner.index.get(task_id).map(|board_id| {
                inner.boards[board_id]
                    .task(task_id)
                    .expect("catalog index must reference its source record")
                    .status
            });
            statuses.push((task_id.clone(), status));
        }
        Ok(statuses)
    }

    pub fn snapshot_advisory(&self) -> (Phase, Vec<Arc<ProjectedTaskRecord>>) {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let snapshots = inner
            .boards
            .values()
            .flat_map(BoardProjection::task_snapshots)
            .collect();
        (inner.phase.clone(), snapshots)
    }
}

/// The single production task-event commit path. Existing task-event append
/// APIs are intentionally kept as compatibility shims over this function.
pub(crate) fn commit_at<F>(
    board: &Path,
    instance: &InstanceName,
    build: F,
) -> anyhow::Result<Result<Vec<u64>, String>>
where
    F: FnOnce(&TaskBoardState) -> Result<Vec<TaskEvent>, String>,
{
    let (home, board_id) = board_identity(board)?;
    // The legacy append path created a new project board lazily. Preserve that
    // behavior before catalog discovery so the board is folded before commit.
    std::fs::create_dir_all(board)?;
    let catalog = for_home(&home);
    if catalog.ensure_fresh().is_err() {
        super::recover_half_writes_at(board);
        catalog
            .rebuild_from_disk(&home)
            .map_err(|_| anyhow::anyhow!("task catalog is unreadable"))?;
    }

    let log_path = super::log_path(board);
    let lock_path = log_path.with_extension("jsonl.lock");
    let file_lock = crate::store::acquire_file_lock(&lock_path)?;
    let mut inner = catalog
        .inner
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if inner.phase != Phase::Ready {
        anyhow::bail!("task catalog is not ready");
    }

    catch_up_board_locked(&mut inner, &board_id, board)?;
    let state = super::replay_uncached(board)?;
    let events = match build(&state) {
        Ok(events) => events,
        Err(reason) => return Ok(Err(reason)),
    };
    if events.is_empty() {
        return Ok(Ok(Vec::new()));
    }

    let count = events.len();
    let (start_seq, hot_lines) =
        super::next_seq_under_lock(board, &log_path, instance, count as u64)?;
    let timestamp = next_commit_timestamp(
        inner
            .boards
            .get(&board_id)
            .and_then(BoardProjection::last_order_key),
    );
    let emitter_id = match crate::agent::resolve_instance(board, instance.as_str()) {
        Ok((id, _)) => Some(id.full()),
        Err(error) => {
            tracing::debug!(instance = %instance, %error, "emitter ID resolution failed");
            None
        }
    };

    let mut envelopes = Vec::with_capacity(count);
    let mut seqs = Vec::with_capacity(count);
    let mut lines = Vec::with_capacity(count);
    for (offset, event) in events.into_iter().enumerate() {
        let seq = start_seq + offset as u64;
        let envelope = TaskEventEnvelope {
            schema_version: super::SCHEMA_VERSION,
            seq,
            timestamp: timestamp.clone(),
            instance: instance.clone(),
            emitter_id: emitter_id.clone(),
            event,
        };
        lines.push(serde_json::to_string(&envelope)?);
        seqs.push(seq);
        envelopes.push(envelope);
    }

    use std::io::Write;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    for line in lines {
        writeln!(log, "{line}")?;
    }
    log.sync_all()?;

    let mut next = inner
        .boards
        .get(&board_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("catalog board disappeared during commit"))?;
    next.apply_ordered_batch(&envelopes)
        .map_err(|error| anyhow::anyhow!("catalog apply after durable append: {error:?}"))?;
    let stamp = hot_log_stamp(board).map_err(anyhow::Error::msg)?;
    next.set_cursor(BoardCursor::from_folded_hot_log(
        stamp.inode,
        stamp.len,
        stamp.mtime_ns,
    ));
    inner.boards.insert(board_id.clone(), next);
    rebuild_routes(&mut inner);

    super::invalidate_replay_cache();
    drop(inner);
    drop(file_lock);

    let post_append_lines = hot_lines + envelopes.len();
    super::maybe_compact_events(board, post_append_lines);
    if post_append_lines > super::COMPACTION_HIGH_WATER {
        let manifest = manifest_for_archives(board).map_err(anyhow::Error::msg)?;
        write_manifest(board, &manifest).map_err(anyhow::Error::msg)?;
        catalog
            .rebuild_from_disk(&home)
            .map_err(|_| anyhow::anyhow!("task catalog rebuild after compaction failed"))?;
    }
    Ok(Ok(seqs))
}

pub(crate) fn board_identity(board: &Path) -> anyhow::Result<(PathBuf, String)> {
    let parent = board.parent();
    if parent
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        == Some("boards")
    {
        let home = parent
            .and_then(Path::parent)
            .ok_or_else(|| anyhow::anyhow!("board has no AgEnD home: {}", board.display()))?;
        let board_id = board
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("board has no project id: {}", board.display()))?;
        Ok((home.to_path_buf(), board_id.to_string()))
    } else {
        Ok((board.to_path_buf(), super::DEFAULT_PROJECT.to_string()))
    }
}

fn catch_up_board_locked(
    inner: &mut CatalogInner,
    board_id: &str,
    board: &Path,
) -> anyhow::Result<()> {
    let observed = hot_log_stamp(board).map_err(anyhow::Error::msg)?;
    let cursor = inner
        .boards
        .get(board_id)
        .and_then(BoardProjection::cursor)
        .ok_or_else(|| anyhow::anyhow!("catalog board is missing or has no cursor"))?;
    match cursor.classify_observed(observed.inode, observed.len, observed.mtime_ns) {
        HotLogFreshness::Current => Ok(()),
        HotLogFreshness::Stale => anyhow::bail!("task catalog hot log is stale"),
        HotLogFreshness::CatchUp { start, end } => {
            let (envelopes, consumed) = read_tail(board, start, end).map_err(anyhow::Error::msg)?;
            let mut next = inner
                .boards
                .get(board_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("catalog board disappeared"))?;
            next.apply_ordered_batch(&envelopes)
                .map_err(|error| anyhow::anyhow!("catalog catch-up: {error:?}"))?;
            next.set_cursor(BoardCursor::from_folded_hot_log(
                observed.inode,
                start + consumed,
                observed.mtime_ns,
            ));
            inner.boards.insert(board_id.to_string(), next);
            rebuild_routes(inner);
            Ok(())
        }
    }
}

fn next_commit_timestamp(last: Option<&OrderKey>) -> String {
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let nanos = last
        .map(|key| now.max(key.timestamp_ns.saturating_add(1)))
        .unwrap_or(now);
    chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(nanos)
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn manifest_path(board: &Path) -> PathBuf {
    super::archive_dir(board).join(MANIFEST_FILE)
}

fn checkpoint_path(board: &Path) -> PathBuf {
    board.join(CHECKPOINT_FILE)
}

fn archive_paths(board: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = super::archive_dir(board);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = std::fs::read_dir(&dir)
        .map_err(|err| format!("read {}: {err}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn modified_ns(metadata: &std::fs::Metadata) -> i128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
        .unwrap_or(0)
}

fn digest_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn manifest_for_archives(board: &Path) -> Result<ArchiveManifestV1, String> {
    let mut archives = Vec::new();
    for path in archive_paths(board)? {
        let metadata =
            std::fs::metadata(&path).map_err(|err| format!("stat {}: {err}", path.display()))?;
        archives.push(ArchiveManifestEntry {
            file: path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| format!("non-UTF8 archive name: {}", path.display()))?
                .to_string(),
            len: metadata.len(),
            mtime_ns: modified_ns(&metadata),
            digest_sha256: digest_file(&path)?,
        });
    }
    Ok(ArchiveManifestV1 {
        schema: MANIFEST_SCHEMA,
        adopted_at: chrono::Utc::now().to_rfc3339(),
        archives,
    })
}

fn write_manifest(board: &Path, manifest: &ArchiveManifestV1) -> Result<(), String> {
    let path = manifest_path(board);
    std::fs::create_dir_all(super::archive_dir(board))
        .map_err(|err| format!("create manifest directory: {err}"))?;
    let bytes = serde_json::to_vec(manifest).map_err(|err| format!("serialize manifest: {err}"))?;
    crate::store::atomic_write(&path, &bytes)
        .map_err(|err| format!("write {}: {err}", path.display()))
}

pub(super) fn refresh_archive_manifest(board: &Path) -> anyhow::Result<()> {
    let manifest = manifest_for_archives(board).map_err(anyhow::Error::msg)?;
    write_manifest(board, &manifest).map_err(anyhow::Error::msg)
}

fn load_manifest(board: &Path) -> Result<Option<ArchiveManifestV1>, String> {
    let path = manifest_path(board);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("read {}: {err}", path.display())),
    };
    let manifest: ArchiveManifestV1 =
        serde_json::from_slice(&bytes).map_err(|err| format!("parse {}: {err}", path.display()))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(format!(
            "unsupported archive manifest schema {}",
            manifest.schema
        ));
    }
    verify_manifest_hints(board, &manifest)?;
    Ok(Some(manifest))
}

fn verify_manifest_hints(board: &Path, manifest: &ArchiveManifestV1) -> Result<(), String> {
    let paths = archive_paths(board)?;
    if paths.len() != manifest.archives.len() {
        return Err("archive manifest file set mismatch".to_string());
    }
    for (path, entry) in paths.iter().zip(&manifest.archives) {
        if path.file_name().and_then(|name| name.to_str()) != Some(entry.file.as_str()) {
            return Err("archive manifest name mismatch".to_string());
        }
        let metadata =
            std::fs::metadata(path).map_err(|err| format!("stat {}: {err}", path.display()))?;
        if metadata.len() != entry.len || modified_ns(&metadata) != entry.mtime_ns {
            return Err(format!(
                "archive manifest hint mismatch: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn scrub_manifest(board: &Path, manifest: &ArchiveManifestV1) -> Result<(), String> {
    verify_manifest_hints(board, manifest)?;
    for entry in &manifest.archives {
        let path = super::archive_dir(board).join(&entry.file);
        if digest_file(&path)? != entry.digest_sha256 {
            return Err(format!("archive digest mismatch: {}", path.display()));
        }
    }
    Ok(())
}

fn write_checkpoint(
    board: &Path,
    board_id: &str,
    projection: &BoardProjection,
) -> Result<(), String> {
    let checkpoint = CheckpointV1 {
        schema: CHECKPOINT_SCHEMA,
        board: board_id.to_string(),
        cursor: projection
            .cursor
            .ok_or_else(|| "projection has no cursor".to_string())?,
        tasks: projection
            .tasks
            .values()
            .map(|task| task.as_ref().clone())
            .collect(),
        last_seq_per_instance: projection.last_seq_per_instance.clone(),
        events_folded: projection.events_folded,
        last_order_key: projection.last_order_key.clone(),
        written_at: chrono::Utc::now().to_rfc3339(),
    };
    let bytes =
        serde_json::to_vec(&checkpoint).map_err(|err| format!("serialize checkpoint: {err}"))?;
    crate::store::atomic_write(&checkpoint_path(board), &bytes)
        .map_err(|err| format!("write checkpoint: {err}"))
}

fn load_checkpoint(board: &Path, board_id: &str) -> Result<Option<BoardProjection>, String> {
    let path = checkpoint_path(board);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("read {}: {err}", path.display())),
    };
    let checkpoint: CheckpointV1 =
        serde_json::from_slice(&bytes).map_err(|err| format!("parse {}: {err}", path.display()))?;
    if checkpoint.schema != CHECKPOINT_SCHEMA || checkpoint.board != board_id {
        return Err("checkpoint schema or board mismatch".to_string());
    }

    let observed = hot_log_stamp(board)?;
    let cursor = checkpoint.cursor.hot_log;
    if observed.len < checkpoint.cursor.live_offset
        || (cursor.inode != 0 && observed.inode != cursor.inode)
    {
        return Err("checkpoint hot-log cursor mismatch".to_string());
    }

    let mut projection = BoardProjection {
        tasks: checkpoint
            .tasks
            .into_iter()
            .map(|task| (task.id.clone(), Arc::new(task)))
            .collect(),
        last_seq_per_instance: checkpoint.last_seq_per_instance,
        events_folded: checkpoint.events_folded,
        last_order_key: checkpoint.last_order_key,
        cursor: Some(checkpoint.cursor),
    };
    if observed.len > checkpoint.cursor.live_offset {
        let (tail, consumed) = read_tail(board, checkpoint.cursor.live_offset, observed.len)?;
        projection
            .apply_ordered_batch(&tail)
            .map_err(|err| format!("checkpoint tail is not canonical: {err:?}"))?;
        projection.set_cursor(BoardCursor::from_folded_hot_log(
            observed.inode,
            checkpoint.cursor.live_offset + consumed,
            observed.mtime_ns,
        ));
    } else {
        projection.set_cursor(BoardCursor::from_folded_hot_log(
            observed.inode,
            observed.len,
            observed.mtime_ns,
        ));
    }
    Ok(Some(projection))
}

fn build_catalog(home: &Path) -> StrictTaskCatalog {
    let paths = match board_paths(home) {
        Ok(paths) => paths,
        Err(cause) => {
            return StrictTaskCatalog::with_home(
                Some(home.to_path_buf()),
                Phase::Unhealthy {
                    since: chrono::Utc::now().to_rfc3339(),
                    causes: vec![cause],
                },
                BTreeMap::new(),
            );
        }
    };

    let mut boards = BTreeMap::new();
    for (name, path) in paths {
        match load_board_projection(&path, &name) {
            Ok(board) => {
                boards.insert(name, board);
            }
            Err(cause) => {
                return StrictTaskCatalog::with_home(
                    Some(home.to_path_buf()),
                    Phase::Unhealthy {
                        since: chrono::Utc::now().to_rfc3339(),
                        causes: vec![format!("{name}: {cause}")],
                    },
                    boards,
                );
            }
        }
    }

    StrictTaskCatalog::with_home(Some(home.to_path_buf()), Phase::Ready, boards)
}

fn board_paths(home: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut boards = BTreeMap::from([(
        super::DEFAULT_PROJECT.to_string(),
        super::board_root(home, super::DEFAULT_PROJECT),
    )]);
    let boards_dir = home.join("boards");
    if !boards_dir.exists() {
        return Ok(boards);
    }
    let entries = std::fs::read_dir(&boards_dir)
        .map_err(|err| format!("read {}: {err}", boards_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read board entry: {err}"))?;
        let file_type = entry
            .file_type()
            .map_err(|err| format!("stat {}: {err}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        boards.insert(name, entry.path());
    }
    Ok(boards)
}

fn build_board_projection(board: &Path) -> Result<BoardProjection, String> {
    #[cfg(test)]
    BOARD_REBUILDS.with(|count| count.set(count.get() + 1));
    let replay = super::replay_strict_at_incumbent(board)
        .map_err(|err| format!("strict replay {}: {}", err.path.display(), err.cause))?;
    let mut projection = BoardProjection::from_replay(replay);
    let order_high_water = super::stream_envelopes_at(board)
        .map_err(|err| format!("read canonical envelopes: {err}"))?
        .iter()
        .map(OrderKey::from_envelope)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("invalid canonical order key: {err:?}"))?
        .into_iter()
        .max();
    projection.last_order_key = order_high_water;
    let stamp = hot_log_stamp(board)?;
    projection.set_cursor(BoardCursor::from_folded_hot_log(
        stamp.inode,
        stamp.len,
        stamp.mtime_ns,
    ));
    Ok(projection)
}

fn load_board_projection(board: &Path, board_id: &str) -> Result<BoardProjection, String> {
    match load_manifest(board)? {
        Some(manifest) => {
            match load_checkpoint(board, board_id) {
                Ok(Some(projection)) => return Ok(projection),
                Ok(None) | Err(_) => {}
            }
            scrub_manifest(board, &manifest)?;
            let projection = build_board_projection(board)?;
            write_checkpoint(board, board_id, &projection)?;
            Ok(projection)
        }
        None => {
            // One-time adoption: prove the legacy bytes first, then publish the
            // manifest and bounded checkpoint. A failed fold leaves no trust file.
            let projection = build_board_projection(board)?;
            let manifest = manifest_for_archives(board)?;
            write_manifest(board, &manifest)?;
            write_checkpoint(board, board_id, &projection)?;
            Ok(projection)
        }
    }
}

fn rebuild_routes(inner: &mut CatalogInner) {
    inner.index.clear();
    inner.duplicates.clear();
    for (board_id, board) in &inner.boards {
        for task_id in board.tasks.keys() {
            if let Some(candidates) = inner.duplicates.get_mut(task_id) {
                candidates.push(board_id.clone());
            } else if let Some(first) = inner.index.remove(task_id) {
                inner
                    .duplicates
                    .insert(task_id.clone(), vec![first, board_id.clone()]);
            } else {
                inner.index.insert(task_id.clone(), board_id.clone());
            }
        }
    }
}

fn hot_log_stamp(board: &Path) -> Result<HotLogStamp, String> {
    let path = super::log_path(board);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HotLogStamp {
                inode: 0,
                len: 0,
                mtime_ns: 0,
            });
        }
        Err(err) => return Err(format!("stat {}: {err}", path.display())),
    };
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
        .unwrap_or(0);
    Ok(HotLogStamp {
        inode: file_identity(&metadata),
        len: metadata.len(),
        mtime_ns,
    })
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn file_identity(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
        ^ metadata.len().rotate_left(17)
}

fn read_tail(board: &Path, start: u64, end: u64) -> Result<(Vec<TaskEventEnvelope>, u64), String> {
    let path = super::log_path(board);
    let mut file =
        std::fs::File::open(&path).map_err(|err| format!("open {}: {err}", path.display()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|err| format!("seek {}: {err}", path.display()))?;
    let mut bytes = vec![0; (end - start) as usize];
    file.read_exact(&mut bytes)
        .map_err(|err| format!("read {}: {err}", path.display()))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|err| format!("non-UTF8 task-event tail: {err}"))?;
    let (complete, fragment) = super::split_complete_and_fragment(text);
    let consumed = if fragment.is_some() {
        text.rfind('\n').map(|index| index + 1).unwrap_or(0)
    } else {
        text.len()
    };
    let mut envelopes = Vec::with_capacity(complete.len());
    for line in complete {
        envelopes.push(super::parse_envelope_strict(line)?);
    }
    Ok((envelopes, consumed as u64))
}

/// Current task state stored by the catalog. Unlike [`TaskRecord`], this type
/// is bounded with respect to the number of events applied to a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectedTaskRecord {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub priority: String,
    pub status: TaskStatus,
    pub owner: Option<InstanceName>,
    pub linked_prs: Vec<PrId>,
    pub block_reason: Option<String>,
    pub created_by: InstanceName,
    pub created_at: String,
    pub updated_at: String,
    pub due_at: Option<String>,
    pub depends_on: Vec<TaskId>,
    pub routed_to: Option<InstanceName>,
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<TaskId>,
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<bool>,
    #[serde(alias = "dispatched_at", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    pub history_len: u64,
    pub last_folded_event: Option<(InstanceName, u64)>,
    pub recent_history: VecDeque<HistoryEntry>,
}

impl From<TaskRecord> for ProjectedTaskRecord {
    fn from(task: TaskRecord) -> Self {
        let history_len = task.history.len() as u64;
        let last_folded_event = task
            .history
            .last()
            .map(|entry| (entry.instance.clone(), entry.seq));
        let recent_history = task
            .history
            .into_iter()
            .rev()
            .take(RECENT_HISTORY_LIMIT)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        Self {
            id: task.id,
            title: task.title,
            description: task.description,
            priority: task.priority,
            status: task.status,
            owner: task.owner,
            linked_prs: task.linked_prs,
            block_reason: task.block_reason,
            created_by: task.created_by,
            created_at: task.created_at,
            updated_at: task.updated_at,
            due_at: task.due_at,
            depends_on: task.depends_on,
            routed_to: task.routed_to,
            result: task.result,
            superseded_by: task.superseded_by,
            branch: task.branch,
            bind: task.bind,
            started_at: task.started_at,
            eta_secs: task.eta_secs,
            tags: task.tags,
            parent_id: task.parent_id,
            metadata: task.metadata,
            history_len,
            last_folded_event,
            recent_history,
        }
    }
}

impl ProjectedTaskRecord {
    pub(crate) fn matches_task_record(&self, task: &TaskRecord) -> bool {
        self.matches_replay(task)
    }

    pub(crate) fn current_task_record(&self) -> TaskRecord {
        TaskRecord {
            id: self.id.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            priority: self.priority.clone(),
            status: self.status,
            owner: self.owner.clone(),
            linked_prs: self.linked_prs.clone(),
            block_reason: self.block_reason.clone(),
            history: self.recent_history.iter().cloned().collect(),
            created_by: self.created_by.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            due_at: self.due_at.clone(),
            depends_on: self.depends_on.clone(),
            routed_to: self.routed_to.clone(),
            result: self.result.clone(),
            superseded_by: self.superseded_by.clone(),
            branch: self.branch.clone(),
            bind: self.bind,
            started_at: self.started_at.clone(),
            eta_secs: self.eta_secs,
            tags: self.tags.clone(),
            parent_id: self.parent_id.clone(),
            metadata: self.metadata.clone(),
        }
    }

    fn matches_replay(&self, task: &TaskRecord) -> bool {
        let recent_start = task.history.len().saturating_sub(RECENT_HISTORY_LIMIT);
        self.id == task.id
            && self.title == task.title
            && self.description == task.description
            && self.priority == task.priority
            && self.status == task.status
            && self.owner == task.owner
            && self.linked_prs == task.linked_prs
            && self.block_reason == task.block_reason
            && self.created_by == task.created_by
            && self.created_at == task.created_at
            && self.updated_at == task.updated_at
            && self.due_at == task.due_at
            && self.depends_on == task.depends_on
            && self.routed_to == task.routed_to
            && self.result == task.result
            && self.superseded_by == task.superseded_by
            && self.branch == task.branch
            && self.bind == task.bind
            && self.started_at == task.started_at
            && self.eta_secs == task.eta_secs
            && self.tags == task.tags
            && self.parent_id == task.parent_id
            && self.metadata == task.metadata
            && self.history_len == task.history.len() as u64
            && self
                .last_folded_event
                .as_ref()
                .map(|(instance, seq)| (instance, *seq))
                == task
                    .history
                    .last()
                    .map(|entry| (&entry.instance, entry.seq))
            && self
                .recent_history
                .iter()
                .eq(task.history[recent_start..].iter())
    }
}

/// One board's bounded shadow snapshot, built from the incumbent replay.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BoardProjection {
    tasks: BTreeMap<TaskId, Arc<ProjectedTaskRecord>>,
    last_seq_per_instance: BTreeMap<InstanceName, u64>,
    events_folded: u64,
    last_order_key: Option<OrderKey>,
    cursor: Option<BoardCursor>,
}

impl BoardProjection {
    /// Project an already-canonical replay while retaining its order high-water.
    pub fn from_sorted_envelopes(
        envelopes: &[TaskEventEnvelope],
    ) -> Result<Self, OrderedApplyError> {
        let mut projection = Self::default();
        projection.apply_ordered_batch(envelopes)?;
        Ok(projection)
    }

    /// Convert an incumbent replay result when the source envelopes are unavailable.
    /// This bridge cannot seed an order high-water; new rebuild paths should use
    /// [`Self::from_sorted_envelopes`].
    pub fn from_replay(state: TaskBoardState) -> Self {
        Self {
            tasks: state
                .tasks
                .into_iter()
                .map(|(id, task)| (id, Arc::new(task.into())))
                .collect(),
            last_seq_per_instance: state.last_seq_per_instance,
            events_folded: state.events_folded,
            last_order_key: None,
            cursor: None,
        }
    }

    pub fn task(&self, id: &TaskId) -> Option<&ProjectedTaskRecord> {
        self.tasks.get(id).map(Arc::as_ref)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &ProjectedTaskRecord> {
        self.tasks.values().map(Arc::as_ref)
    }

    pub fn task_snapshot(&self, id: &TaskId) -> Option<Arc<ProjectedTaskRecord>> {
        self.tasks.get(id).cloned()
    }

    pub fn task_snapshots(&self) -> Vec<Arc<ProjectedTaskRecord>> {
        self.tasks.values().cloned().collect()
    }

    pub fn last_seq_for(&self, instance: &InstanceName) -> Option<u64> {
        self.last_seq_per_instance.get(instance).copied()
    }

    pub fn events_folded(&self) -> u64 {
        self.events_folded
    }

    /// Compare the bounded shadow with the incumbent replay without cloning
    /// its unbounded audit history.
    pub fn matches_replay(&self, replay: &TaskBoardState) -> bool {
        self.events_folded == replay.events_folded
            && self.last_seq_per_instance == replay.last_seq_per_instance
            && self.tasks.len() == replay.tasks.len()
            && self.tasks.iter().all(|(id, task)| {
                replay
                    .tasks
                    .get(id)
                    .is_some_and(|replayed| task.matches_replay(replayed))
            })
    }

    pub fn last_order_key(&self) -> Option<&OrderKey> {
        self.last_order_key.as_ref()
    }

    pub fn cursor(&self) -> Option<&BoardCursor> {
        self.cursor.as_ref()
    }

    pub fn set_cursor(&mut self, cursor: BoardCursor) {
        self.cursor = Some(cursor);
    }

    /// Apply one canonical envelope only when its key advances.
    pub fn apply_ordered(&mut self, env: &TaskEventEnvelope) -> Result<bool, OrderedApplyError> {
        let received = OrderKey::from_envelope(env)?;
        if let Some(previous) = &self.last_order_key {
            if received <= *previous {
                return Err(OrderedApplyError::OutOfOrder {
                    previous: previous.clone(),
                    received,
                });
            }
        }

        let applied = self.apply(env);
        self.last_order_key = Some(received);
        Ok(applied)
    }

    /// Apply a canonical batch only after every envelope advances the cursor.
    pub fn apply_ordered_batch(
        &mut self,
        envelopes: &[TaskEventEnvelope],
    ) -> Result<(), OrderedApplyError> {
        let mut previous = self.last_order_key.clone();
        let mut keys = Vec::with_capacity(envelopes.len());
        for env in envelopes {
            let received = OrderKey::from_envelope(env)?;
            if let Some(previous) = &previous {
                if received <= *previous {
                    return Err(OrderedApplyError::OutOfOrder {
                        previous: previous.clone(),
                        received,
                    });
                }
            }
            previous = Some(received.clone());
            keys.push(received);
        }

        for (env, key) in envelopes.iter().zip(keys) {
            self.apply(env);
            self.last_order_key = Some(key);
        }
        Ok(())
    }

    /// Fold one canonical envelope into the bounded projection. Returns false
    /// when this emitter sequence was already observed.
    pub fn apply(&mut self, env: &TaskEventEnvelope) -> bool {
        let previous = self
            .last_seq_per_instance
            .get(&env.instance)
            .copied()
            .unwrap_or(0);
        if env.seq <= previous {
            return false;
        }

        self.last_seq_per_instance
            .insert(env.instance.clone(), env.seq);
        self.events_folded += 1;

        let task_id = env.event.task_id().clone();
        self.apply_event(&env.event, &task_id, &env.instance, &env.timestamp);

        if let Some(task) = self.tasks.get_mut(&task_id) {
            let task = Arc::make_mut(task);
            task.history_len += 1;
            task.last_folded_event = Some((env.instance.clone(), env.seq));
            task.recent_history.push_back(HistoryEntry {
                seq: env.seq,
                timestamp: env.timestamp.clone(),
                instance: env.instance.clone(),
                kind: env.event.kind_str(),
            });
            if task.recent_history.len() > RECENT_HISTORY_LIMIT {
                task.recent_history.pop_front();
            }
        }
        true
    }

    fn mutate_task(&mut self, task_id: &TaskId, mutate: impl FnOnce(&mut ProjectedTaskRecord)) {
        let Some(current) = self.tasks.get(task_id) else {
            return;
        };
        let mut updated = current.as_ref().clone();
        mutate(&mut updated);
        self.tasks.insert(task_id.clone(), Arc::new(updated));
    }

    fn apply_event(
        &mut self,
        event: &TaskEvent,
        task_id: &TaskId,
        instance: &InstanceName,
        timestamp: &str,
    ) {
        match event {
            TaskEvent::Created {
                title,
                description,
                priority,
                owner,
                due_at,
                depends_on,
                routed_to,
                branch,
                bind,
                eta_secs,
                tags,
                parent_id,
                ..
            } => {
                self.tasks.entry(task_id.clone()).or_insert_with(|| {
                    Arc::new(ProjectedTaskRecord {
                        id: task_id.clone(),
                        title: title.clone(),
                        description: description.clone(),
                        priority: priority.clone(),
                        status: TaskStatus::Open,
                        owner: owner.clone(),
                        linked_prs: Vec::new(),
                        block_reason: None,
                        created_by: instance.clone(),
                        created_at: timestamp.to_string(),
                        updated_at: timestamp.to_string(),
                        due_at: due_at.clone(),
                        depends_on: depends_on.clone(),
                        routed_to: routed_to.clone(),
                        result: None,
                        superseded_by: None,
                        branch: branch.clone(),
                        bind: *bind,
                        started_at: None,
                        eta_secs: *eta_secs,
                        tags: tags.clone(),
                        parent_id: parent_id.clone(),
                        metadata: BTreeMap::new(),
                        history_len: 0,
                        last_folded_event: None,
                        recent_history: VecDeque::new(),
                    })
                });
            }
            TaskEvent::Cancelled { .. } => {
                self.mutate_task(task_id, |task| {
                    task.status = TaskStatus::Cancelled;
                    task.updated_at = timestamp.to_string();
                });
                let child_ids = self
                    .tasks
                    .iter()
                    .filter(|(_, task)| {
                        task.parent_id.as_ref() == Some(task_id)
                            && matches!(task.status, TaskStatus::Open | TaskStatus::Claimed)
                    })
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                for child_id in child_ids {
                    self.mutate_task(&child_id, |child| {
                        child.status = TaskStatus::Cancelled;
                        child.updated_at = timestamp.to_string();
                    });
                }
            }
            TaskEvent::Claimed { by, .. } => {
                self.mutate_task(task_id, |task| {
                    task.status = TaskStatus::Claimed;
                    task.owner = Some(by.clone());
                    task.routed_to = None;
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::InProgress { by, .. } => {
                self.mutate_task(task_id, |task| {
                    task.status = TaskStatus::InProgress;
                    task.owner = Some(by.clone());
                    task.updated_at = timestamp.to_string();
                    if task.started_at.is_none() {
                        task.started_at = Some(timestamp.to_string());
                    }
                });
            }
            TaskEvent::Verified { .. } => {
                self.set_status(task_id, timestamp, TaskStatus::Verified);
            }
            TaskEvent::Done { source, .. } => {
                self.mutate_task(task_id, |task| {
                    task.status = TaskStatus::Done;
                    task.updated_at = timestamp.to_string();
                    match source {
                        DoneSource::OperatorManual { result, .. } => task.result = result.clone(),
                        DoneSource::ReportAutoClose { report_summary, .. }
                            if task.result.is_none() =>
                        {
                            task.result = Some(report_summary.clone());
                        }
                        _ => {}
                    }
                });
            }
            TaskEvent::Superseded { successor_id, .. } => {
                self.mutate_task(task_id, |task| {
                    task.status = TaskStatus::Superseded;
                    task.result = Some(format!("Superseded by {}", successor_id.0));
                    task.superseded_by = Some(successor_id.clone());
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::Linked { pr_id, .. } => {
                self.mutate_task(task_id, |task| {
                    if !task.linked_prs.contains(pr_id) {
                        task.linked_prs.push(*pr_id);
                    }
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::Blocked { reason, .. } => {
                self.mutate_task(task_id, |task| {
                    task.status = TaskStatus::Blocked;
                    task.block_reason = Some(reason.clone());
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::Unblocked { .. } => {
                self.mutate_task(task_id, |task| {
                    if task.status == TaskStatus::Blocked {
                        task.status = TaskStatus::Open;
                    }
                    task.block_reason = None;
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::Reopened { .. } => self.set_status(task_id, timestamp, TaskStatus::Open),
            TaskEvent::Released { .. } => {
                self.mutate_task(task_id, |task| {
                    task.status = TaskStatus::Open;
                    task.owner = None;
                    task.routed_to = None;
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::MovedToBacklog { .. } => {
                self.set_status(task_id, timestamp, TaskStatus::Backlog);
            }
            TaskEvent::MovedToReview { .. } => {
                self.set_status(task_id, timestamp, TaskStatus::InReview);
            }
            TaskEvent::TaskCloseProposed { .. } => self.touch(task_id, timestamp),
            TaskEvent::OwnerAssigned {
                owner, routed_to, ..
            } => {
                self.mutate_task(task_id, |task| {
                    task.owner = owner.clone();
                    task.routed_to = routed_to.clone();
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::PriorityChanged { priority, .. } => {
                self.mutate_task(task_id, |task| {
                    task.priority = priority.clone();
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::DescriptionUpdated { description, .. } => {
                self.mutate_task(task_id, |task| {
                    task.description = description.clone();
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::TagsSet { tags, .. } => {
                self.mutate_task(task_id, |task| {
                    task.tags = tags.clone();
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::ResultSet { result, .. } => {
                self.mutate_task(task_id, |task| {
                    task.result = Some(result.clone());
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::MetadataSet { key, value, .. } => {
                self.mutate_task(task_id, |task| {
                    task.metadata.insert(key.clone(), value.clone());
                    task.updated_at = timestamp.to_string();
                });
            }
            TaskEvent::BranchLinked { branch, .. } => {
                self.mutate_task(task_id, |task| {
                    task.branch = Some(branch.clone());
                    task.updated_at = timestamp.to_string();
                });
            }
        }
    }

    fn set_status(&mut self, task_id: &TaskId, timestamp: &str, status: TaskStatus) {
        self.mutate_task(task_id, |task| {
            task.status = status;
            task.updated_at = timestamp.to_string();
        });
    }

    fn touch(&mut self, task_id: &TaskId, timestamp: &str) {
        self.mutate_task(task_id, |task| {
            task.updated_at = timestamp.to_string();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_events::{
        ConfidenceScore, LinkSource, PrSnapshot, TaskEvent, TaskEventEnvelope, SCHEMA_VERSION,
    };

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let home =
            std::env::temp_dir().join(format!("agend-catalog-{}-{tag}-{id}", std::process::id()));
        std::fs::create_dir_all(&home).expect("create temp home");
        home
    }

    fn envelope(seq: u64, event: TaskEvent) -> TaskEventEnvelope {
        envelope_from("writer", seq, event)
    }

    fn envelope_from(instance: &str, seq: u64, event: TaskEvent) -> TaskEventEnvelope {
        TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq,
            timestamp: format!("2026-08-24T00:00:{:02}Z", seq % 60),
            instance: InstanceName::from(instance),
            emitter_id: None,
            event,
        }
    }

    fn envelope_at(timestamp: &str, seq: u64, event: TaskEvent) -> TaskEventEnvelope {
        let mut env = envelope(seq, event);
        env.timestamp = timestamp.to_string();
        env
    }

    fn created(task_id: &TaskId, title: &str) -> TaskEvent {
        TaskEvent::Created {
            task_id: task_id.clone(),
            title: title.into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        }
    }

    fn write_envelopes(path: &Path, envelopes: &[TaskEventEnvelope]) {
        let bytes = envelopes
            .iter()
            .map(|env| {
                format!(
                    "{}\n",
                    serde_json::to_string(env).expect("serialize envelope")
                )
            })
            .collect::<String>();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create event directory");
        }
        std::fs::write(path, bytes).expect("write envelopes");
    }

    fn rebuild_count() -> u64 {
        BOARD_REBUILDS.with(std::cell::Cell::get)
    }

    fn replay_with_metadata_events(count: u64) -> TaskBoardState {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let mut state = TaskBoardState::default();
        assert!(state.apply(&envelope(
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "bounded projection".into(),
                description: "parity fixture".into(),
                priority: "high".into(),
                owner: Some(InstanceName::from("owner")),
                due_at: Some("2026-08-25T00:00:00Z".into()),
                depends_on: vec![TaskId::from("t-20260823000000000000-1-1")],
                routed_to: Some(InstanceName::from("lead")),
                branch: Some("fix/catalog".into()),
                bind: Some(true),
                eta_secs: Some(60),
                tags: vec!["catalog".into()],
                parent_id: None,
            },
        )));
        for seq in 2..=count + 1 {
            assert!(state.apply(&envelope(
                seq,
                TaskEvent::MetadataSet {
                    task_id: task_id.clone(),
                    by: InstanceName::from("writer"),
                    key: "stable".into(),
                    value: serde_json::json!(true),
                },
            )));
        }
        state
    }

    #[test]
    fn projection_preserves_current_state_and_replay_high_water() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let replay = replay_with_metadata_events(20);
        let mut incumbent =
            serde_json::to_value(replay.tasks.get(&task_id).expect("replayed task"))
                .expect("serialize replayed task");
        incumbent
            .as_object_mut()
            .expect("task object")
            .remove("history");

        let projection = BoardProjection::from_replay(replay);
        let task = projection.task(&task_id).expect("projected task");
        let mut projected = serde_json::to_value(task).expect("serialize projected task");
        let projected_object = projected.as_object_mut().expect("projected task object");
        projected_object.remove("history_len");
        projected_object.remove("last_folded_event");
        projected_object.remove("recent_history");

        assert_eq!(projected, incumbent, "every current-state field must match");
        assert_eq!(task.history_len, 21);
        assert_eq!(task.recent_history.len(), RECENT_HISTORY_LIMIT);
        assert_eq!(task.recent_history.front().map(|entry| entry.seq), Some(6));
        assert_eq!(
            task.last_folded_event,
            Some((InstanceName::from("writer"), 21))
        );
        assert_eq!(
            projection.last_seq_for(&InstanceName::from("writer")),
            Some(21)
        );
        assert_eq!(projection.events_folded(), 21);
        assert_eq!(projection.tasks().count(), 1);
    }

    #[test]
    fn shadow_equivalence_detects_current_state_and_high_water_mismatches() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let replay = replay_with_metadata_events(20);
        let projection = BoardProjection::from_replay(replay.clone());

        assert!(projection.matches_replay(&replay));

        macro_rules! field_mismatch {
            ($field:ident, $value:expr) => {{
                let mut changed = replay.clone();
                changed
                    .tasks
                    .get_mut(&task_id)
                    .expect("replayed task")
                    .$field = $value;
                assert!(
                    !projection.matches_replay(&changed),
                    "{} mismatch must be detected",
                    stringify!($field)
                );
            }};
        }
        field_mismatch!(id, TaskId::from("different-id"));
        field_mismatch!(title, "different title".into());
        field_mismatch!(description, "different description".into());
        field_mismatch!(priority, "low".into());
        field_mismatch!(status, TaskStatus::Claimed);
        field_mismatch!(owner, None);
        field_mismatch!(linked_prs, vec![PrId(1)]);
        field_mismatch!(block_reason, Some("blocked".into()));
        field_mismatch!(created_by, InstanceName::from("different"));
        field_mismatch!(created_at, "different".into());
        field_mismatch!(updated_at, "different".into());
        field_mismatch!(due_at, None);
        field_mismatch!(depends_on, Vec::new());
        field_mismatch!(routed_to, None);
        field_mismatch!(result, Some("different".into()));
        field_mismatch!(superseded_by, Some(TaskId::from("successor")));
        field_mismatch!(branch, None);
        field_mismatch!(bind, Some(false));
        field_mismatch!(started_at, Some("different".into()));
        field_mismatch!(eta_secs, None);
        field_mismatch!(tags, Vec::new());
        field_mismatch!(parent_id, Some(TaskId::from("parent")));
        field_mismatch!(metadata, BTreeMap::new());

        let mut changed_history_len = replay.clone();
        let history = &mut changed_history_len
            .tasks
            .get_mut(&task_id)
            .expect("replayed task")
            .history;
        history.insert(0, history[0].clone());
        assert!(!projection.matches_replay(&changed_history_len));

        let mut changed_last_folded = projection.clone();
        Arc::make_mut(
            changed_last_folded
                .tasks
                .get_mut(&task_id)
                .expect("projected task"),
        )
        .last_folded_event = None;
        assert!(!changed_last_folded.matches_replay(&replay));

        let mut changed_recent = projection.clone();
        Arc::make_mut(
            changed_recent
                .tasks
                .get_mut(&task_id)
                .expect("projected task"),
        )
        .recent_history
        .back_mut()
        .expect("recent history")
        .kind = "different";
        assert!(!changed_recent.matches_replay(&replay));

        let mut changed_task_set = replay.clone();
        let extra_id = TaskId::from("extra-task");
        let mut extra = changed_task_set.tasks[&task_id].clone();
        extra.id = extra_id.clone();
        changed_task_set.tasks.insert(extra_id, extra);
        assert!(!projection.matches_replay(&changed_task_set));

        let mut changed_high_water = replay.clone();
        changed_high_water
            .last_seq_per_instance
            .insert(InstanceName::from("writer"), 999);
        assert!(!projection.matches_replay(&changed_high_water));

        let mut changed_event_count = replay;
        changed_event_count.events_folded += 1;
        assert!(!projection.matches_replay(&changed_event_count));
    }

    #[test]
    fn projection_size_is_bounded_by_tasks_not_events() {
        let one_x = BoardProjection::from_replay(replay_with_metadata_events(100));
        let ten_x = BoardProjection::from_replay(replay_with_metadata_events(1_000));
        let one_x_bytes = serde_json::to_vec(&one_x).expect("serialize 1x projection");
        let ten_x_bytes = serde_json::to_vec(&ten_x).expect("serialize 10x projection");

        assert_eq!(one_x.tasks().count(), ten_x.tasks().count());
        assert_eq!(
            one_x.tasks().next().expect("1x task").recent_history.len(),
            RECENT_HISTORY_LIMIT
        );
        assert_eq!(
            ten_x.tasks().next().expect("10x task").recent_history.len(),
            RECENT_HISTORY_LIMIT
        );
        assert!(
            ten_x_bytes.len() <= one_x_bytes.len() + 32,
            "10x events grew bounded projection from {} to {} bytes",
            one_x_bytes.len(),
            ten_x_bytes.len()
        );
    }

    #[test]
    fn task_snapshots_replace_only_the_changed_record() {
        let changed_id = TaskId::from("t-20260824000000000000-1-1");
        let untouched_id = TaskId::from("t-20260824000000000000-1-2");
        let create = |task_id: TaskId, title: &str| TaskEvent::Created {
            task_id,
            title: title.into(),
            description: "snapshot fixture".into(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        let mut projection = BoardProjection::from_sorted_envelopes(&[
            envelope(1, create(changed_id.clone(), "changed")),
            envelope(2, create(untouched_id.clone(), "untouched")),
        ])
        .expect("initial projection");
        let changed_before = projection
            .task_snapshot(&changed_id)
            .expect("changed snapshot");
        let untouched_before = projection
            .task_snapshot(&untouched_id)
            .expect("untouched snapshot");
        let all_before = projection.task_snapshots();
        assert_eq!(
            all_before
                .iter()
                .map(|task| task.id.clone())
                .collect::<Vec<_>>(),
            vec![changed_id.clone(), untouched_id.clone()]
        );
        assert!(Arc::ptr_eq(&all_before[0], &changed_before));
        assert!(Arc::ptr_eq(&all_before[1], &untouched_before));

        projection
            .apply_ordered(&envelope(
                3,
                TaskEvent::MetadataSet {
                    task_id: changed_id.clone(),
                    by: InstanceName::from("writer"),
                    key: "updated".into(),
                    value: serde_json::json!(true),
                },
            ))
            .expect("ordered update");

        let changed_after = projection
            .task_snapshot(&changed_id)
            .expect("changed snapshot");
        let untouched_after = projection
            .task_snapshot(&untouched_id)
            .expect("untouched snapshot");
        let all_after = projection.task_snapshots();
        assert!(!std::sync::Arc::ptr_eq(&changed_before, &changed_after));
        assert!(std::sync::Arc::ptr_eq(&untouched_before, &untouched_after));
        assert!(!Arc::ptr_eq(&all_before[0], &all_after[0]));
        assert!(Arc::ptr_eq(&all_before[1], &all_after[1]));
        assert!(changed_before.metadata.is_empty());
        assert!(all_before[0].metadata.is_empty());
        assert_eq!(
            changed_after.metadata.get("updated"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn advisory_catalog_snapshot_is_phase_labelled_and_pointer_only() {
        let first_id = TaskId::from("t-20260824000000000000-1-1");
        let second_id = TaskId::from("t-20260824000000000000-1-2");
        let create = |task_id: TaskId| TaskEvent::Created {
            task_id,
            title: "advisory fixture".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        let first =
            BoardProjection::from_sorted_envelopes(&[envelope(1, create(first_id.clone()))])
                .expect("first board");
        let second =
            BoardProjection::from_sorted_envelopes(&[envelope(1, create(second_id.clone()))])
                .expect("second board");
        let first_snapshot = first.task_snapshot(&first_id).expect("first snapshot");
        let second_snapshot = second.task_snapshot(&second_id).expect("second snapshot");
        let catalog = StrictTaskCatalog::new(
            Phase::Building,
            BTreeMap::from([("a-board".into(), first), ("b-board".into(), second)]),
        );

        let (phase, snapshots) = catalog.snapshot_advisory();

        assert_eq!(phase, Phase::Building);
        assert_eq!(
            snapshots
                .iter()
                .map(|task| task.id.clone())
                .collect::<Vec<_>>(),
            vec![first_id, second_id]
        );
        assert!(Arc::ptr_eq(&snapshots[0], &first_snapshot));
        assert!(Arc::ptr_eq(&snapshots[1], &second_snapshot));
    }

    #[test]
    fn catalog_route_is_ready_only_and_rejects_duplicate_ids() {
        let unique_id = TaskId::from("t-20260824000000000000-1-1");
        let duplicate_id = TaskId::from("t-20260824000000000000-1-2");
        let create = |task_id: TaskId| TaskEvent::Created {
            task_id,
            title: "route fixture".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        let first = BoardProjection::from_sorted_envelopes(&[
            envelope(1, create(unique_id.clone())),
            envelope(2, create(duplicate_id.clone())),
        ])
        .expect("first board");
        let unique_snapshot = first.task_snapshot(&unique_id).expect("unique snapshot");
        let second =
            BoardProjection::from_sorted_envelopes(&[envelope(1, create(duplicate_id.clone()))])
                .expect("second board");
        let boards = BTreeMap::from([
            ("a-board".into(), first.clone()),
            ("b-board".into(), second.clone()),
        ]);

        for phase in [
            Phase::Building,
            Phase::Unhealthy {
                since: "2026-08-24T00:00:00Z".into(),
                causes: vec!["fixture".into()],
            },
        ] {
            let catalog = StrictTaskCatalog::new(phase, boards.clone());
            assert!(matches!(
                catalog.route(&unique_id),
                Err(CatalogRouteError::Unreadable)
            ));
        }

        let catalog = StrictTaskCatalog::new(Phase::Ready, boards);
        let (board, task) = catalog.route(&unique_id).expect("unique route");
        assert_eq!(board, "a-board");
        assert!(Arc::ptr_eq(&task, &unique_snapshot));
        assert_eq!(
            catalog.route(&duplicate_id),
            Err(CatalogRouteError::Ambiguous {
                boards: vec!["a-board".into(), "b-board".into()],
            })
        );
        assert_eq!(
            catalog.route(&TaskId::from("missing")),
            Err(CatalogRouteError::NotFound)
        );
    }

    #[test]
    fn catalog_list_reads_are_ready_only_ordered_and_pointer_only() {
        let first_id = TaskId::from("t-20260824000000000000-1-1");
        let second_id = TaskId::from("t-20260824000000000000-1-2");
        let create = |task_id: TaskId| TaskEvent::Created {
            task_id,
            title: "list fixture".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        let first =
            BoardProjection::from_sorted_envelopes(&[envelope(1, create(first_id.clone()))])
                .expect("first board");
        let second =
            BoardProjection::from_sorted_envelopes(&[envelope(1, create(second_id.clone()))])
                .expect("second board");
        let first_snapshot = first.task_snapshot(&first_id).expect("first snapshot");
        let second_snapshot = second.task_snapshot(&second_id).expect("second snapshot");
        let boards = BTreeMap::from([("a-board".into(), first), ("b-board".into(), second)]);

        let building = StrictTaskCatalog::new(Phase::Building, boards.clone());
        assert_eq!(building.all_tasks(), Err(CatalogRouteError::Unreadable));
        assert_eq!(
            building.board("a-board"),
            Err(CatalogRouteError::Unreadable)
        );

        let ready = StrictTaskCatalog::new(Phase::Ready, boards);
        let all = ready.all_tasks().expect("ready all-tasks snapshot");
        assert_eq!(
            all.iter().map(|task| task.id.clone()).collect::<Vec<_>>(),
            vec![first_id, second_id]
        );
        assert!(Arc::ptr_eq(&all[0], &first_snapshot));
        assert!(Arc::ptr_eq(&all[1], &second_snapshot));

        let board = ready.board("b-board").expect("ready board snapshot");
        assert_eq!(board.len(), 1);
        assert!(Arc::ptr_eq(&board[0], &second_snapshot));
        assert_eq!(ready.board("missing"), Err(CatalogRouteError::NotFound));
    }

    #[test]
    fn catalog_statuses_are_ready_only_ordered_and_fail_closed() {
        let unique_id = TaskId::from("t-20260824000000000000-1-1");
        let duplicate_id = TaskId::from("t-20260824000000000000-1-2");
        let missing_id = TaskId::from("missing");
        let create = |task_id: TaskId| TaskEvent::Created {
            task_id,
            title: "status fixture".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        let first = BoardProjection::from_sorted_envelopes(&[
            envelope(1, create(unique_id.clone())),
            envelope(
                2,
                TaskEvent::Claimed {
                    task_id: unique_id.clone(),
                    by: InstanceName::from("owner"),
                },
            ),
            envelope(3, create(duplicate_id.clone())),
        ])
        .expect("first board");
        let second =
            BoardProjection::from_sorted_envelopes(&[envelope(1, create(duplicate_id.clone()))])
                .expect("second board");
        let boards = BTreeMap::from([("a-board".into(), first), ("b-board".into(), second)]);

        let requested = vec![unique_id.clone(), missing_id.clone(), unique_id.clone()];
        let building = StrictTaskCatalog::new(Phase::Building, boards.clone());
        assert_eq!(
            building.statuses(&requested),
            Err(CatalogRouteError::Unreadable)
        );

        let ready = StrictTaskCatalog::new(Phase::Ready, boards);
        assert_eq!(
            ready.statuses(&requested),
            Ok(vec![
                (unique_id, Some(TaskStatus::Claimed)),
                (missing_id, None),
                (
                    TaskId::from("t-20260824000000000000-1-1"),
                    Some(TaskStatus::Claimed),
                ),
            ])
        );
        assert_eq!(
            ready.statuses(std::slice::from_ref(&duplicate_id)),
            Err(CatalogRouteError::Ambiguous {
                boards: vec!["a-board".into(), "b-board".into()],
            })
        );
        assert_eq!(
            ready.statuses(&[TaskId::from("t-20260824000000000000-1-1"), duplicate_id,]),
            Err(CatalogRouteError::Ambiguous {
                boards: vec!["a-board".into(), "b-board".into()],
            })
        );
    }

    #[test]
    fn hot_log_freshness_shapes_are_fail_closed() {
        let cursor = HotLogStamp {
            inode: 7,
            len: 10,
            mtime_ns: 100,
        };

        assert_eq!(classify_hot_log(cursor, cursor), HotLogFreshness::Current);
        assert_eq!(
            classify_hot_log(
                cursor,
                HotLogStamp {
                    inode: 7,
                    len: 14,
                    mtime_ns: 101,
                },
            ),
            HotLogFreshness::CatchUp { start: 10, end: 14 }
        );
        assert_eq!(
            classify_hot_log(
                cursor,
                HotLogStamp {
                    inode: 7,
                    len: 14,
                    mtime_ns: 100,
                },
            ),
            HotLogFreshness::CatchUp { start: 10, end: 14 }
        );
        for stale in [
            HotLogStamp {
                inode: 8,
                len: 10,
                mtime_ns: 100,
            },
            HotLogStamp {
                inode: 8,
                len: 14,
                mtime_ns: 101,
            },
            HotLogStamp {
                inode: 7,
                len: 9,
                mtime_ns: 101,
            },
            HotLogStamp {
                inode: 7,
                len: 10,
                mtime_ns: 101,
            },
        ] {
            assert_eq!(classify_hot_log(cursor, stale), HotLogFreshness::Stale);
        }
    }

    #[test]
    fn board_cursor_records_the_folded_hot_log_position() {
        let cursor = BoardCursor::from_folded_hot_log(7, 10, 100);

        assert_eq!(cursor.live_offset(), 10);
        assert_eq!(
            cursor.classify_observed(7, 10, 100),
            HotLogFreshness::Current
        );
        assert_eq!(
            cursor.classify_observed(7, 14, 101),
            HotLogFreshness::CatchUp { start: 10, end: 14 }
        );
    }

    #[test]
    fn board_projection_keeps_its_optional_source_cursor() {
        let mut projection = BoardProjection::default();
        assert!(projection.cursor().is_none());

        projection.set_cursor(BoardCursor::from_folded_hot_log(7, 10, 100));

        assert_eq!(projection.cursor().map(BoardCursor::live_offset), Some(10));
    }

    #[test]
    fn board_set_freshness_compares_names_and_fails_closed_on_missing() {
        let known =
            std::collections::BTreeSet::from(["default".to_string(), "research".to_string()]);

        assert_eq!(
            classify_board_set(&known, &known),
            BoardSetFreshness::Current
        );
        assert_eq!(
            classify_board_set(
                &known,
                &std::collections::BTreeSet::from([
                    "default".to_string(),
                    "research".to_string(),
                    "support".to_string(),
                ]),
            ),
            BoardSetFreshness::New {
                names: vec!["support".to_string()],
            }
        );
        assert_eq!(
            classify_board_set(
                &known,
                &std::collections::BTreeSet::from(["default".to_string(), "support".to_string(),]),
            ),
            BoardSetFreshness::Missing {
                names: vec!["research".to_string()],
            }
        );
        assert_eq!(
            classify_board_set(
                &std::collections::BTreeSet::from([
                    "a-second-one".to_string(),
                    "default".to_string(),
                    "research".to_string(),
                ]),
                &std::collections::BTreeSet::from(["default".to_string()]),
            ),
            BoardSetFreshness::Missing {
                names: vec!["a-second-one".to_string(), "research".to_string()],
            }
        );
    }

    #[test]
    fn board_set_observation_updates_phase_without_discovering_or_folding() {
        let boards = BTreeMap::from([
            ("default".to_string(), BoardProjection::default()),
            ("research".to_string(), BoardProjection::default()),
        ]);

        let current = StrictTaskCatalog::new(Phase::Ready, boards.clone());
        assert_eq!(
            current.observe_board_set(
                &BTreeSet::from(["default".to_string(), "research".to_string()]),
                "2026-08-24T15:40:00Z",
            ),
            Ok(())
        );
        assert_eq!(current.snapshot_advisory().0, Phase::Ready);

        let building = StrictTaskCatalog::new(Phase::Building, boards.clone());
        assert_eq!(
            building.observe_board_set(
                &BTreeSet::from(["default".to_string(), "research".to_string()]),
                "2026-08-24T15:40:00Z",
            ),
            Err(CatalogRouteError::Unreadable)
        );
        assert_eq!(building.snapshot_advisory().0, Phase::Building);

        let added = StrictTaskCatalog::new(Phase::Ready, boards.clone());
        assert_eq!(
            added.observe_board_set(
                &BTreeSet::from([
                    "default".to_string(),
                    "research".to_string(),
                    "support".to_string(),
                ]),
                "2026-08-24T15:40:00Z",
            ),
            Err(CatalogRouteError::Unreadable)
        );
        assert_eq!(added.snapshot_advisory().0, Phase::Building);

        let missing = StrictTaskCatalog::new(Phase::Ready, boards);
        assert_eq!(
            missing.observe_board_set(
                &BTreeSet::from(["support".to_string()]),
                "2026-08-24T15:40:00Z",
            ),
            Err(CatalogRouteError::Unreadable)
        );
        assert_eq!(
            missing.snapshot_advisory().0,
            Phase::Unhealthy {
                since: "2026-08-24T15:40:00Z".to_string(),
                causes: vec!["missing boards: default, research".to_string()],
            }
        );
    }

    #[test]
    fn incremental_apply_matches_incumbent_replay() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let child_id = TaskId::from("t-20260824000000000000-1-2");
        let actor = InstanceName::from("owner");
        let snapshot = PrSnapshot {
            pr_state: "merged".into(),
            merge_sha: Some("abc123".into()),
            api_response_hash: "hash".into(),
            captured_at: "2026-08-24T00:00:00Z".into(),
        };
        let events = vec![
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "parent".into(),
                description: "original".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: Some(InstanceName::from("lead")),
                branch: None,
                bind: Some(true),
                eta_secs: Some(60),
                tags: Vec::new(),
                parent_id: None,
            },
            TaskEvent::Created {
                task_id: child_id.clone(),
                title: "child".into(),
                description: "cascade fixture".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: Some(task_id.clone()),
            },
            TaskEvent::Claimed {
                task_id: task_id.clone(),
                by: actor.clone(),
            },
            TaskEvent::InProgress {
                task_id: task_id.clone(),
                by: actor.clone(),
            },
            TaskEvent::Blocked {
                task_id: task_id.clone(),
                reason: "waiting".into(),
            },
            TaskEvent::Unblocked {
                task_id: task_id.clone(),
            },
            TaskEvent::MovedToBacklog {
                task_id: task_id.clone(),
            },
            TaskEvent::Reopened {
                task_id: task_id.clone(),
                reason: "retry".into(),
                source_evidence: "test".into(),
            },
            TaskEvent::OwnerAssigned {
                task_id: task_id.clone(),
                by: actor.clone(),
                owner: Some(actor.clone()),
                routed_to: Some(InstanceName::from("lead")),
            },
            TaskEvent::PriorityChanged {
                task_id: task_id.clone(),
                by: actor.clone(),
                priority: "high".into(),
            },
            TaskEvent::DescriptionUpdated {
                task_id: task_id.clone(),
                by: actor.clone(),
                description: "updated".into(),
            },
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["catalog".into()],
            },
            TaskEvent::ResultSet {
                task_id: task_id.clone(),
                by: actor.clone(),
                result: "explicit".into(),
            },
            TaskEvent::MetadataSet {
                task_id: task_id.clone(),
                by: actor.clone(),
                key: "key".into(),
                value: serde_json::json!("value"),
            },
            TaskEvent::BranchLinked {
                task_id: task_id.clone(),
                by: actor.clone(),
                branch: "fix/catalog".into(),
            },
            TaskEvent::Linked {
                task_id: task_id.clone(),
                pr_id: PrId(3347),
                source: LinkSource::Explicit {
                    authored_at: "2026-08-24T00:00:00Z".into(),
                },
                snapshot: snapshot.clone(),
            },
            TaskEvent::MovedToReview {
                task_id: task_id.clone(),
            },
            TaskEvent::Unblocked {
                task_id: task_id.clone(),
            },
            TaskEvent::Verified {
                task_id: task_id.clone(),
                by_reviewer: InstanceName::from("reviewer"),
                verdict: "VERIFIED".into(),
            },
            TaskEvent::TaskCloseProposed {
                task_id: task_id.clone(),
                candidate: DoneSource::LegacyBackfill {
                    sweep_id: "sweep".into(),
                    reasoning: "fixture".into(),
                    snapshot: Some(snapshot),
                },
                sweep_id: "sweep".into(),
                confidence: ConfidenceScore {
                    total: 1.0,
                    signal_count: 1,
                    sub_scores: BTreeMap::new(),
                },
            },
            TaskEvent::Done {
                task_id: task_id.clone(),
                by: actor.clone(),
                source: DoneSource::ReportAutoClose {
                    report_summary: "does not replace explicit".into(),
                    closed_at: "2026-08-24T00:00:00Z".into(),
                },
            },
            TaskEvent::Released {
                task_id: task_id.clone(),
                reason: "release".into(),
            },
            TaskEvent::Superseded {
                task_id: task_id.clone(),
                by: actor.clone(),
                successor_id: TaskId::from("t-20260824000000000000-1-3"),
            },
            TaskEvent::Claimed {
                task_id: child_id.clone(),
                by: actor.clone(),
            },
            TaskEvent::Cancelled {
                task_id: task_id.clone(),
                by: actor,
                reason: "cancel tree".into(),
            },
        ];

        let mut incumbent = TaskBoardState::default();
        let mut projection = BoardProjection::default();
        for (index, event) in events.into_iter().enumerate() {
            let env = envelope(index as u64 + 1, event);
            assert!(incumbent.apply(&env));
            assert!(projection.apply(&env));
            assert_eq!(
                projection,
                BoardProjection::from_replay(incumbent.clone()),
                "projection diverged at event {} ({})",
                env.seq,
                env.event.kind_str()
            );
        }

        assert_eq!(
            projection.task(&child_id).expect("child").status,
            TaskStatus::Cancelled,
            "parent cancellation must cascade exactly like incumbent replay"
        );
    }

    #[test]
    fn incremental_apply_dedupes_per_instance_and_bounds_history() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let created = envelope(
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "bounded".into(),
                description: "incremental".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
            },
        );
        let mut projection = BoardProjection::default();
        assert!(projection.apply(&created));
        assert!(!projection.apply(&created));

        for seq in 2..=25 {
            assert!(projection.apply(&envelope(
                seq,
                TaskEvent::MetadataSet {
                    task_id: task_id.clone(),
                    by: InstanceName::from("writer"),
                    key: "seq".into(),
                    value: serde_json::json!(seq),
                },
            )));
        }
        let other = envelope_from(
            "other-writer",
            2,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["other".into()],
            },
        );
        assert!(projection.apply(&other));
        assert!(!projection.apply(&envelope_from(
            "other-writer",
            1,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["stale".into()],
            },
        )));

        let task = projection.task(&task_id).expect("task");
        assert_eq!(task.history_len, 26);
        assert_eq!(task.recent_history.len(), RECENT_HISTORY_LIMIT);
        assert_eq!(task.recent_history.front().map(|entry| entry.seq), Some(11));
        assert_eq!(task.recent_history.back().map(|entry| entry.seq), Some(2));
        assert_eq!(projection.events_folded(), 26);
        assert_eq!(
            projection.last_seq_for(&InstanceName::from("writer")),
            Some(25)
        );
        assert_eq!(
            projection.last_seq_for(&InstanceName::from("other-writer")),
            Some(2)
        );
    }

    #[test]
    fn ordered_apply_rejects_non_advancing_keys_without_mutation() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let created = envelope_at(
            "2026-08-24T00:00:02Z",
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "ordered".into(),
                description: "tail gate".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
            },
        );
        let stale = envelope_at(
            "2026-08-24T00:00:01Z",
            2,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["stale".into()],
            },
        );
        let invalid = envelope_at(
            "not-a-timestamp",
            3,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["invalid".into()],
            },
        );
        let mut later_lower_instance = envelope_from(
            "aaa",
            2,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["later".into()],
            },
        );
        later_lower_instance.timestamp = "2026-08-24T00:00:03Z".into();
        let mut lower_instance = envelope_from(
            "AAA",
            99,
            TaskEvent::TagsSet {
                task_id: task_id.clone(),
                tags: vec!["wrong-instance".into()],
            },
        );
        lower_instance.timestamp = later_lower_instance.timestamp.clone();
        let mut lower_seq = envelope_from(
            "aaa",
            1,
            TaskEvent::TagsSet {
                task_id,
                tags: vec!["wrong-seq".into()],
            },
        );
        lower_seq.timestamp = later_lower_instance.timestamp.clone();

        let mut projection = BoardProjection::default();
        assert_eq!(projection.apply_ordered(&created), Ok(true));
        let accepted = projection.clone();

        assert!(matches!(
            projection.apply_ordered(&stale),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, accepted);
        assert!(matches!(
            projection.apply_ordered(&created),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, accepted);
        assert_eq!(
            projection.apply_ordered(&invalid),
            Err(OrderedApplyError::InvalidTimestamp(
                "not-a-timestamp".into()
            ))
        );
        assert_eq!(projection, accepted);

        assert_eq!(projection.apply_ordered(&later_lower_instance), Ok(true));
        let advanced = projection.clone();
        assert!(matches!(
            projection.apply_ordered(&lower_instance),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, advanced);
        assert!(matches!(
            projection.apply_ordered(&lower_seq),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, advanced);
    }

    #[test]
    fn canonical_initial_fold_matches_replay_and_seeds_order_cursor() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let tagged = |instance: &str, timestamp: &str, seq: u64, tag: &str| {
            let mut env = envelope_from(
                instance,
                seq,
                TaskEvent::TagsSet {
                    task_id: task_id.clone(),
                    tags: vec![tag.into()],
                },
            );
            env.timestamp = timestamp.into();
            env
        };
        let mut envelopes = vec![
            tagged("AAA", "2026-08-24T00:00:03Z", 1, "last"),
            tagged("zzz", "2026-08-24T00:00:02Z", 1, "third"),
            tagged("aaa", "2026-08-24T00:00:02Z", 2, "second"),
            tagged("aaa", "2026-08-24T00:00:02Z", 1, "first"),
            envelope_at(
                "2026-08-24T00:00:01Z",
                1,
                TaskEvent::Created {
                    task_id: task_id.clone(),
                    title: "initial fold".into(),
                    description: "cursor seed".into(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                    tags: Vec::new(),
                    parent_id: None,
                },
            ),
        ];
        let mut order_key_sorted = envelopes.clone();
        order_key_sorted.sort_by_key(|env| OrderKey::from_envelope(env).expect("valid key"));
        super::super::sort_envelopes(&mut envelopes);
        let identity =
            |env: &TaskEventEnvelope| (env.timestamp.clone(), env.instance.clone(), env.seq);
        assert_eq!(
            order_key_sorted.iter().map(identity).collect::<Vec<_>>(),
            envelopes.iter().map(identity).collect::<Vec<_>>()
        );

        let mut incumbent = TaskBoardState::default();
        for env in &envelopes {
            assert!(incumbent.apply(env));
        }
        let projection = BoardProjection::from_sorted_envelopes(&envelopes).expect("initial fold");
        let mut expected = BoardProjection::from_replay(incumbent);
        expected.last_order_key = projection.last_order_key.clone();
        assert_eq!(projection, expected);

        let mut stale = tagged("zzz", "2026-08-24T00:00:02Z", 2, "stale");
        stale.timestamp = "2026-08-24T00:00:02Z".into();
        let mut projection = projection;
        let accepted = projection.clone();
        assert!(matches!(
            projection.apply_ordered(&stale),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, accepted);

        let mut malformed = envelopes;
        malformed[1].timestamp = "not-a-timestamp".into();
        assert_eq!(
            BoardProjection::from_sorted_envelopes(&malformed),
            Err(OrderedApplyError::InvalidTimestamp(
                "not-a-timestamp".into()
            ))
        );
    }

    #[test]
    fn canonical_batch_is_atomic_and_matches_sequential_apply() {
        let task_id = TaskId::from("t-20260824000000000000-1-1");
        let event = |timestamp: &str, seq: u64, tag: &str| {
            envelope_at(
                timestamp,
                seq,
                TaskEvent::TagsSet {
                    task_id: task_id.clone(),
                    tags: vec![tag.into()],
                },
            )
        };
        let created = envelope_at(
            "2026-08-24T00:00:01Z",
            1,
            TaskEvent::Created {
                task_id: task_id.clone(),
                title: "catch-up".into(),
                description: "atomic batch".into(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
            },
        );
        let valid = vec![
            event("2026-08-24T00:00:02Z", 2, "second"),
            event("2026-08-24T00:00:03Z", 3, "third"),
        ];

        let mut projection = BoardProjection::default();
        projection.apply_ordered(&created).expect("seed");
        let seed = projection.clone();

        let mut malformed = valid.clone();
        malformed[1].timestamp = "not-a-timestamp".into();
        assert_eq!(
            projection.apply_ordered_batch(&malformed),
            Err(OrderedApplyError::InvalidTimestamp(
                "not-a-timestamp".into()
            ))
        );
        assert_eq!(projection, seed);

        assert!(matches!(
            projection.apply_ordered_batch(std::slice::from_ref(&created)),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, seed);

        let out_of_order = vec![valid[1].clone(), valid[0].clone()];
        assert!(matches!(
            projection.apply_ordered_batch(&out_of_order),
            Err(OrderedApplyError::OutOfOrder { .. })
        ));
        assert_eq!(projection, seed);

        let mut sequential = seed;
        for env in &valid {
            sequential.apply_ordered(env).expect("ordered event");
        }
        projection
            .apply_ordered_batch(&valid)
            .expect("ordered batch");
        assert_eq!(projection, sequential);
    }

    #[test]
    fn home_catalog_is_persistent_and_catches_up_appended_tail() {
        let home = tmp_home("catch-up");
        let task_id = TaskId::from("t-20260825000000000000-1-1");
        let writer = InstanceName::from("writer");

        super::super::append_at(&home, &writer, created(&task_id, "before")).expect("seed task");
        let first = for_home(&home);
        let second = for_home(&home);
        assert!(
            Arc::ptr_eq(&first, &second),
            "registry must retain one catalog"
        );
        assert_eq!(
            first.route(&task_id).expect("initial route").1.title,
            "before"
        );

        super::super::append_at(
            &home,
            &writer,
            TaskEvent::DescriptionUpdated {
                task_id: task_id.clone(),
                by: writer.clone(),
                description: "after".into(),
            },
        )
        .expect("append update");

        assert_eq!(
            first
                .route(&task_id)
                .expect("tail catch-up route")
                .1
                .description,
            "after"
        );
    }

    #[test]
    fn authority_fails_closed_when_known_hot_log_is_replaced() {
        let home = tmp_home("replaced-log");
        let task_id = TaskId::from("t-20260825000000000000-2-1");
        let writer = InstanceName::from("writer");
        super::super::append_at(&home, &writer, created(&task_id, "known")).expect("seed task");
        let catalog = for_home(&home);
        catalog.route(&task_id).expect("initial route");

        std::fs::write(super::super::log_path(&home), b"").expect("replace hot log");
        assert!(matches!(
            catalog.route(&task_id),
            Err(CatalogRouteError::Unreadable)
        ));
        assert!(matches!(
            catalog.snapshot_advisory().0,
            Phase::Unhealthy { .. }
        ));
    }

    #[test]
    fn authority_discovers_new_board_before_answering() {
        let home = tmp_home("new-board");
        let default_id = TaskId::from("t-20260825000000000000-3-1");
        let project_id = TaskId::from("t-20260825000000000000-3-2");
        let writer = InstanceName::from("writer");
        super::super::append_at(&home, &writer, created(&default_id, "default"))
            .expect("seed default");
        let catalog = for_home(&home);
        catalog.route(&default_id).expect("initial default route");

        let project = super::super::board_root(&home, "project");
        std::fs::create_dir_all(&project).expect("project board");
        super::super::append_at(&project, &writer, created(&project_id, "project"))
            .expect("seed project");

        let (board, task) = catalog.route(&project_id).expect("new board folded");
        assert_eq!(board, "project");
        assert_eq!(task.title, "project");
        assert_eq!(catalog.snapshot_advisory().0, Phase::Ready);
    }

    #[test]
    fn commit_advances_canonical_order_past_a_future_cursor() {
        let home = tmp_home("future-cursor");
        let task_id = TaskId::from("t-20260825000000000000-4-1");
        let writer = InstanceName::from("writer");
        let seeded = envelope_at("2099-01-01T00:00:00Z", 1, created(&task_id, "future"));
        std::fs::write(
            super::super::log_path(&home),
            format!("{}\n", serde_json::to_string(&seeded).expect("serialize")),
        )
        .expect("seed future event");

        super::super::append_at(
            &home,
            &writer,
            TaskEvent::DescriptionUpdated {
                task_id: task_id.clone(),
                by: writer.clone(),
                description: "ordered".into(),
            },
        )
        .expect("ordered commit");

        let envelopes = super::super::stream_envelopes_at(&home).expect("stream events");
        let keys = envelopes
            .iter()
            .map(OrderKey::from_envelope)
            .collect::<Result<Vec<_>, _>>()
            .expect("valid keys");
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            for_home(&home)
                .route(&task_id)
                .expect("route")
                .1
                .description,
            "ordered"
        );
    }

    #[test]
    fn commit_refuses_unreadable_board_without_writing_bytes() {
        let home = tmp_home("write-gate");
        let log = super::super::log_path(&home);
        let future = serde_json::json!({
            "schema_version": super::super::SCHEMA_VERSION + 1,
            "seq": 1,
            "timestamp": "2026-08-25T00:00:00Z",
            "instance": "future",
            "event": {
                "kind": "Created",
                "task_id": "t-20260825000000000000-5-1",
                "title": "future",
                "description": "",
                "priority": "normal",
                "owner": null
            }
        });
        let original = format!("{future}\n");
        std::fs::write(&log, &original).expect("future log");

        let result = super::super::append_at(
            &home,
            &InstanceName::from("writer"),
            created(&TaskId::from("t-20260825000000000000-5-2"), "must not land"),
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(log).expect("read log"), original);
    }

    #[test]
    fn adoption_writes_manifest_and_checkpoint_without_existing_archive_dir() {
        let home = tmp_home("adoption-files");
        assert!(!super::super::archive_dir(&home).exists());

        let projection =
            load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt empty board");

        assert_eq!(projection.events_folded(), 0);
        assert!(manifest_path(&home).is_file());
        assert!(checkpoint_path(&home).is_file());
        assert_eq!(
            load_manifest(&home)
                .expect("load manifest")
                .expect("manifest")
                .archives,
            Vec::new()
        );
    }

    #[test]
    fn checkpoint_reload_skips_full_rebuild_and_folds_only_hot_tail() {
        let home = tmp_home("checkpoint-tail");
        let task_id = TaskId::from("t-20260825000000000000-6-1");
        let writer = InstanceName::from("writer");
        let first = envelope(1, created(&task_id, "checkpoint"));
        write_envelopes(&super::super::log_path(&home), std::slice::from_ref(&first));

        let before = rebuild_count();
        let adopted =
            load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("initial adoption");
        assert_eq!(rebuild_count(), before + 1);
        assert_eq!(adopted.task(&task_id).expect("task").description, "");

        let update = envelope_at(
            "2026-08-25T00:01:00Z",
            2,
            TaskEvent::DescriptionUpdated {
                task_id: task_id.clone(),
                by: writer,
                description: "tail".into(),
            },
        );
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(super::super::log_path(&home))
            .expect("open hot log");
        use std::io::Write as _;
        writeln!(
            log,
            "{}",
            serde_json::to_string(&update).expect("serialize tail")
        )
        .expect("append tail");

        let reloaded =
            load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("checkpoint reload");
        assert_eq!(rebuild_count(), before + 1, "must not replay full history");
        assert_eq!(
            reloaded.task(&task_id).expect("tail task").description,
            "tail"
        );
    }

    #[test]
    fn invalid_checkpoint_is_scrubbed_rebuilt_and_replaced() {
        let home = tmp_home("checkpoint-heal");
        let task_id = TaskId::from("t-20260825000000000000-7-1");
        write_envelopes(
            &super::super::log_path(&home),
            &[envelope(1, created(&task_id, "heal"))],
        );
        load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt");

        let path = checkpoint_path(&home);
        let mut checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read checkpoint"))
                .expect("parse checkpoint");
        checkpoint["tasks"][0]["recent_history"][0]["kind"] =
            serde_json::Value::String("future_kind".into());
        std::fs::write(
            &path,
            serde_json::to_vec(&checkpoint).expect("serialize tamper"),
        )
        .expect("tamper checkpoint");

        let before = rebuild_count();
        let healed = load_board_projection(&home, super::super::DEFAULT_PROJECT)
            .expect("rebuild invalid checkpoint");
        assert_eq!(rebuild_count(), before + 1);
        assert_eq!(healed.task(&task_id).expect("healed task").title, "heal");
        let replaced: CheckpointV1 =
            serde_json::from_slice(&std::fs::read(path).expect("read replacement"))
                .expect("valid replacement");
        assert_eq!(replaced.schema, CHECKPOINT_SCHEMA);
    }

    #[test]
    fn manifest_digest_mismatch_fails_closed_when_rebuild_is_required() {
        let home = tmp_home("manifest-digest");
        let task_id = TaskId::from("t-20260825000000000000-8-1");
        let archive = super::super::archive_dir(&home).join("0001.jsonl");
        write_envelopes(&archive, &[envelope(1, created(&task_id, "archive"))]);
        load_board_projection(&home, super::super::DEFAULT_PROJECT).expect("adopt archive");

        let manifest_file = manifest_path(&home);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_file).expect("read manifest"))
                .expect("parse manifest");
        manifest["archives"][0]["digest_sha256"] = serde_json::Value::String("00".repeat(32));
        std::fs::write(
            &manifest_file,
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("tamper manifest");
        std::fs::remove_file(checkpoint_path(&home)).expect("remove checkpoint");

        let error = load_board_projection(&home, super::super::DEFAULT_PROJECT)
            .expect_err("digest mismatch must fail closed");
        assert!(error.contains("archive digest mismatch"), "{error}");
    }
}
