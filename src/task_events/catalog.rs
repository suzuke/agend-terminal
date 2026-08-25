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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    static ARCHIVE_BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Return the daemon-local shadow catalog for one AgEnD home.
pub fn for_home(home: &Path) -> Arc<StrictTaskCatalog> {
    let key = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut catalogs = CATALOGS.lock();
    if let Some(catalog) = catalogs.get(&key) {
        return Arc::clone(catalog);
    }

    if needs_background_adoption(&key) {
        let catalog = Arc::new(StrictTaskCatalog::with_home(
            Some(key.clone()),
            Phase::Building,
            BTreeMap::new(),
        ));
        catalogs.insert(key.clone(), Arc::clone(&catalog));
        drop(catalogs);
        let worker = Arc::clone(&catalog);
        // fire-and-forget: one-time legacy archive adoption publishes a terminal catalog phase.
        let _ = std::thread::Builder::new()
            .name("task-catalog-adoption".to_string())
            .spawn(move || {
                let _ = worker.rebuild_from_disk(&key);
            });
        return catalog;
    }

    let catalog = Arc::new(build_catalog(&key));
    catalogs.insert(key, Arc::clone(&catalog));
    catalog
}

fn needs_background_adoption(home: &Path) -> bool {
    board_paths(home).is_ok_and(|boards| {
        boards.values().any(|board| {
            !manifest_path(board).exists()
                && archive_paths(board).is_ok_and(|archives| !archives.is_empty())
        })
    })
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
    inode: [u8; 24],
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
    #[cfg(test)]
    pub fn from_folded_hot_log(inode: u64, len: u64, mtime_ns: i128) -> Self {
        Self::from_hot_log_identity(identity_from_u64(inode), len, mtime_ns)
    }

    fn from_hot_log_identity(inode: [u8; 24], len: u64, mtime_ns: i128) -> Self {
        Self {
            hot_log: HotLogStamp {
                inode,
                len,
                mtime_ns,
            },
            live_offset: len,
        }
    }

    #[cfg(test)]
    pub fn live_offset(&self) -> u64 {
        self.live_offset
    }

    fn classify_observed(&self, inode: [u8; 24], len: u64, mtime_ns: i128) -> HotLogFreshness {
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
    refresh: parking_lot::Mutex<()>,
    inner: RwLock<CatalogInner>,
    rebuild_in_flight: AtomicBool,
}

struct CatalogInner {
    phase: Phase,
    boards: BTreeMap<String, BoardProjection>,
    index: BTreeMap<TaskId, String>,
    duplicates: BTreeMap<TaskId, Vec<String>>,
}

impl StrictTaskCatalog {
    #[cfg(test)]
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
            refresh: parking_lot::Mutex::new(()),
            inner: RwLock::new(CatalogInner {
                phase,
                boards,
                index,
                duplicates,
            }),
            rebuild_in_flight: AtomicBool::new(false),
        }
    }

    fn ensure_fresh(&self) -> Result<(), CatalogRouteError> {
        match &self.home {
            Some(home) => {
                match self
                    .inner
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .phase
                    .clone()
                {
                    Phase::Building => return Err(CatalogRouteError::Unreadable),
                    Phase::Unhealthy { .. } => {
                        self.schedule_rebuild(home);
                        return Err(CatalogRouteError::Unreadable);
                    }
                    Phase::Ready => {}
                }
                let result = self.refresh_all(home, false);
                if result.is_err()
                    && matches!(
                        self.inner
                            .read()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .phase,
                        Phase::Unhealthy { .. }
                    )
                {
                    self.schedule_rebuild(home);
                }
                result
            }
            None => Ok(()),
        }
    }

    fn schedule_rebuild(&self, home: &Path) {
        if self
            .rebuild_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let key = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        let Some(catalog) = CATALOGS.lock().get(&key).cloned() else {
            self.rebuild_in_flight.store(false, Ordering::Release);
            return;
        };
        // fire-and-forget: one bounded self-healing rebuild publishes a terminal phase.
        let spawned = std::thread::Builder::new()
            .name("task-catalog-rebuild".to_string())
            .spawn(move || {
                let _ = catalog.rebuild_from_disk(&key);
                catalog.rebuild_in_flight.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.rebuild_in_flight.store(false, Ordering::Release);
        }
    }

    fn refresh_all(&self, home: &Path, allow_new_board: bool) -> Result<(), CatalogRouteError> {
        let _refresh = self.refresh.lock();
        self.refresh_all_locked(home, allow_new_board)
    }

    fn refresh_all_locked(
        &self,
        home: &Path,
        allow_new_board: bool,
    ) -> Result<(), CatalogRouteError> {
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
                if !allow_new_board {
                    return Err(CatalogRouteError::Unreadable);
                }
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
                next.set_cursor(BoardCursor::from_hot_log_identity(
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
        self.rebuild_from_disk_with_hook(home, |_| {})
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn rebuild_from_disk_with_hook(
        &self,
        home: &Path,
        mut after_offline_build: impl FnMut(usize),
    ) -> Result<(), CatalogRouteError> {
        {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.phase = Phase::Building;
        }

        let mut last_cause = "catalog rebuild did not produce a stable snapshot".to_string();
        for attempt in 0..3 {
            let paths = match board_paths(home) {
                Ok(paths) => paths,
                Err(cause) => {
                    last_cause = cause;
                    continue;
                }
            };
            let mut boards = BTreeMap::new();
            let mut failed = None;
            for (name, path) in &paths {
                match load_board_projection(path, name) {
                    Ok(board) => {
                        boards.insert(name.clone(), board);
                    }
                    Err(cause) => {
                        failed = Some(format!("{name}: {cause}"));
                        break;
                    }
                }
            }
            if let Some(cause) = failed {
                last_cause = cause;
                continue;
            }

            after_offline_build(attempt);

            let mut file_locks = Vec::with_capacity(paths.len());
            for path in paths.values() {
                if let Err(err) = std::fs::create_dir_all(path) {
                    failed = Some(format!("create {}: {err}", path.display()));
                    break;
                }
                let lock_path = super::log_path(path).with_extension("jsonl.lock");
                match crate::store::acquire_file_lock(&lock_path) {
                    Ok(lock) => file_locks.push(lock),
                    Err(err) => {
                        failed = Some(format!("lock {}: {err}", lock_path.display()));
                        break;
                    }
                }
            }
            if let Some(cause) = failed {
                last_cause = cause;
                continue;
            }

            for (name, path) in &paths {
                let projection = boards
                    .get_mut(name)
                    .expect("rebuilt board must have a projection");
                match catch_up_projection_locked(projection, path) {
                    Ok(true) => {}
                    Ok(false) => {
                        failed = Some(format!("{name}: hot log rotated during rebuild"));
                        break;
                    }
                    Err(cause) => {
                        failed = Some(format!("{name}: {cause}"));
                        break;
                    }
                }
            }
            if let Some(cause) = failed {
                last_cause = cause;
                continue;
            }

            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.boards = boards;
            rebuild_routes(&mut inner);
            inner.phase = Phase::Ready;
            drop(file_locks);
            return Ok(());
        }

        Err(self.mark_unhealthy(format!(
            "catalog rebuild exhausted 3 attempts: {last_cause}"
        )))
    }

    #[cfg(test)]
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
    ) -> Result<Option<bool>, CatalogRouteError> {
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
            .map(|board| board.matches_current_replay(replay))
            .ok_or(CatalogRouteError::NotFound)
    }

    #[cfg(test)]
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

    #[cfg(test)]
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
    if catalog
        .inner
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .phase
        != Phase::Ready
    {
        anyhow::bail!("task catalog is not ready");
    }
    let log_path = super::log_path(board);
    let lock_path = log_path.with_extension("jsonl.lock");
    let file_lock = crate::store::acquire_file_lock(&lock_path)?;
    if super::recover_half_writes_under_lock(board) {
        let projection = load_board_projection(board, &board_id)
            .map_err(|cause| anyhow::anyhow!("task catalog recovery failed: {cause}"))?;
        let mut inner = catalog
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.boards.insert(board_id.clone(), projection);
        rebuild_routes(&mut inner);
    }
    catalog
        .refresh_all(&home, true)
        .map_err(|_| anyhow::anyhow!("task catalog is unreadable"))?;
    // Keep the board lock, but do not hold the catalog lock while `build`
    // checks dependencies: a cross-board check may commit to another board.
    {
        let mut inner = catalog
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.phase != Phase::Ready {
            anyhow::bail!("task catalog is not ready");
        }
        catch_up_board_locked(&mut inner, &board_id, board)?;
    }
    let state = {
        let inner = catalog
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .boards
            .get(&board_id)
            .ok_or_else(|| anyhow::anyhow!("catalog board disappeared before commit"))?
            .current_state()
    };
    let events = match build(&state) {
        Ok(events) => events,
        Err(reason) => return Ok(Err(reason)),
    };
    if events.is_empty() {
        return Ok(Ok(Vec::new()));
    }

    let _refresh = catalog.refresh.lock();
    catalog
        .refresh_all_locked(&home, true)
        .map_err(|_| anyhow::anyhow!("task catalog is unreadable"))?;
    let mut inner = catalog
        .inner
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if inner.phase != Phase::Ready {
        anyhow::bail!("task catalog is not ready");
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
    next.set_cursor(BoardCursor::from_hot_log_identity(
        stamp.inode,
        stamp.len,
        stamp.mtime_ns,
    ));
    inner.boards.insert(board_id.clone(), next);
    rebuild_routes(&mut inner);

    super::invalidate_replay_cache();
    drop(inner);
    drop(file_lock);
    drop(_refresh);

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
            next.set_cursor(BoardCursor::from_hot_log_identity(
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

fn catch_up_projection_locked(
    projection: &mut BoardProjection,
    board: &Path,
) -> Result<bool, String> {
    let observed = hot_log_stamp(board)?;
    let cursor = *projection
        .cursor()
        .ok_or_else(|| "rebuilt projection has no cursor".to_string())?;
    match cursor.classify_observed(observed.inode, observed.len, observed.mtime_ns) {
        HotLogFreshness::Current => Ok(true),
        HotLogFreshness::Stale => Ok(false),
        HotLogFreshness::CatchUp { start, end } => {
            let (envelopes, consumed) = read_tail(board, start, end)?;
            if consumed != end - start {
                return Err("hot log ended with an incomplete record during rebuild".to_string());
            }
            projection
                .apply_ordered_batch(&envelopes)
                .map_err(|error| format!("rebuild catch-up is not canonical: {error:?}"))?;
            projection.set_cursor(BoardCursor::from_hot_log_identity(
                observed.inode,
                end,
                observed.mtime_ns,
            ));
            Ok(true)
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
    #[cfg(test)]
    ARCHIVE_BYTES_READ.with(|count| count.set(count.get() + bytes.len() as u64));
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

pub(super) fn compact_at_with_keep(board: &Path, keep: usize) -> anyhow::Result<()> {
    if !super::log_path(board).exists() {
        return Ok(());
    }
    let (home, board_id) = board_identity(board)?;
    let catalog = for_home(&home);
    catalog
        .refresh_all(&home, true)
        .map_err(|_| anyhow::anyhow!("task catalog is unreadable"))?;
    let suffix = chrono::Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string();
    crate::event_log::append_lines_under_lock(board, super::LOG_NAME, |log_path| {
        let mut inner = catalog
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        catch_up_board_locked(&mut inner, &board_id, board)?;
        let content = std::fs::read_to_string(log_path)?;
        let lines: Vec<&str> = content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if lines.len() <= keep {
            return Ok(Vec::new());
        }
        let split = lines.len() - keep;
        let archived: String = lines[..split]
            .iter()
            .map(|line| format!("{line}\n"))
            .collect();
        let kept: String = lines[split..]
            .iter()
            .map(|line| format!("{line}\n"))
            .collect();
        let archive = super::archive_dir(
            log_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("log_path has no parent"))?,
        );
        std::fs::create_dir_all(&archive)?;
        crate::store::atomic_write(
            &archive.join(format!("task_events.{suffix}.jsonl")),
            archived.as_bytes(),
        )?;
        crate::store::atomic_write(log_path, kept.as_bytes())?;
        refresh_archive_manifest(board)?;
        super::invalidate_replay_cache();
        let stamp = hot_log_stamp(board).map_err(anyhow::Error::msg)?;
        let mut projection = inner
            .boards
            .get(&board_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("catalog board disappeared during compaction"))?;
        projection.set_cursor(BoardCursor::from_hot_log_identity(
            stamp.inode,
            stamp.len,
            stamp.mtime_ns,
        ));
        inner.boards.insert(board_id.clone(), projection);
        Ok(Vec::new())
    })?;
    let projection = catalog
        .inner
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .boards
        .get(&board_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("catalog board disappeared after compaction"))?;
    write_checkpoint(board, &board_id, &projection).map_err(anyhow::Error::msg)
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
        || (cursor.inode != [0; 24] && observed.inode != cursor.inode)
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
        projection.set_cursor(BoardCursor::from_hot_log_identity(
            observed.inode,
            checkpoint.cursor.live_offset + consumed,
            observed.mtime_ns,
        ));
    } else {
        projection.set_cursor(BoardCursor::from_hot_log_identity(
            observed.inode,
            observed.len,
            observed.mtime_ns,
        ));
    }
    if hot_log_stamp(board)? != observed {
        return Err("hot log changed while loading checkpoint".to_string());
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
        if let Err(err) = std::fs::create_dir_all(&path) {
            return StrictTaskCatalog::with_home(
                Some(home.to_path_buf()),
                Phase::Unhealthy {
                    since: chrono::Utc::now().to_rfc3339(),
                    causes: vec![format!("{name}: create {}: {err}", path.display())],
                },
                boards,
            );
        }
        let lock_path = super::log_path(&path).with_extension("jsonl.lock");
        let _lock = match crate::store::acquire_file_lock(&lock_path) {
            Ok(lock) => lock,
            Err(err) => {
                return StrictTaskCatalog::with_home(
                    Some(home.to_path_buf()),
                    Phase::Unhealthy {
                        since: chrono::Utc::now().to_rfc3339(),
                        causes: vec![format!("{name}: lock {}: {err}", lock_path.display())],
                    },
                    boards,
                );
            }
        };
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
    let before = hot_log_stamp(board)?;
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
    if stamp != before {
        return Err("hot log changed while rebuilding projection".to_string());
    }
    projection.set_cursor(BoardCursor::from_hot_log_identity(
        stamp.inode,
        stamp.len,
        stamp.mtime_ns,
    ));
    Ok(projection)
}

fn load_board_projection(board: &Path, board_id: &str) -> Result<BoardProjection, String> {
    match load_manifest(board)? {
        Some(manifest) => {
            if let Ok(Some(projection)) = load_checkpoint(board, board_id) {
                return Ok(projection);
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
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HotLogStamp {
                inode: [0; 24],
                len: 0,
                mtime_ns: 0,
            });
        }
        Err(err) => return Err(format!("open {}: {err}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|err| format!("stat {}: {err}", path.display()))?;
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
        .unwrap_or(0);
    Ok(HotLogStamp {
        inode: file_identity(&file, &metadata)?,
        len: metadata.len(),
        mtime_ns,
    })
}

#[cfg(unix)]
fn file_identity(_file: &std::fs::File, metadata: &std::fs::Metadata) -> Result<[u8; 24], String> {
    use std::os::unix::fs::MetadataExt;
    Ok(identity_from_u64(metadata.ino()))
}

#[cfg(windows)]
fn file_identity(file: &std::fs::File, _metadata: &std::fs::Metadata) -> Result<[u8; 24], String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut info = FILE_ID_INFO::default();
    // SAFETY: `file` owns a live handle and `info` is a correctly sized writable buffer.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(format!(
            "read Windows file identity: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut identity = [0_u8; 24];
    identity[..8].copy_from_slice(&info.VolumeSerialNumber.to_le_bytes());
    identity[8..].copy_from_slice(&info.FileId.Identifier);
    Ok(identity)
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &std::fs::File, metadata: &std::fs::Metadata) -> Result<[u8; 24], String> {
    Ok(identity_from_u64(
        metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0),
    ))
}

#[cfg(any(not(windows), test))]
fn identity_from_u64(value: u64) -> [u8; 24] {
    let mut identity = [0_u8; 24];
    identity[..8].copy_from_slice(&value.to_le_bytes());
    identity
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
    #[cfg(test)]
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

    #[cfg(test)]
    pub fn task(&self, id: &TaskId) -> Option<&ProjectedTaskRecord> {
        self.tasks.get(id).map(Arc::as_ref)
    }

    #[cfg(test)]
    pub fn tasks(&self) -> impl Iterator<Item = &ProjectedTaskRecord> {
        self.tasks.values().map(Arc::as_ref)
    }

    pub fn task_snapshot(&self, id: &TaskId) -> Option<Arc<ProjectedTaskRecord>> {
        self.tasks.get(id).cloned()
    }

    pub fn task_snapshots(&self) -> Vec<Arc<ProjectedTaskRecord>> {
        self.tasks.values().cloned().collect()
    }

    /// Bounded current-state view for commit preconditions. Production
    /// preconditions inspect task fields, not the unbounded audit timeline;
    /// retain only the catalog's recent-history ring on this compatibility
    /// surface so a write never replays history while holding the board lock.
    fn current_state(&self) -> TaskBoardState {
        TaskBoardState {
            tasks: self
                .tasks
                .iter()
                .map(|(id, task)| (id.clone(), task.current_task_record()))
                .collect(),
            last_seq_per_instance: self.last_seq_per_instance.clone(),
            events_folded: self.events_folded,
        }
    }

    #[cfg(test)]
    pub fn last_seq_for(&self, instance: &InstanceName) -> Option<u64> {
        self.last_seq_per_instance.get(instance).copied()
    }

    #[cfg(test)]
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

    fn matches_current_replay(&self, replay: &TaskBoardState) -> Option<bool> {
        if self.events_folded > replay.events_folded {
            None
        } else {
            Some(self.matches_replay(replay))
        }
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
    #[cfg(test)]
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
#[path = "catalog/tests.rs"]
mod tests;
