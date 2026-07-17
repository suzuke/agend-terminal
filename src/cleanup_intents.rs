use std::path::{Path, PathBuf};

fn intents_dir(home: &Path) -> PathBuf {
    home.join("cleanup-intents")
}

fn intent_key(repo: &str, branch: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut h);
    branch.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CleanupIntent {
    pub repo: String,
    pub branch: String,
    pub expected_head: String,
    pub task_id: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scm_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
}

pub(crate) fn persist_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    expected_head: &str,
    task_id: &str,
    scm_slug: Option<&str>,
    pr_number: Option<u64>,
) -> Result<(), String> {
    let dir = intents_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create cleanup-intents dir: {e}"))?;
    let intent = CleanupIntent {
        repo: repo.to_string(),
        branch: branch.to_string(),
        expected_head: expected_head.to_string(),
        task_id: task_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        scm_slug: scm_slug.map(String::from),
        pr_number,
    };
    let key = intent_key(repo, branch);
    let path = dir.join(format!("{key}.json"));
    let body = serde_json::to_string_pretty(&intent)
        .map_err(|e| format!("serialize cleanup intent: {e}"))?;
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("write cleanup intent: {e}"))
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum SettleOutcome {
    Deleted,
    Pending(String),
    HeadDrift(String),
    BranchAbsent,
    GitError(String),
    DeleteFailed(String),
}

/// Settle a cleanup intent: delete the branch with expected-head CAS.
///
/// `merged` is the authoritative terminal signal — only `true` authorizes
/// deletion. `pr_number` provides generation identity — when present on
/// both the event and the intent, they must match to prevent a stale
/// merged event from settling a newer intent.
///
/// Intent file is removed ONLY after successful delete or confirmed branch
/// absence. Git/I/O errors preserve the intent for retry.
pub(crate) fn settle_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    merged: bool,
    event_pr_number: Option<u64>,
) -> Option<SettleOutcome> {
    let key = intent_key(repo, branch);
    let path = intents_dir(home).join(format!("{key}.json"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return None;
    };
    let Ok(intent) = serde_json::from_str::<CleanupIntent>(&content) else {
        return None;
    };

    if intent.repo != repo || intent.branch != branch {
        return None;
    }

    if !merged {
        return Some(SettleOutcome::Pending(format!(
            "cleanup intent for '{branch}': not yet merged — intent preserved"
        )));
    }

    // Generation identity: intent with pr_number requires a matching event
    // pr_number. Missing event generation when intent has one → fail-closed.
    match (intent.pr_number, event_pr_number) {
        (Some(intent_pr), Some(event_pr)) if intent_pr != event_pr => {
            return Some(SettleOutcome::Pending(format!(
                "cleanup intent for '{branch}': PR generation mismatch \
                 (intent=#{intent_pr}, event=#{event_pr}) — intent preserved"
            )));
        }
        (Some(intent_pr), None) => {
            return Some(SettleOutcome::Pending(format!(
                "cleanup intent for '{branch}': intent carries PR #{intent_pr} \
                 but terminal event has no generation — fail-closed"
            )));
        }
        _ => {}
    }

    let source_repo = Path::new(&intent.repo);
    if !source_repo.is_dir() {
        return Some(SettleOutcome::GitError(format!(
            "cleanup intent for '{branch}': source repo '{}' not a directory — intent preserved",
            intent.repo
        )));
    }
    // Typed branch-existence check: show-ref --verify --quiet exit codes:
    // 0 = present, 1 = absent (confirmed), any other = git error (preserve).
    let full_ref = format!("refs/heads/{branch}");
    match crate::git_helpers::git_bypass(
        source_repo,
        &["show-ref", "--verify", "--quiet", &full_ref],
    ) {
        Err(e) => {
            return Some(SettleOutcome::GitError(format!(
                "cleanup intent for '{branch}': git spawn error ({e}) — intent preserved for retry"
            )));
        }
        Ok(o) => match o.status.code() {
            Some(0) => {}
            Some(1) => {
                let _ = std::fs::remove_file(&path);
                return Some(SettleOutcome::BranchAbsent);
            }
            other => {
                return Some(SettleOutcome::GitError(format!(
                    "cleanup intent for '{branch}': show-ref exit {} — intent preserved for retry",
                    other.map_or("signal".to_string(), |c| c.to_string())
                )));
            }
        },
    }
    // Branch exists — get tip SHA for CAS.
    let tip = match crate::git_helpers::git_cmd(source_repo, &["rev-parse", branch]) {
        Ok(t) => t,
        Err(e) => {
            return Some(SettleOutcome::GitError(format!(
                "cleanup intent for '{branch}': rev-parse error ({e}) — intent preserved for retry"
            )));
        }
    };
    if tip.trim() != intent.expected_head {
        return Some(SettleOutcome::HeadDrift(format!(
            "cleanup intent for '{branch}': expected head {} but actual tip is {} \
             — preserved (fail-closed)",
            intent.expected_head,
            tip.trim()
        )));
    }
    let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
    match del {
        Ok(o) if o.status.success() => {
            let _ = std::fs::remove_file(&path);
            Some(SettleOutcome::Deleted)
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Some(SettleOutcome::DeleteFailed(format!(
                "git branch -D failed: {stderr}"
            )))
        }
        Err(e) => Some(SettleOutcome::DeleteFailed(format!(
            "git branch -D failed: {e}"
        ))),
    }
}

/// Settle intents matching an SCM slug. The CI poller knows the slug but not
/// the local canonical path; this scans all intents for a slug match.
pub(crate) fn settle_by_scm_slug(
    home: &Path,
    scm_slug: &str,
    branch: &str,
    merged: bool,
    event_pr_number: Option<u64>,
) -> Option<SettleOutcome> {
    let dir = intents_dir(home);
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if intent.branch == branch && intent.scm_slug.as_deref().is_some_and(|s| s == scm_slug) {
            return settle_intent(home, &intent.repo, branch, merged, event_pr_number);
        }
    }
    None
}

/// Durable retry consumer: scan all intents, check merge status via
/// `pr_merge_status` for each, and settle those confirmed merged. Called
/// from the periodic sweep to ensure intents are settled even if the
/// original CI watch was removed before settlement succeeded.
pub(crate) fn sweep_settle_merged(home: &Path) {
    let dir = intents_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let repo_path = Path::new(&intent.repo);
        if !repo_path.is_dir() {
            continue;
        }
        let default = crate::git_helpers::default_branch(repo_path);
        // Independently observe the merged PR number — never echo the
        // intent's own pr_number as the event generation.
        let observed_pr =
            crate::branch_sweep::merged_pr_number(repo_path, &default, &intent.branch);
        let Some(pr_num) = observed_pr else {
            continue;
        };
        match settle_intent(home, &intent.repo, &intent.branch, true, Some(pr_num)) {
            Some(SettleOutcome::Deleted) => {
                tracing::info!(
                    branch = %intent.branch,
                    "cleanup intent settled by sweep retry"
                );
            }
            Some(SettleOutcome::DeleteFailed(reason)) | Some(SettleOutcome::GitError(reason)) => {
                tracing::warn!(
                    branch = %intent.branch, %reason,
                    "cleanup intent sweep retry failed — will retry next tick"
                );
            }
            _ => {}
        }
    }
}

pub(crate) fn intent_repos(home: &Path) -> Vec<String> {
    let dir = intents_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut repos = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(intent) = serde_json::from_str::<CleanupIntent>(&content) {
                repos.insert(intent.repo);
            }
        }
    }
    repos.into_iter().collect()
}

#[allow(dead_code)]
pub(crate) fn has_intent(home: &Path, repo: &str, branch: &str) -> bool {
    let key = intent_key(repo, branch);
    intents_dir(home).join(format!("{key}.json")).exists()
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ReconcileCandidate {
    pub candidate_id: String,
    pub branch: String,
    pub expected_head: String,
    pub task_id: String,
    pub outcome: String,
}

#[derive(Debug)]
pub(crate) struct ReconcileResult {
    pub candidates: Vec<ReconcileCandidate>,
    pub settled: usize,
    pub preserved: usize,
}

fn is_branch_checked_out(repo: &Path, branch: &str) -> bool {
    let output = match crate::git_helpers::git_bypass(repo, &["worktree", "list", "--porcelain"]) {
        Ok(o) if o.status.success() => o,
        _ => return true, // unknown → preserve (fail-closed)
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if b == branch {
                return true;
            }
        }
    }
    false
}

pub(crate) fn reconcile_terminal_review_intents(home: &Path, dry_run: bool) -> ReconcileResult {
    let dir = intents_dir(home);
    let mut result = ReconcileResult {
        candidates: Vec::new(),
        settled: 0,
        preserved: 0,
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if !intent.branch.starts_with("review/") {
            continue;
        }
        let cid = intent_key(&intent.repo, &intent.branch);

        if intent.task_id.is_empty() {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:no_task_id".into(),
            });
            continue;
        }

        let task_terminal = match crate::tasks::load_routed(home, &intent.task_id) {
            Ok(rt) => matches!(
                rt.record().status,
                crate::task_events::TaskStatus::Done | crate::task_events::TaskStatus::Verified
            ),
            Err(_) => false,
        };
        if !task_terminal {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:task_not_terminal".into(),
            });
            continue;
        }

        let repo_path = Path::new(&intent.repo);
        if !repo_path.is_dir() {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:repo_missing".into(),
            });
            continue;
        }

        if crate::worktree_cleanup::branch_has_other_active_binding(
            home,
            repo_path,
            &intent.branch,
            None,
        )
        .unwrap_or(true)
        {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:active_binding".into(),
            });
            continue;
        }

        // Typed branch-existence check (show-ref exit 0/1/other).
        let full_ref = format!("refs/heads/{}", intent.branch);
        match crate::git_helpers::git_bypass(
            repo_path,
            &["show-ref", "--verify", "--quiet", &full_ref],
        ) {
            Err(e) => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: format!("preserved:git_spawn_error:{e}"),
                });
                continue;
            }
            Ok(o) => match o.status.code() {
                Some(0) => {} // branch exists
                Some(1) => {
                    // branch confirmed absent — remove intent
                    if !dry_run {
                        let _ = std::fs::remove_file(&p);
                    }
                    result.settled += 1;
                    result.candidates.push(ReconcileCandidate {
                        candidate_id: cid,
                        branch: intent.branch.clone(),
                        expected_head: intent.expected_head.clone(),
                        task_id: intent.task_id.clone(),
                        outcome: "settled:branch_absent".into(),
                    });
                    continue;
                }
                other => {
                    result.preserved += 1;
                    result.candidates.push(ReconcileCandidate {
                        candidate_id: cid,
                        branch: intent.branch.clone(),
                        expected_head: intent.expected_head.clone(),
                        task_id: intent.task_id.clone(),
                        outcome: format!(
                            "preserved:git_show_ref_exit:{}",
                            other.map_or("signal".into(), |c| c.to_string())
                        ),
                    });
                    continue;
                }
            },
        }

        // Acquire branch lease lock — shared classifier for both dry-run and apply.
        let _branch_lock =
            match crate::binding::acquire_branch_lease_lock(home, &intent.repo, &intent.branch) {
                Ok(lock) => lock,
                Err(e) => {
                    result.preserved += 1;
                    result.candidates.push(ReconcileCandidate {
                        candidate_id: cid,
                        branch: intent.branch.clone(),
                        expected_head: intent.expected_head.clone(),
                        task_id: intent.task_id.clone(),
                        outcome: format!("preserved:lock_failed:{e}"),
                    });
                    continue;
                }
            };

        // Under lock: re-verify all gates.
        // Re-read intent from disk (concurrent modification).
        let re_content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:intent_gone_under_lock".into(),
                });
                continue;
            }
        };
        let re_intent: CleanupIntent = match serde_json::from_str::<CleanupIntent>(&re_content) {
            Ok(i) if i.branch == intent.branch && i.expected_head == intent.expected_head => i,
            _ => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:intent_changed_under_lock".into(),
                });
                continue;
            }
        };

        // Re-verify task terminal under lock.
        let still_terminal = crate::tasks::load_routed(home, &re_intent.task_id)
            .map(|rt| {
                matches!(
                    rt.record().status,
                    crate::task_events::TaskStatus::Done | crate::task_events::TaskStatus::Verified
                )
            })
            .unwrap_or(false);
        if !still_terminal {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:task_no_longer_terminal".into(),
            });
            continue;
        }

        // Re-verify no binding under lock.
        if crate::worktree_cleanup::branch_has_other_active_binding(
            home,
            repo_path,
            &intent.branch,
            None,
        )
        .unwrap_or(true)
        {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:binding_appeared_under_lock".into(),
            });
            continue;
        }

        // Re-verify exact head under lock via typed show-ref + rev-parse.
        let tip = match crate::git_helpers::git_cmd(repo_path, &["rev-parse", &intent.branch]) {
            Ok(t) if !t.is_empty() => t.trim().to_string(),
            _ => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:head_unresolvable_under_lock".into(),
                });
                continue;
            }
        };
        if tip != intent.expected_head {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: format!("preserved:head_drift:{tip}"),
            });
            continue;
        }

        // Worktree occupancy gate: refuse if the branch is checked out.
        if is_branch_checked_out(repo_path, &intent.branch) {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:checked_out".into(),
            });
            continue;
        }

        // All gates passed under lock. Dry-run stops here.
        if dry_run {
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "qualified:dry_run".into(),
            });
            continue;
        }

        // Create collision-safe recovery ref before delete.
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let short_head = if tip.len() >= 8 { &tip[..8] } else { &tip };
        let recovery_ref = format!(
            "refs/agend/recovery/{}/{}-{}",
            intent.branch, short_head, epoch
        );
        // Check for existing recovery at this exact ref (don't overwrite).
        if crate::git_helpers::git_ok(
            repo_path,
            &["show-ref", "--verify", "--quiet", &recovery_ref],
        ) {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:recovery_ref_collision".into(),
            });
            continue;
        }
        if crate::git_helpers::git_cmd(repo_path, &["update-ref", &recovery_ref, &tip]).is_err() {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:recovery_ref_create_failed".into(),
            });
            continue;
        }
        // Verify recovery ref points to exact tip.
        let ref_verified = crate::git_helpers::git_cmd(repo_path, &["rev-parse", &recovery_ref])
            .map(|s| s.trim().to_string())
            .is_ok_and(|s| s == tip);
        if !ref_verified {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:recovery_ref_verify_failed".into(),
            });
            continue;
        }

        // Atomic CAS delete: update-ref -d with expected old SHA.
        let full_ref = format!("refs/heads/{}", intent.branch);
        match crate::git_helpers::git_bypass(
            repo_path,
            &["update-ref", "-d", &full_ref, &intent.expected_head],
        ) {
            Ok(o) if o.status.success() => {
                let _ = std::fs::remove_file(&p);
                result.settled += 1;
                crate::event_log::log(
                    home,
                    "review_branch_reconciled",
                    &intent.task_id,
                    &format!(
                        "candidate_id={} branch={} head={} recovery_ref={}",
                        cid, intent.branch, tip, recovery_ref
                    ),
                );
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: format!("settled:deleted:recovery={recovery_ref}"),
                });
            }
            _ => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:delete_failed".into(),
                });
            }
        }
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-intent-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn tmp_repo(tag: &str) -> PathBuf {
        let dir = tmp_dir(tag);
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        dir
    }

    fn git_in(wt: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn branch_tip(repo: &Path, branch: &str) -> String {
        crate::git_helpers::git_cmd(repo, &["rev-parse", branch])
            .expect("rev-parse")
            .trim()
            .to_string()
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        crate::git_helpers::git_ok(repo, &["rev-parse", "--verify", branch])
    }

    fn make_branch(repo: &Path, name: &str) -> String {
        git_in(repo, &["checkout", "-b", name]);
        std::fs::write(repo.join("f.txt"), "content").ok();
        git_in(repo, &["add", "."]);
        git_in(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "work",
            ],
        );
        let tip = branch_tip(repo, name);
        git_in(repo, &["checkout", "main"]);
        tip
    }

    #[test]
    fn settle_merged_exact_head_deletes_branch() {
        let home = tmp_dir("settle-ok");
        let repo = tmp_repo("settle-ok-repo");
        let tip = make_branch(&repo, "feat/settle-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/settle-test",
            &tip,
            "t-123",
            None,
            Some(42),
        )
        .expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/settle-test", true, Some(42));
        assert!(matches!(outcome, Some(SettleOutcome::Deleted)));
        assert!(!branch_exists(&repo, "feat/settle-test"));
        assert!(!has_intent(&home, &rs, "feat/settle-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_not_merged_preserves_branch_and_intent() {
        let home = tmp_dir("settle-pending");
        let repo = tmp_repo("settle-pending-repo");
        let tip = make_branch(&repo, "feat/pending-test");
        let rs = repo.display().to_string();
        persist_intent(&home, &rs, "feat/pending-test", &tip, "t-456", None, None)
            .expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/pending-test", false, None);
        assert!(matches!(outcome, Some(SettleOutcome::Pending(_))));
        assert!(branch_exists(&repo, "feat/pending-test"));
        assert!(has_intent(&home, &rs, "feat/pending-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn settle_head_drift_preserves_branch_and_intent() {
        let home = tmp_dir("settle-drift");
        let repo = tmp_repo("settle-drift-repo");
        let _tip = make_branch(&repo, "feat/drift-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/drift-test",
            "0000000000000000000000000000000000000000",
            "t-789",
            None,
            None,
        )
        .expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/drift-test", true, None);
        assert!(matches!(outcome, Some(SettleOutcome::HeadDrift(_))));
        assert!(branch_exists(&repo, "feat/drift-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn restart_preserves_then_merged_settles() {
        let home = tmp_dir("restart-settle");
        let repo = tmp_repo("restart-settle-repo");
        let tip = make_branch(&repo, "feat/restart-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/restart-test",
            &tip,
            "t-restart",
            None,
            Some(10),
        )
        .expect("persist");

        // Restart: merged=false → preserved
        let outcome = settle_intent(&home, &rs, "feat/restart-test", false, None);
        assert!(matches!(outcome, Some(SettleOutcome::Pending(_))));
        assert!(branch_exists(&repo, "feat/restart-test"));
        assert!(has_intent(&home, &rs, "feat/restart-test"));

        // PR merges with matching generation
        let outcome = settle_intent(&home, &rs, "feat/restart-test", true, Some(10));
        assert!(matches!(outcome, Some(SettleOutcome::Deleted)));
        assert!(!branch_exists(&repo, "feat/restart-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn old_generation_event_does_not_settle_newer_intent() {
        let home = tmp_dir("gen-mismatch");
        let repo = tmp_repo("gen-mismatch-repo");
        let tip = make_branch(&repo, "feat/gen-test");
        let rs = repo.display().to_string();
        // Intent for PR #20 (newer)
        persist_intent(&home, &rs, "feat/gen-test", &tip, "t-gen", None, Some(20))
            .expect("persist");

        // Old PR #10 merged event → generation mismatch → preserved
        let outcome = settle_intent(&home, &rs, "feat/gen-test", true, Some(10));
        assert!(matches!(outcome, Some(SettleOutcome::Pending(_))));
        assert!(branch_exists(&repo, "feat/gen-test"));
        assert!(has_intent(&home, &rs, "feat/gen-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn transient_git_error_preserves_intent_for_retry() {
        let home = tmp_dir("git-error");
        // Non-existent repo path → git error, not branch absence
        persist_intent(
            &home,
            "/nonexistent/repo/path",
            "feat/error-test",
            "abc123",
            "t-err",
            None,
            None,
        )
        .expect("persist");

        let outcome = settle_intent(
            &home,
            "/nonexistent/repo/path",
            "feat/error-test",
            true,
            None,
        );
        assert!(
            matches!(outcome, Some(SettleOutcome::GitError(_))),
            "git error must NOT clear intent: {outcome:?}"
        );
        assert!(
            has_intent(&home, "/nonexistent/repo/path", "feat/error-test"),
            "intent must survive git error"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn real_missing_branch_clears_intent() {
        let home = tmp_dir("missing-branch");
        let repo = tmp_repo("missing-branch-repo");
        let rs = repo.display().to_string();
        // Intent for a branch that never existed in this repo
        persist_intent(
            &home,
            &rs,
            "feat/definitely-missing-branch",
            "abc123",
            "t-miss",
            None,
            None,
        )
        .expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/definitely-missing-branch", true, None);
        assert!(
            matches!(outcome, Some(SettleOutcome::BranchAbsent)),
            "real missing branch must be BranchAbsent (not GitError): {outcome:?}"
        );
        assert!(
            !has_intent(&home, &rs, "feat/definitely-missing-branch"),
            "intent must be cleared for confirmed absent branch"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn non_git_directory_preserves_intent_as_git_error() {
        let home = tmp_dir("non-git-dir");
        let non_git = tmp_dir("plain-dir-not-git");
        let rs = non_git.display().to_string();
        persist_intent(&home, &rs, "feat/x", "abc", "t-1", None, None).expect("persist");

        let outcome = settle_intent(&home, &rs, "feat/x", true, None);
        assert!(
            matches!(outcome, Some(SettleOutcome::GitError(_))),
            "non-git directory must be GitError (not BranchAbsent): {outcome:?}"
        );
        assert!(has_intent(&home, &rs, "feat/x"), "intent must survive");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&non_git).ok();
    }

    #[test]
    fn missing_event_generation_fails_closed_when_intent_has_one() {
        let home = tmp_dir("gen-failclosed");
        let repo = tmp_repo("gen-failclosed-repo");
        let tip = make_branch(&repo, "feat/gen-fc");
        let rs = repo.display().to_string();
        persist_intent(&home, &rs, "feat/gen-fc", &tip, "t-fc", None, Some(42)).expect("persist");

        // Merged event with NO pr_number → intent has #42 → fail-closed
        let outcome = settle_intent(&home, &rs, "feat/gen-fc", true, None);
        assert!(
            matches!(outcome, Some(SettleOutcome::Pending(_))),
            "missing event generation must fail-closed: {outcome:?}"
        );
        assert!(branch_exists(&repo, "feat/gen-fc"), "branch must survive");
        assert!(has_intent(&home, &rs, "feat/gen-fc"), "intent must survive");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn persist_failure_is_surfaced() {
        // Persist to an unwritable path
        let result = persist_intent(
            Path::new(if cfg!(windows) {
                r"\\?\Z:\nonexistent\root\agend-test"
            } else {
                "/nonexistent/root/agend-test"
            }),
            "/repo",
            "feat/x",
            "abc",
            "t-1",
            None,
            None,
        );
        assert!(result.is_err(), "persist to unwritable path must fail");
    }

    #[test]
    fn settle_by_slug_matches_scm_identity() {
        let home = tmp_dir("slug-settle");
        let repo = tmp_repo("slug-settle-repo");
        let tip = make_branch(&repo, "feat/slug-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "feat/slug-test",
            &tip,
            "t-slug",
            Some("owner/repo"),
            Some(5),
        )
        .expect("persist");

        let outcome = settle_by_scm_slug(&home, "owner/repo", "feat/slug-test", true, Some(5));
        assert!(matches!(outcome, Some(SettleOutcome::Deleted)));
        assert!(!branch_exists(&repo, "feat/slug-test"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn intent_repos_contributes_to_discovery() {
        let home = tmp_dir("intent-repos");
        persist_intent(
            &home,
            "/path/to/repo-a",
            "feat/a",
            "abc123",
            "t-1",
            None,
            None,
        )
        .expect("persist");
        persist_intent(
            &home,
            "/path/to/repo-b",
            "feat/b",
            "def456",
            "t-2",
            None,
            None,
        )
        .expect("persist");
        persist_intent(
            &home,
            "/path/to/repo-a",
            "feat/c",
            "ghi789",
            "t-3",
            None,
            None,
        )
        .expect("persist");

        let repos = intent_repos(&home);
        assert!(repos.contains(&"/path/to/repo-a".to_string()));
        assert!(repos.contains(&"/path/to/repo-b".to_string()));
        assert_eq!(repos.len(), 2, "deduplication: {repos:?}");

        std::fs::remove_dir_all(&home).ok();
    }

    fn seed_terminal_task(home: &Path, task_id: &str) {
        use crate::task_events::{InstanceName, TaskEvent, TaskId};
        let tid = TaskId(task_id.to_string());
        let board = crate::task_events::board_root(home, crate::task_events::DEFAULT_PROJECT);
        std::fs::create_dir_all(&board).ok();
        let emitter = InstanceName::from("test");
        let _ = crate::task_events::append(
            &board,
            &emitter,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "test".into(),
                description: String::new(),
                priority: "normal".into(),
                tags: Vec::new(),
                owner: None,
                depends_on: Vec::new(),
                parent_id: None,
                branch: None,
                due_at: None,
                routed_to: None,
                bind: None,
                eta_secs: None,
            },
        );
        let _ = crate::task_events::append(
            &board,
            &emitter,
            TaskEvent::Done {
                task_id: tid,
                by: emitter.clone(),
                source: crate::task_events::DoneSource::ReportAutoClose {
                    report_summary: "test done".into(),
                    closed_at: chrono::Utc::now().to_rfc3339(),
                },
            },
        );
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_terminal_review_intent_deletes_branch() {
        let home = tmp_dir("reconcile-ok");
        let repo = tmp_repo("reconcile-ok-repo");
        let tip = make_branch(&repo, "review/pr-999-test");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "review/pr-999-test",
            &tip,
            "t-reconcile-ok",
            None,
            None,
        )
        .expect("persist");
        seed_terminal_task(&home, "t-reconcile-ok");

        let result = reconcile_terminal_review_intents(&home, false);

        assert_eq!(
            result.settled, 1,
            "terminal intent must be settled: {result:?}"
        );
        assert!(
            !branch_exists(&repo, "review/pr-999-test"),
            "review branch must be deleted"
        );
        assert!(
            !has_intent(&home, &rs, "review/pr-999-test"),
            "intent file must be removed"
        );
        // Recovery ref must exist.
        let refs_out = crate::git_helpers::git_cmd(
            &repo,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/agend/recovery/review/pr-999-test/",
            ],
        )
        .unwrap_or_default();
        assert!(
            refs_out.lines().any(|l| l.trim() == tip),
            "recovery ref must point to deleted tip"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_preserves_non_terminal_task() {
        let home = tmp_dir("reconcile-nt");
        let repo = tmp_repo("reconcile-nt-repo");
        let tip = make_branch(&repo, "review/pr-888-keep");
        let rs = repo.display().to_string();
        persist_intent(&home, &rs, "review/pr-888-keep", &tip, "t-nt", None, None)
            .expect("persist");

        let result = reconcile_terminal_review_intents(&home, false);

        assert_eq!(
            result.preserved, 1,
            "non-terminal must be preserved: {result:?}"
        );
        assert!(branch_exists(&repo, "review/pr-888-keep"));
        assert!(has_intent(&home, &rs, "review/pr-888-keep"));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_preserves_head_drift() {
        let home = tmp_dir("reconcile-drift");
        let repo = tmp_repo("reconcile-drift-repo");
        let _tip = make_branch(&repo, "review/pr-777-drift");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "review/pr-777-drift",
            "deadbeef00",
            "t-drift",
            None,
            None,
        )
        .expect("persist");
        seed_terminal_task(&home, "t-drift");

        let result = reconcile_terminal_review_intents(&home, false);

        assert_eq!(result.preserved, 1, "drift must preserve: {result:?}");
        assert!(branch_exists(&repo, "review/pr-777-drift"));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_idempotent_on_replay() {
        let home = tmp_dir("reconcile-idem");
        let repo = tmp_repo("reconcile-idem-repo");
        let tip = make_branch(&repo, "review/pr-555-idem");
        let rs = repo.display().to_string();
        persist_intent(&home, &rs, "review/pr-555-idem", &tip, "t-idem", None, None)
            .expect("persist");
        seed_terminal_task(&home, "t-idem");

        let r1 = reconcile_terminal_review_intents(&home, false);
        assert_eq!(r1.settled, 1);

        let r2 = reconcile_terminal_review_intents(&home, false);
        assert_eq!(r2.candidates.len(), 0, "no candidates on replay");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn reconcile_skips_non_review_branches() {
        let home = tmp_dir("reconcile-nonreview");
        persist_intent(
            &home,
            "/tmp/fake",
            "feat/not-review",
            "abc",
            "t-x",
            None,
            None,
        )
        .expect("persist");

        let result = reconcile_terminal_review_intents(&home, false);
        assert!(result.candidates.is_empty(), "non-review skipped");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_dry_run_does_not_mutate() {
        let home = tmp_dir("reconcile-dryrun");
        let repo = tmp_repo("reconcile-dryrun-repo");
        let tip = make_branch(&repo, "review/pr-444-dry");
        let rs = repo.display().to_string();
        persist_intent(&home, &rs, "review/pr-444-dry", &tip, "t-dry", None, None)
            .expect("persist");
        seed_terminal_task(&home, "t-dry");

        let result = reconcile_terminal_review_intents(&home, true);

        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.outcome.contains("dry_run")),
            "dry-run must produce qualified candidate: {result:?}"
        );
        assert!(
            branch_exists(&repo, "review/pr-444-dry"),
            "branch must survive dry-run"
        );
        assert!(
            has_intent(&home, &rs, "review/pr-444-dry"),
            "intent must survive dry-run"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn reconcile_git_error_preserves_intent() {
        let home = tmp_dir("reconcile-giterr");
        let non_git = tmp_dir("reconcile-notgit");
        let rs = non_git.display().to_string();
        persist_intent(&home, &rs, "review/pr-333-err", "abc", "t-err", None, None)
            .expect("persist");
        seed_terminal_task(&home, "t-err");

        let result = reconcile_terminal_review_intents(&home, false);

        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.outcome.contains("preserved:git")),
            "git error must preserve: {result:?}"
        );
        assert!(
            has_intent(&home, &rs, "review/pr-333-err"),
            "intent must survive git error"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&non_git).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_candidate_id_is_stable() {
        let home = tmp_dir("reconcile-candid");
        let repo = tmp_repo("reconcile-candid-repo");
        let tip = make_branch(&repo, "review/pr-222-cid");
        let rs = repo.display().to_string();
        persist_intent(&home, &rs, "review/pr-222-cid", &tip, "t-cid", None, None)
            .expect("persist");
        seed_terminal_task(&home, "t-cid");

        let expected_cid = intent_key(&rs, "review/pr-222-cid");
        let result = reconcile_terminal_review_intents(&home, false);

        assert!(
            result
                .candidates
                .iter()
                .any(|c| c.candidate_id == expected_cid),
            "candidate_id must be stable derived from intent identity: {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_dry_run_head_drift_preserved() {
        let home = tmp_dir("dryrun-drift");
        let repo = tmp_repo("dryrun-drift-repo");
        let _tip = make_branch(&repo, "review/pr-333-drift");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "review/pr-333-drift",
            "0000000000000000000000000000000000000000",
            "t-dryrun-drift",
            None,
            None,
        )
        .unwrap();
        seed_terminal_task(&home, "t-dryrun-drift");

        let result = reconcile_terminal_review_intents(&home, true);
        assert!(
            !result
                .candidates
                .iter()
                .any(|c| c.outcome.starts_with("qualified")),
            "head-drift dry-run must be preserved, never qualified: {result:?}"
        );
        assert!(
            branch_exists(&repo, "review/pr-333-drift"),
            "branch must survive drift dry-run"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_cas_stale_ref_survives() {
        let home = tmp_dir("cas-stale");
        let repo = tmp_repo("cas-stale-repo");
        let old_tip = make_branch(&repo, "review/pr-444-cas");
        let rs = repo.display().to_string();
        persist_intent(
            &home,
            &rs,
            "review/pr-444-cas",
            &old_tip,
            "t-cas-stale",
            None,
            None,
        )
        .unwrap();
        seed_terminal_task(&home, "t-cas-stale");

        // Advance the branch head so the CAS expected_head is now stale.
        git_in(&repo, &["checkout", "review/pr-444-cas"]);
        std::fs::write(repo.join("new.txt"), "new content").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "advance",
            ],
        );
        let new_tip = branch_tip(&repo, "review/pr-444-cas");
        git_in(&repo, &["checkout", "main"]);
        assert_ne!(old_tip, new_tip, "branch must have advanced");

        let result = reconcile_terminal_review_intents(&home, false);

        assert_eq!(result.settled, 0, "stale CAS must not delete: {result:?}");
        assert!(
            branch_exists(&repo, "review/pr-444-cas"),
            "branch with new head must survive stale CAS"
        );
        assert_eq!(
            branch_tip(&repo, "review/pr-444-cas"),
            new_tip,
            "new head must be preserved"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn reconcile_production_entry_wired() {
        let source = include_str!("worktree_cleanup.rs");
        assert!(
            source.contains("reconcile_terminal_review_intents"),
            "worktree_cleanup.rs must call reconcile_terminal_review_intents \
             — if this fails, the reconciler was unwired from the periodic sweep"
        );
    }
}
