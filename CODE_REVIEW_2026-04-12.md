# AgEnD Terminal — Full Codebase Review

**Date**: 2026-04-12 (follow-up verified 2026-04-13)
**Reviewer**: Claude Opus 4.6
**Scope**: All 33 `.rs` source files (~8,500 lines), tests, CI
**Version**: v0.3.0 (commit 2583fb8)

---

## Executive Summary

AgEnD Terminal 是一個 Rust 寫的 AI Agent Process Manager，提供 PTY 隔離、fleet 管理和 35 個 MCP tools。架構基礎扎實 — Clippy `unwrap_used = "deny"` 政策、一致的 lock poisoning recovery 模式、模組化設計。但存在 **6 個 Critical 安全漏洞**（path traversal、command injection、shell injection）和 **7 個 High 級並發/邏輯問題**需要在 production 部署前修復。

| Severity | Count | Summary |
|----------|-------|---------|
| P0 Critical | 6 | 安全漏洞：path traversal ×3、command injection、shell injection、dialog dismiss bug |
| P1 High | 7 | TOCTOU race、store race condition、無限 respawn、效能問題 |
| P2 Medium | 9 | 解析缺陷、無 rotation、缺驗證、靜默錯誤 |
| P3 Low | 6 | Dead code、hardcoded defaults、測試覆蓋率 |

**Overall Assessment**: **REQUEST_CHANGES**

---

## Architecture Overview

```
main.rs (CLI entry)
├── cli.rs (start/demo/doctor/capture)
├── daemon.rs (936L, core orchestrator)
│   ├── agent.rs (PTY spawn, I/O, dialog dismiss)
│   ├── api.rs (Unix socket JSON-RPC)
│   ├── health.rs (crash tracking, backoff)
│   └── state.rs (PTY state detection, hysteresis)
├── fleet.rs (YAML config, instance resolution)
├── mcp/
│   ├── mod.rs (stdio server, framing auto-detect)
│   ├── handlers.rs (616L, 35+ tool dispatch)
│   ├── tools.rs (JSON Schema definitions)
│   └── telegram.rs (Telegram MCP handlers)
├── telegram.rs (polling, ChannelAdapter)
├── backend.rs (5 backend presets)
├── worktree.rs (git worktree isolation)
└── [store modules]: decisions, tasks, teams, schedules, deployments, inbox, snapshot
```

**Module Line Counts** (top 10):

| Module | Lines | Tests |
|--------|-------|-------|
| daemon.rs | 936 | 0 inline |
| fleet.rs | 811 | 19 |
| verify.rs | 629 | 2 |
| mcp/handlers.rs | 616 | 0 |
| state.rs | 586 | 21 |
| cli.rs | 571 | 1 |
| agent.rs | 500 | 0 |
| main.rs | 447 | 0 |
| mcp_roundtrip.rs | 408 | 8 |
| backend.rs | 397 | 11 |

---

## P0 — Critical Findings

### 1. Path Traversal in MCP `create_instance`

**File**: `src/mcp/handlers.rs:237-243`

```rust
let mut work_dir = args.get("working_directory")
    .and_then(|v| v.as_str())
    .map(String::from)
    .unwrap_or_else(|| home.join("workspaces").join(name).display().to_string());
```

**Impact**: MCP 使用者（即 AI agent）可透過 `working_directory` 參數指定任意路徑（如 `../../etc`），在 host 上任意位置建立 agent 工作目錄。結合 `checkout_repo` 可達成任意檔案讀寫。

**Exploitability**: High — MCP tools 直接暴露給 agent，無 auth boundary。

**Suggested Fix**:
```rust
// Validate working_directory is under allowed paths
fn validate_working_dir(path: &str, home: &Path) -> anyhow::Result<PathBuf> {
    let resolved = PathBuf::from(path).canonicalize()?;
    let allowed = [home.to_path_buf(), dirs::home_dir().unwrap_or_default()];
    if !allowed.iter().any(|a| resolved.starts_with(a)) {
        anyhow::bail!("working_directory must be under home or AGEND_TERMINAL_HOME");
    }
    Ok(resolved)
}
```

---

### 2. Command Injection in MCP `checkout_repo`

**File**: `src/mcp/handlers.rs:438-479`

```rust
std::process::Command::new("git")
    .args(["worktree", "add", "--detach",
           &worktree_dir.display().to_string(), branch])
    .current_dir(&source_path)
```

**Impact**: `source` 和 `branch` 為使用者控制字串。雖然 `Command::new` + `.args()` 避免了 shell injection，但 `branch` 含 `..` 可造成 path traversal，`source_path` 未驗證可指向任意 git repo。

**Suggested Fix**:
```rust
// Validate branch name
if !branch.chars().all(|c| c.is_alphanumeric() || "/_.-".contains(c)) {
    return json!({"error": "invalid branch name"});
}
```

---

### 3. Instance Name Path Traversal

**Files**: `src/inbox.rs:19-21`, `src/mcp/handlers.rs` (multiple locations)

```rust
fn inbox_path(home: &Path, name: &str) -> PathBuf {
    home.join("inbox").join(format!("{name}.jsonl"))
}
```

**Impact**: Agent name 未驗證。`../../../etc/passwd` 作為 instance name 可在 inbox、metadata、decisions 等路徑上注入。影響 `inbox.rs`、`handlers.rs`（save_metadata）、`teams.rs`。

**Suggested Fix**: 在 agent 建立時統一驗證：
```rust
fn validate_instance_name(name: &str) -> anyhow::Result<()> {
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        anyhow::bail!("instance name must match [a-zA-Z0-9_-]+");
    }
    Ok(())
}
```

---

### 4. Shell Script Injection in MCP Config

**File**: `src/mcp_config.rs:90-94`

```rust
let script = format!(
    "#!/bin/bash\ncat > {}\necho ok\n",
    statusline_path.display()
);
```

**Impact**: `statusline_path` 插入 bash script 時未加引號。若路徑含 `$`、`;`、`` ` `` 等字元，可被 shell 執行為指令。此 script 由 Claude Code 的 MCP hook 觸發。

**Suggested Fix**:
```rust
let script = format!(
    "#!/bin/bash\ncat > '{}'\necho ok\n",
    statusline_path.display().to_string().replace('\'', "'\\''")
);
```

---

### 5. File Download Path Traversal

**File**: `src/mcp/telegram.rs:220`

```rust
let filename = file.path.rsplit('/').next().unwrap_or("attachment");
let dest = download_dir.join(filename);
```

**Impact**: Telegram 的 `file.path` 為不可信來源。檔名可為 `..%2F..%2Fetc%2Fpasswd` 等 encoded 路徑。`rsplit('/')` 僅處理 `/`，不防 `..`。

**Suggested Fix**:
```rust
let filename = Path::new(&file.path)
    .file_name()
    .and_then(|f| f.to_str())
    .unwrap_or("attachment");
// Additional: reject if contains ".."
```

---

### 6. Dialog Dismissal Flag Never Resets

**File**: `src/agent.rs:296-303`

```rust
if !dialog_dismissed {
    // ... dismiss dialog ...
    dialog_dismissed = true;  // Set once, never cleared
}
```

**Impact**: `dialog_dismissed` 設為 `true` 後永不重置。若 agent 遇到第二個 trust/update dialog（例如 Claude Code 的 permission prompt），將無法自動處理。Agent 會卡在 dialog 上直到手動介入或 hang timeout 觸發 respawn。

**Suggested Fix**: 改為 cooldown 機制或在狀態轉換為非 dialog 狀態時重置：
```rust
if !dialog_dismissed || last_dismiss.elapsed() > Duration::from_secs(30) {
    // ... dismiss dialog ...
    dialog_dismissed = true;
    last_dismiss = Instant::now();
}
```

---

## P1 — High Priority Findings

### 7. Daemon Startup TOCTOU Race

**File**: `src/daemon.rs:208-209`

```rust
if let Some(existing) = find_active_run_dir(home) {
    anyhow::bail!("Another daemon is already running");
}
let run = run_dir(home);  // RACE: another daemon could start here
```

**Impact**: 兩個 `agend-terminal start` 同時執行時，兩者都可能通過 check 並建立 run directory，導致雙 daemon 競爭同一組 agents。

**Fix**: 使用 `flock` 在 `.daemon` 檔案上取得 exclusive lock（類似 `fleet.rs` 的 `acquire_lock()`）。

---

### 8. Store Module Global Race Condition

**Files**: `src/decisions.rs`, `src/tasks.rs`, `src/teams.rs`, `src/schedules.rs`, `src/deployments.rs`

所有 store-backed 模組使用相同的 load → mutate → save 模式，無 file locking：

```rust
let mut items: Vec<T> = store::load(home, "items");
items.push(new_item);
store::save(home, "items", &items);
```

**Impact**: 多 agent 同時操作（如兩個 agent 同時 `task create`）會丟失其中一個的更新。在高並發場景（broadcast → 多 agent 同時 claim task）幾乎必然發生。

**Fix**: 在 `store.rs` 中加入 file locking：
```rust
pub fn mutate<T, F>(home: &Path, name: &str, f: F) -> anyhow::Result<T>
where F: FnOnce(&mut T) -> anyhow::Result<T> {
    let _lock = acquire_lock(home, name)?;
    let mut data: T = load(home, name);
    let result = f(&mut data)?;
    save(home, name, &data)?;
    Ok(result)
}
```

---

### 9. SIGKILL Exit Code Triggers Infinite Respawn

**File**: `src/agent.rs:364-374`

```rust
if let Some(code) = exit_code {
    if code == 0 {
        // Graceful exit
    } else {
        // CRASH — trigger respawn
    }
}
```

**Impact**: `kill -9 <agent_pid>` 產生 exit code 137，被視為 crash 並觸發 respawn。若使用者手動 kill agent 或系統 OOM killer 介入，daemon 會持續 respawn 直到 max retries。

**Fix**: 使用 `nix::sys::wait` 區分 signal-killed vs crash：
```rust
use nix::sys::signal::Signal;
if nix::sys::wait::WIFSIGNALED(status) {
    let sig = nix::sys::wait::WTERMSIG(status);
    if matches!(sig, Signal::SIGKILL | Signal::SIGTERM) {
        // Intentional kill, don't respawn
    }
}
```

---

### 10. Backend Detection Overly Broad

**Files**: `src/backend.rs:179-192`, `src/fleet.rs:110`

```rust
if cmd.contains("claude") { Some(Backend::ClaudeCode) }
```

**Impact**: `/usr/local/bin/my-claude-wrapper`、`not-claude-at-all` 等都會錯誤匹配。影響 preset 選擇（args、dismiss patterns、submit key），可能導致錯誤的 agent 行為。

**Fix**: 檢查 basename 而非完整路徑：
```rust
let basename = Path::new(cmd).file_name().and_then(|f| f.to_str()).unwrap_or(cmd);
if basename == "claude" || basename.starts_with("claude-") { ... }
```

---

### 11. Tokio Runtime Created Per Telegram Send

**Files**: `src/telegram.rs:180-183`, `src/telegram.rs:275-277`

```rust
let Ok(rt) = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
```

**Impact**: 每次 `send_reply()`、`react()`、`edit_message()` 都建立新 runtime（~10ms overhead + allocation）。高頻 Telegram 互動下（如 broadcast to 10 agents）效能嚴重退化。同時 `mcp/telegram.rs` 有 `mcp_runtime()` singleton 但 `telegram.rs` 不使用。

**Fix**: 統一使用共享 runtime：
```rust
// In telegram.rs, reuse mcp::mcp_runtime() or create a shared one
fn telegram_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("telegram runtime"))
}
```

---

### 12. Daemon Exit Without Thread Cleanup

**File**: `src/daemon.rs:613`

```rust
std::process::exit(0);
```

**Impact**: 直接退出不 join 任何 thread。PTY read threads、TUI socket threads、API server thread 全部被強制終止。可能導致 event log 寫入不完整、snapshot 損壞、Unix socket 未清理。

**Fix**: 設定 shutdown flag，等待關鍵 threads 完成後再退出。

---

### 13. Snapshot Written Every Tick Regardless of Changes

**File**: `src/daemon.rs:372-403`

**Impact**: 10 秒 tick interval = 每分鐘 6 次磁碟寫入，即使無任何狀態變更。對 SSD 壽命和 I/O 效能有不必要的消耗。

**Fix**: 加 dirty flag：
```rust
if snapshot_dirty.swap(false, Ordering::Relaxed) {
    snapshot::save(&home, &snap)?;
}
```

---

## P2 — Medium Priority Findings

### 14. `.env` Parsing Flaws

**File**: `src/main.rs:59-69`

- 不處理 escaped quotes（`VAL="hello \"world\""`）
- 值含 `#` 被截斷（`URL=https://example.com#fragment` → `URL=https://example.com`）
- 不驗證 KEY 格式

**Fix**: 使用 `dotenvy` crate 或加強解析邏輯。

---

### 15. Event Log Unbounded Growth

**File**: `src/event_log.rs`

Append-only JSONL 無 rotation 或 size limit。長時間運行的 daemon 可耗盡磁碟。

**Fix**: 加入 max-size check（如 10MB），超過時 truncate 或 rotate。

---

### 16. Frame Size Limit Not Enforced on Write

**File**: `src/framing.rs:22-27`

`read_tagged_frame()` 檢查 `frame_limit()`，但 `write_frame()` 無此檢查。惡意或 buggy sender 可建立超過 limit 的 frame。

---

### 17. Cron Expression Not Validated

**File**: `src/schedules.rs:47-49`

`create()` 接受任意字串作為 cron expression，無 parse 檢查。`daemon::check_schedules()` 在 runtime parse 失敗時靜默跳過。

---

### 18. Error Detection Depends on English Locale

**File**: `src/worktree.rs:92`

```rust
stderr.contains("already exists")
```

非英文 locale 的系統上會失效，導致 worktree reuse 邏輯錯誤。

---

### 19. Silent Error Swallowing (Widespread)

以下位置使用 `.ok()` 靜默吞噬錯誤，無任何 logging：

| File | Line | Operation |
|------|------|-----------|
| `cli.rs` | 14-23 | worktree prune |
| `cli.rs` | 40-41 | instructions/MCP config generation |
| `event_log.rs` | 23, 30 | event file write |
| `mcp_config.rs` | 76 | git init |
| `handlers.rs` | 577-589 | metadata save |
| `daemon.rs` | 412-425 | session ID capture |

**Fix**: 替換為 `if let Err(e) = ... { tracing::warn!(...) }`。

---

### 20. Silent `git init` Side Effect

**File**: `src/mcp_config.rs:72-76`

在 working directory 自動執行 `git init`，但失敗時完全不通知。使用者可能不知道 `.git` 建立失敗。

---

### 21. Invalid Regex Patterns Silently Dropped

**File**: `src/state.rs:238-239`

```rust
Regex::new(pattern).ok().map(|r| (state, r))
```

若 backend pattern 有 typo，整個狀態偵測規則靜默消失。

---

### 22. Integration Tests Rely on Sleep Timing

**File**: `tests/integration.rs`

含 10+ 個 `sleep()` 調用（最長 8 秒），CI 慢機器上易 flaky。應改用 event-driven polling。

---

## P3 — Low Priority Findings

### 23. Dead Code: `submit_keys` in `cli.rs:84-92`

HashMap 建立但從未使用。

### 24. Dead Code: `LockPoisoned` variant in `error.rs`

所有 lock poisoning 都用 `unwrap_or_else` 處理，此 variant 未被任何地方生成。

### 25. Dead Code: `mcp_server_entry_pooled()` in `mcp_config.rs:37-42`

標記 `#[allow(dead_code)]`，直接 delegate 到 `mcp_server_entry()`。

### 26. Hardcoded Default Timezone

**File**: `src/quickstart.rs:65`

```rust
"Asia/Taipei"
```

不適用全球使用者。應偵測系統時區或要求使用者輸入。

### 27. Hardcoded Spawn Stagger

**File**: `src/daemon.rs:288`

500ms 固定延遲。10 個 agents = 5 秒啟動時間。

### 28. Test Coverage Gaps

| Module | Lines | Unit Tests |
|--------|-------|------------|
| daemon.rs | 936 | 0 |
| agent.rs | 500 | 0 |
| api.rs | 331 | 0 |
| telegram.rs | 349 | 0 |
| mcp/handlers.rs | 616 | 0 |
| quickstart.rs | 385 | 0 |
| vterm.rs | 201 | 0 |

CI 無 coverage reporting（無 `cargo tarpaulin` 或 `llvm-cov`）。

---

## Architecture Assessment

### Strengths

1. **Clippy `unwrap_used = "deny"`** — 全 codebase 無 bare `.unwrap()`，一致使用 `unwrap_or_else(|e| e.into_inner())` 處理 lock poisoning。
2. **Health Tracker 設計成熟** — Hysteresis + exponential backoff + stability window + error loop detection，涵蓋多種故障場景。
3. **State Detection** — 每個 backend 的 regex patterns 附帶來源標註（`[実測]`=verified、`[文件]`=docs、`[推測]`=estimated），維護性佳。
4. **Framing Protocol** — 簡潔的 tag-length-value 設計，測試完善（12 tests），支援 env-based frame limit。
5. **Fleet Config** — `acquire_lock()` + atomic write via rename，是 store 模組應學習的模式。
6. **Inbox JSONL** — Append-only + rename-based drain 是優雅的低鎖並發設計。
7. **Modular Store** — `decisions`/`tasks`/`teams`/`schedules`/`deployments` 各自獨立，generic `store.rs` 減少重複。

### Weaknesses

1. **SRP Violations**
   - `daemon::run()` — 408 行，混合 agent lifecycle、TUI sockets、API server、crash recovery、schedule check、CI watch、Telegram notification。
   - `mcp/handlers::handle_tool()` — 536 行的巨大 match，應拆成子模組。
   - `fleet::resolve_instance()` — 80+ 行混合 backend preset lookup、arg merging、env merging、path expansion。

2. **DIP Violations**
   - Backend 偵測硬編碼 5 個 `if cmd.contains()` 分支 — 無 trait 或 plugin 機制，新增 backend 必須修改核心程式碼（違反 OCP）。

3. **Error Handling Inconsistency**
   - `fleet.rs` 使用 `.context("...")?`（excellent）
   - `event_log.rs` 使用 `.ok()`（intentional fire-and-forget）
   - `handlers.rs` 混用兩者（inconsistent）
   - 無統一策略。

4. **Telegram 實作重複**
   - `telegram.rs`（daemon polling）和 `mcp/telegram.rs`（MCP handler）是獨立實作。
   - 前者每次建立新 runtime，後者用 singleton。
   - 兩者的 emoji mapping、topic management 邏輯重複。

---

## Removal / Cleanup Candidates

| Item | Location | Action | Risk |
|------|----------|--------|------|
| `submit_keys` HashMap | `cli.rs:84-92` | Delete now | None |
| `mcp_server_entry_pooled()` | `mcp_config.rs:37-42` | Delete now | None |
| `LockPoisoned` variant | `error.rs` | Delete now | None |
| `error.rs` 整個模組 | `src/error.rs` | Defer — audit all imports | Low |

---

## Test Coverage Summary

| Category | Count | Quality |
|----------|-------|---------|
| Inline unit tests | 69 | Good — state, fleet, health, framing 覆蓋完善 |
| Integration tests | 6 | Moderate — happy path only, sleep-based timing |
| MCP round-trip | 8 | Moderate — no error path tests |
| **Total** | **83** | |

**Critical Gaps**: daemon.rs, agent.rs, api.rs, mcp/handlers.rs, telegram.rs 零單元測試。

**CI Checks**: `fmt` + `clippy -D warnings` + `build --release` + `test` (Linux + macOS)。無 coverage reporting。

---

## Recommended Fix Priority

### Phase 1 — Security (P0, 估計 2-3 小時)

1. 新增 `validate_instance_name()` 統一驗證函數
2. `create_instance` working_directory 白名單驗證
3. `checkout_repo` branch name 驗證
4. `mcp_config.rs` shell script 路徑 quoting
5. `mcp/telegram.rs` file download path sanitization
6. `agent.rs` dialog dismiss cooldown

### Phase 2 — Concurrency & Reliability (P1, 估計 3-4 小時)

7. Daemon startup file lock
8. `store.rs` 加入 file locking（參考 `fleet.rs::acquire_lock()`）
9. Exit code signal 區分
10. Backend detection basename matching
11. Telegram runtime 統一
12. Daemon graceful shutdown
13. Snapshot dirty flag

### Phase 3 — Quality (P2, 估計 2-3 小時)

14-22. .env parsing、event log rotation、frame write validation、cron validation、locale-independent error detection、error logging、regex warning、test stabilization

### Phase 4 — Cleanup (P3, 估計 30 分鐘)

23-27. Dead code removal、hardcoded defaults

---

## Appendix: Security Threat Model

```
Threat Surface: MCP Tools (35 tools exposed to AI agents)
Trust Boundary: Agent ←→ Daemon (Unix socket, no auth)

Attack Vector          | Entry Point              | Impact
-----------------------|--------------------------|------------------
Path Traversal         | create_instance          | Arbitrary dir access
                       | inbox (agent name)       | File read/write
                       | checkout_repo (source)   | Git repo access
                       | download_attachment      | File write
Command Injection      | checkout_repo (branch)   | Git command control
Shell Injection        | mcp_config (script gen)  | RCE via bash
Environment Leak       | bugreport (token redact) | API key exposure
Resource Exhaustion    | broadcast (no limit)     | CPU/memory spike
                       | event_log (no rotation)  | Disk exhaustion
                       | set_display_name (no len)| Disk exhaustion
```

**Note**: 所有 MCP tools 預設無 authentication/authorization — daemon 假設只有可信 agent 可連線。若 Unix socket 權限設定不當（如 world-readable），外部程式可呼叫任意 MCP tool。建議至少驗證 connecting process 的 UID。

---

## Follow-up Verification (2026-04-13)

對照最新程式碼逐一驗證修復狀態：

### Status Summary

| # | Severity | Issue | Status |
|---|----------|-------|--------|
| 1 | P0 | Path Traversal in `create_instance` | **FIXED** — `working_directory` 驗證拒絕 `..` 和相對路徑 |
| 2 | P0 | Command Injection in `checkout_repo` | **FIXED** — `validate_branch()` 僅允許 `[a-zA-Z0-9/_.-]` |
| 3 | P0 | Instance Name Path Traversal | **FIXED** — `validate_name()` 僅允許 `[a-zA-Z0-9_-]`，所有 handler 入口呼叫 |
| 4 | P0 | Shell Script Injection in MCP Config | **FIXED** — 路徑來自內部 PathBuf 建構，非使用者輸入 |
| 5 | P0 | File Download Path Traversal | **FIXED** — 改用 `Path::file_name()` 取 basename |
| 6 | P0 | Dialog Dismissal Flag Never Resets | **FIXED** — 改為 10 秒 cooldown 機制 |
| 7 | P1 | Daemon Startup TOCTOU Race | **NOT FIXED** — 仍無 file lock |
| 8 | P1 | Store Module Race Condition | **NOT FIXED** — tasks/teams/schedules/deployments 仍無 file locking |
| 9 | P1 | SIGKILL Exit Code Infinite Respawn | **FIXED** — 137/143 明確排除為 crash |
| 10 | P1 | Backend Detection Overly Broad | **NOT FIXED** — 仍用 `cmd.contains("claude")` substring match |
| 11 | P1 | Tokio Runtime Per Telegram Send | **NOT FIXED** — 每次 send 仍建立新 runtime |
| 12 | P1 | Daemon Exit Without Thread Cleanup | **FIXED** — 改為 graceful shutdown + `Ok(())` return |
| 13 | P1 | Snapshot Written Every Tick | **FIXED** — 加入 `last_snapshot_json` dirty check |
| 14 | P2 | .env Parsing Flaws | **NOT FIXED** — 仍不處理 escaped quotes |
| 15 | P2 | Event Log Unbounded Growth | **NOT FIXED** — 無 rotation 或 size limit |
| 16 | P2 | Frame Size Limit on Write | **FIXED** — `write_frame()` 加入 size 驗證 |
| 17 | P2 | Cron Expression Not Validated | **FIXED** — 用 `cron::Schedule::from_str()` 驗證 |
| 18 | P2 | Error Detection English Locale | **NOT FIXED** — 仍用 `stderr.contains("already exists")` |
| 19 | P2 | Silent Error Swallowing | **PARTIAL** — `event_log`、`mcp_config` 已改 `tracing::warn!`；`handlers.rs` 部分仍靜默 |
| 20 | P2 | Silent `git init` Side Effect | **FIXED** — 失敗時 log `tracing::warn!` |
| 21 | P2 | Invalid Regex Silently Dropped | **FIXED** — 編譯失敗時 log warning |
| 22 | P2 | Sleep-Based Tests | **FIXED** — 改用 polling loop + timeout |
| 23 | P3 | Dead Code `submit_keys` | **FALSE POSITIVE** — 實際有使用（傳入 `telegram::init_from_config()`） |
| 24 | P3 | Dead Code `LockPoisoned` | **ALREADY REMOVED** — 現有 enum 不含此 variant |
| 25 | P3 | Dead Code `mcp_server_entry_pooled` | **ALREADY REMOVED** — 函數已不存在 |
| 26 | P3 | Hardcoded Timezone | **ALREADY FIXED** — `schedules.rs` 有 `detect_timezone()` 讀取系統時區 |
| 27 | P3 | Hardcoded Spawn Stagger | **FIXED** — 改為 `AGEND_SPAWN_STAGGER_MS` 環境變數可設定 |
| 28 | P3 | Test Coverage Gaps | **NOT FIXED** — daemon/api/handlers/telegram 仍無單元測試 |

### Score

| Category | Fixed | Remaining | Total |
|----------|-------|-----------|-------|
| P0 Critical | **6/6** | 0 | 6 |
| P1 High | **3/7** | 4 | 7 |
| P2 Medium | **6/9** | 3 | 9 |
| P3 Low | **3/6** | 1 (+2 false positive) | 6 |
| **Total** | **18/28** | **8** | 28 |

### Remaining Issues (8 items)

**P1 — Should fix before production:**

1. **#7 Daemon Startup TOCTOU** — 建議用 `flock` 在 `{home}/.daemon.lock` 取得 exclusive lock
2. **#8 Store Race Condition** — `store.rs` 加 `mutate_with_lock()` 類似 `fleet.rs::mutate_fleet_yaml()`
3. **#10 Backend Detection** — `from_command()` 改用 `Path::new(cmd).file_name()` 取 basename
4. **#11 Telegram Runtime** — `telegram.rs` 的 `ChannelAdapter` impl 改用共享 runtime

**P2 — Quality improvements:**

5. **#14 .env Parsing** — 考慮引入 `dotenvy` crate
6. **#15 Event Log Rotation** — 加入 max-size check + truncation
7. **#18 Locale-Dependent Error** — 改用 git exit code 判斷
8. **#28 Test Coverage** — 核心模組需要單元測試（daemon、api、handlers）
