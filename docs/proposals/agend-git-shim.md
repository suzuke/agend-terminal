# agend-git-shim Implementation Plan

> **Goal**：以最小侵入性把 git 透明地納入 fleet 管理。Daemon 控 binding，shim 翻譯 cwd，git hook 補 commit 脈絡。所有 backend 一視同仁、agent 行為零學習成本。

**Status**: Draft (Phase 0 PoC 已驗證，5 個技術假設全 PASS)

**Date**: 2026-05-05

---

## 0. 背景與動機

### 0.1 問題

多 agent 場景下 git 的痛點（簡述）：

- **單 working tree 限制**：`git checkout` 一次只能在一個分支；多 agent 同 cwd 時 checkout race（§10.4 amendment 提到 1 小時內 12+ 次 checkout）
- **Worktree 雖是主流解但有自身摩擦**：env / DB / port 衝突、磁碟、清理紀律
- **Commit history 缺脈絡**：commit 訊息只說「發生了什麼」，記不到 task / agent / prompt
- **跨 backend 不一致**：每個 backend 的 hook / shell tool 機制都不同

詳細痛點清單見另一份背景文件（landscape research）。

### 0.2 設計原則

1. **單一控制平面**：PATH shim + git 原生 hook，兩者都是 POSIX/git 原生機制
2. **Backend-agnostic**：不依賴 Claude/Codex/Kiro/Gemini 任何特殊功能
3. **Agent 零學習成本**：agent 用既有 git CLI、既有 cwd 行為
4. **Daemon-driven binding**：cwd 不持久是各 backend 共通事實，不逆它
5. **Fail-safe**：`AGEND_GIT_BYPASS=1` 緊急逃生
6. **零侵入既有代碼**：擴充 `agent.rs:368-376` PATH 注入既有邏輯，不改 schema

### 0.3 Phase 0 PoC 結論

5 個技術假設全部驗證通過：

| # | 假設 | 結果 |
|---|---|---|
| A1 | PATH shim 攔得到 git | ✅ |
| A2 | shim 自動 chdir 對 agent 透明 | ✅ |
| A3 | binding.json 動態切換瞬間生效 | ✅ |
| A4 | git 原生 hook 注入 fleet trailer | ✅ |
| A5 | daemon spawn 各 backend 能注入 PATH | ✅ (agent.rs 已有 90% 基礎設施) |

PoC 留在 `/tmp/agend-shim-poc/`。

---

## 1. 系統架構

### 1.1 三個 component

```
┌──────────────────── agend daemon ────────────────────┐
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │ Binding Manager (新模組)                          │ │
│  │   • spawn agent: cmd.env(PATH=...:$AGEND_HOME/bin)│ │
│  │   • 派 task 時寫 runtime/<agent>/binding.json     │ │
│  │   • 管 worktree lifecycle (建/收)                 │ │
│  │   • 寫 fleet_events.jsonl                          │ │
│  └─────────────────────────────────────────────────┘ │
└───────────────┬───────────────────┬───────────────────┘
                │ writes                │ spawns
                ↓                       ↓
   $AGEND_HOME/runtime/        agent process (claude/codex/kiro/gemini)
     <agent>/binding.json         env: PATH, AGEND_HOME, AGEND_INSTANCE_NAME
                ↑                       │ runs git ...
                │ reads                 ↓
                │              ┌─────────────────────┐
                │              │  agend-git (Rust)   │
                └──────────────│   • 讀 binding      │
                               │   • chdir + exec    │
                               │   • deny list       │
                               └─────────┬───────────┘
                                         ↓ exec
                               ┌─────────────────────┐
                               │  /usr/bin/git       │
                               │   .git/hooks/       │
                               │     prepare-commit- │
                               │     msg → trailer   │
                               └─────────────────────┘
```

### 1.2 序列圖：派 task 全流程

```
lead          daemon                 binding.json   agent     shim   git
 │   派 task   │                            │         │        │      │
 │────────────>│                            │         │        │      │
 │             │ build/find worktree        │         │        │      │
 │             │ install hooks              │         │        │      │
 │             │ write binding              │         │        │      │
 │             │───────────────────────────>│         │        │      │
 │             │ task message (with         │         │        │      │
 │             │  workdir hint)             │         │        │      │
 │             │─────────────────────────────────────>│        │      │
 │             │                            │         │ git st │      │
 │             │                            │         │───────>│      │
 │             │                            │  read   │        │      │
 │             │                            │<────────┼────────│      │
 │             │                            │         │        │ chdir│
 │             │                            │         │        │ exec │
 │             │                            │         │        │─────>│
 │             │                            │         │  out   │      │
 │             │                            │         │<───────┼──────│
 │             │ task done                  │         │        │      │
 │             │<──────────────────────────────────────────────│      │
 │             │ clear binding              │         │        │      │
 │             │ schedule worktree GC       │         │        │      │
```

---

## 2. 介面 Spec

### 2.1 binding.json schema

路徑：`$AGEND_HOME/runtime/<agent_name>/binding.json`

```jsonc
{
  "version": 1,
  "agent": "claude-76f359",
  "task": "T-123",                // null 為 unbound
  "worktree": "/Users/.../worktrees/claude-76f359-feature-x",
  "branch": "feature-x",
  "issued_at": "2026-05-05T12:30:00Z",
  "issued_by": "lead"             // dispatch 來源
}
```

unbound 時：

```jsonc
{
  "version": 1,
  "agent": "claude-76f359",
  "task": null
}
```

**Invariants**:

- `task != null ⟺ worktree != null ∧ branch != null`
- daemon 是唯一 writer，shim/hook 唯讀
- 寫入時用 `fcntl flock`（POSIX）+ 原子 rename
- 讀取容錯：解析失敗等同 unbound（fail-safe）

### 2.2 Commit trailer 格式

由 `prepare-commit-msg` hook 注入，格式遵守 [git interpret-trailers](https://git-scm.com/docs/git-interpret-trailers) RFC 822 風格：

```
<原本 commit message>

Agend-Agent: claude-76f359
Agend-Task: T-123
Agend-Branch: feature-x
Agend-Issued-At: 2026-05-05T12:30:00Z
```

**規則**:

- 跳過 `merge` / `squash` / `template` 來源
- 已有 `Agend-Agent:` 則 idempotent skip
- hook fail 不阻擋 commit (`exit 0` always)

### 2.3 Shim 行為矩陣

| git subcommand | unbound | bound to `<X>` | deny reason |
|---|---|---|---|
| `status`, `log`, `diff`, `show`, `blame`, `ls-*`, `rev-parse`, `fetch` | passthrough (caller cwd) | chdir + pass (worktree) | — |
| `commit` | DENY | chdir + pass (hook 注入) | unbound |
| `checkout`/`switch <X>` (same branch) | DENY | chdir + pass (no-op) | unbound |
| `checkout`/`switch <Y>` (different branch) | DENY | DENY | cross-branch |
| `checkout -b <new>` | DENY | DENY | fleet-managed |
| `branch` (純建 ref) | passthrough | chdir + pass | — |
| `worktree add/move/remove` | DENY | DENY | fleet-managed |
| `push`, `pull`, `reset`, `revert`, `cherry-pick` | DENY | chdir + pass | unbound |
| `stash` | DENY | chdir + pass | unbound |
| `config`, `submodule`, `help`, `version` | passthrough | chdir + pass | — |
| `*` (其他) | passthrough | chdir + pass | — |

**逃生口**: `AGEND_GIT_BYPASS=1` 環境變數 → 全部 passthrough，繞過 binding 邏輯。

### 2.4 ERROR 訊息規範

統一格式（LLM 友善，仿 git/cargo/pip 原生 ERROR）：

```
agend-git: ERROR <one-line summary>
           <context line>
           HINT: <recovery action>
```

**範例**:

```
$ git checkout main
agend-git: ERROR cross-branch checkout denied
           this instance is bound to feature-x via task T-123
           HINT: ask lead to reassign, or wait until task completes
exit status 1

$ git commit -m "..."   # unbound mode
agend-git: ERROR commit denied in unbound mode
           this instance has no active task binding
           HINT: request a task assignment via lead
exit status 1

$ git worktree add ...
agend-git: ERROR worktree creation is fleet-managed
           HINT: lead allocates worktrees on task dispatch
exit status 1
```

每個 deny 也寫一筆事件到 `fleet_events.jsonl`：

```json
{"kind":"git_command_denied","agent":"...","reason":"cross-branch","attempted":"...","ts":"..."}
```

### 2.5 Hook 安裝點

由 daemon 在建 worktree 時複製到 `$WORKTREE/.git/hooks/`：

- `prepare-commit-msg` (必裝)
- `post-checkout` (選裝，回報 cwd 給 daemon)
- `pre-push` (選裝，把 trailer 同步成 PR body)

**重要**：hook 用 git 原生機制，不依賴任何 backend。

---

## 3. 程式碼改動清單

### 3.1 新增 binary: `crates/agend-git/`

新 Cargo crate（生產 Rust 版 shim），約 200-300 行。

```
crates/agend-git/
  Cargo.toml
  src/
    main.rs          # entry point, dispatch
    binding.rs       # 讀 binding.json with flock
    classify.rs      # subcommand → action mapping
    deny.rs          # ERROR formatting + fleet_events 記錄
    passthrough.rs   # exec /usr/bin/git
```

**核心邏輯**：

```rust
fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();

    if env::var_os("AGEND_GIT_BYPASS").is_some() {
        return passthrough::exec(&real_git_path(), &args);
    }

    let binding = binding::read_for_self();   // 讀 own agent's binding.json
    let action = classify::resolve(&args, &binding);

    match action {
        Action::Passthrough => passthrough::exec(&real_git_path(), &args),
        Action::ChdirAndPass(wt) => {
            env::set_current_dir(&wt)?;
            passthrough::exec(&real_git_path(), &args)
        }
        Action::Deny { reason, hint } => {
            deny::print(&reason, &hint);
            deny::record_event(&binding, reason);
            ExitCode::from(1)
        }
    }
}
```

**部署**：cargo build 後 link/copy 到 `$AGEND_HOME/bin/git`。

### 3.2 agend-terminal core 改動

#### 3.2.1 `src/agent.rs` (擴充既有 PATH 注入)

**位置**：line 368-376

**修改**：在 PATH prepend 邏輯前面加上 `$AGEND_HOME/bin/`：

```rust
// 新增：把 shim 目錄放在 PATH 最前面
if let Ok(home) = std::env::var("AGEND_HOME") {
    let shim_dir = PathBuf::from(home).join("bin");
    if shim_dir.exists() {
        paths.push(shim_dir);  // 第一順位
    }
}
// 既有：agend-terminal binary dir
if let Ok(exe) = std::env::current_exe() {
    if let Some(dir) = exe.parent() {
        paths.push(dir.to_path_buf());
    }
}
// 既有：caller PATH
if let Some(existing) = std::env::var_os("PATH") {
    paths.extend(std::env::split_paths(&existing));
}
```

**也加**：注入 `AGEND_HOME` 給 agent process（如果尚未注入）：

```rust
if let Ok(home) = std::env::var("AGEND_HOME") {
    cmd.env("AGEND_HOME", home);
}
```

#### 3.2.2 新增 `src/binding.rs`（daemon 內部模組）

職責：

- 寫/讀 `runtime/<agent>/binding.json`
- 提供 IPC API 給 task dispatch 流程
- flock + 原子 rename
- 啟動時 reconcile（孤兒檔清掉）

API：

```rust
pub struct BindingManager { home: PathBuf }

impl BindingManager {
    pub fn bind(&self, agent: &str, task: &Task, branch: &str, worktree: &Path) -> Result<()>;
    pub fn unbind(&self, agent: &str) -> Result<()>;
    pub fn read(&self, agent: &str) -> Result<Option<Binding>>;
    pub fn reconcile_on_startup(&self, fleet_state: &FleetState) -> Result<()>;
}
```

#### 3.2.3 新增 `src/worktree_pool.rs`

職責：

- 建/找 / 回收 worktree（取代 §10.4 的人類紀律）
- worktree 命名：`$AGEND_HOME/worktrees/<agent>-<branch-sanitized>/`
- 安裝 hooks 到 `.git/hooks/`
- 複製 `.env*` 模板（仿 gtr 設定）
- task done → 標記候選回收 → 一段時間後 GC

API：

```rust
pub struct WorktreePool { ... }

impl WorktreePool {
    pub fn lease(&self, agent: &str, branch: &str) -> Result<WorktreeLease>;
    pub fn release(&self, lease: WorktreeLease) -> Result<()>;
    pub fn gc_idle(&self, idle_threshold: Duration) -> Result<usize>;
}

pub struct WorktreeLease { pub path: PathBuf, pub branch: String, ... }
```

#### 3.2.4 task dispatch 改動（具體位置 Phase 1 確認）

派 task 流程加掛：

```rust
// Pseudo-code in dispatch handler
async fn dispatch(task: Task, target: AgentId) {
    if task.requires_worktree() {
        let lease = worktree_pool.lease(&target, &task.branch)?;
        binding_mgr.bind(&target, &task, &task.branch, &lease.path)?;
        let msg = format!(
            "{}\n\nWorking directory: {}",
            task.message,
            lease.path.display()
        );
        send(target, msg).await?;
    } else {
        send(target, task.message).await?;
    }
}

// On task done
async fn handle_done(task: TaskId, agent: AgentId) {
    binding_mgr.unbind(&agent)?;
    worktree_pool.release(lease).await?;  // 標候選回收
}
```

`task.requires_worktree()` 判定：task kind 是 implementation / review。

#### 3.2.5 `src/api/` 或 `src/mcp/` MCP tool 擴充

新增（或改名既有）MCP tool：

```
worktree.list                  # 列當前 fleet 的 worktree
binding.show <agent>           # 給操作員 / 其他 agent debug 用
fleet_events.tail --filter git # 看 deny 事件
```

### 3.3 Hook 模板

新增 `assets/hooks/`：

```
assets/hooks/
  prepare-commit-msg     # 從 binding.json + env 注入 trailer
  post-checkout          # (可選) 通知 daemon
```

`worktree_pool.lease()` 建 worktree 後 copy 過去（`.git/hooks/` 在 worktree 共享 `.git` 設定下也可能用 `git config core.hooksPath` 指向 fleet 統一目錄）。

### 3.4 fleet.yaml 演進

既有欄位 `worktree` / `git_branch` 從「nullable but unused」變成 daemon 自動填寫。**不需要改 schema**。

可選：加 `worktree_required: bool` 欄位讓 instance 顯式宣告（例如 `general` 設 false 永遠 unbound）。

---

## 4. Phase 排序與 Deliverable

### Phase 1 — Hook trailer (1.5 天)

**目標**：先把 commit 脈絡這個零風險獨立功能做掉，順便驗證 daemon → env → hook 鏈路。

**Deliverables**:

- `assets/hooks/prepare-commit-msg` 模板（PoC 已有，改 production-ready）
- `WorktreePool` 雛形：只實作 `install_hook(worktree)`
- daemon 啟動時對既有 `worktrees/*` 補裝 hook（向下相容）
- 文件：trailer 格式規範

**Exit criteria**:

- 手動建 worktree + 派任意 dummy task，commit 看到 trailer
- 三個 backend（claude / codex / kiro / gemini 任選兩）都驗證一次

**風險**：極低（不動 git 行為）

---

### Phase 2 — Binding state + shim binary (4 天)

**目標**：核心 component 上線。

**Deliverables**:

- `crates/agend-git/` 完整 binary
- `src/binding.rs`：BindingManager 完整 + flock
- `agent.rs`: PATH 注入加 `$AGEND_HOME/bin/`
- daemon 啟動時 reconcile binding 孤兒
- shim 行為矩陣的 `passthrough` + `chdir+pass` 兩個 mode（先不做 deny）
- `AGEND_GIT_BYPASS=1` 逃生口

**Exit criteria**:

- bound 模式：agent 從任意 cwd 跑 git，shim 自動 chdir 對 worktree
- unbound 模式：行為跟原本 git 一樣
- 動態切換 binding 立即生效
- shim overhead < 10ms（Rust release build）
- bypass 環境變數驗證

**風險**：中（首次動 PATH，要保 bypass 可逃）

---

### Phase 3 — Worktree lifecycle (3 天)

**目標**：把 §10.4 從人類紀律自動化。

**Deliverables**:

- `src/worktree_pool.rs` 完整實作
- task dispatch 加 `lease/release` 鉤子
- task done 觸發 release
- 啟動時 reconcile 孤兒 worktree
- `.env*` 複製、port offset hash（從 worktree path）
- `worktree.list` MCP tool

**Exit criteria**:

- lead 派 impl task → daemon 自動建 worktree + install hook + bind
- task done → worktree 標候選 → 24h 後 GC（可調）
- 至少跑一次「同一 agent 接續做兩個不同 branch task」整個流程不 respawn

**風險**：中（worktree GC 要小心不刪人類正在用的）

---

### Phase 4 — Deny list 完整化 (2 天)

**目標**：補 shim 防禦面，把 §10.4 的「checkout race」根除。

**Deliverables**:

- shim 完整 deny 行為矩陣（cross-branch checkout、worktree add、unbound mutate）
- ERROR 訊息統一格式
- deny 事件寫 `fleet_events.jsonl`
- daemon 偵測連續 N 次同 deny → inbox 通知 lead
- 操作員 CLI: `agend git override` 暫時放行（debug 用）

**Exit criteria**:

- 故意製造 cross-branch checkout 攻擊，全部被擋
- ERROR 訊息對 LLM 可解析
- 連續 deny 觸發 lead 通知

**風險**：中低（核心邏輯已驗證，補周邊）

---

### Phase 5 — Hotspot detection (選做，2 天)

**目標**：開始啃 Agentic Drift 的入門款。

**Deliverables**:

- 從 commit trailer 即時建索引：`(file → set of agents that touched it last 7 days)`
- `prepare-commit-msg` 額外檢查：本 commit 的檔案有沒有「7 天內別 agent 也動過」
- 命中 → commit 不擋，但發 inbox warning 給 lead

**Exit criteria**:

- 模擬兩個 agent 動同 hotspot file，第二者 commit 後 lead 收到 warning

**風險**：低（純資訊增強，不改決策）

---

## 5. 測試策略

### 5.1 PoC 升級為 integration test

把 `/tmp/agend-shim-poc/` 的測試流程搬進 `tests/git_shim_integration.rs`：

```rust
#[test]
fn shim_chdir_transparent_from_arbitrary_cwd() { ... }

#[test]
fn binding_dynamic_switch_immediate() { ... }

#[test]
fn cross_branch_checkout_denied() { ... }

#[test]
fn unbound_mutate_denied() { ... }

#[test]
fn bypass_env_disables_shim() { ... }

#[test]
fn hook_injects_trailer_on_commit() { ... }
```

每個 test 用 `tempdir()` 建獨立 sandbox。

### 5.2 跨 backend 驗證

Phase 2 結束後跑一次 fleet 級驗證：

| Backend | 驗證項目 |
|---|---|
| claude (lead, claude-76f359) | 自家 PoC，已驗 |
| codex (reviewer) | 派 review task，看 git log 是否從 worktree 看 |
| kiro-cli (dev, kiro-cli-*) | 派 implementation task，shim chdir 是否生效 |
| gemini | 派 read-only 查詢，unbound passthrough 是否正常 |

驗證劇本（可寫成 `scripts/cross-backend-smoke-test.sh`）：對每個 backend 派個 mini task「在 feature-test 分支加一行到 README，commit 並 push」，檢查：

1. shim 是否被攔到（看 fleet_events）
2. commit 的 trailer 是否正確
3. 沒有 worktree 之外的污染

### 5.3 Race / failure injection

- 並發寫 binding.json（兩 thread 同時呼叫 `bind()`）→ 驗證 flock
- daemon crash 中途 → 重啟後 reconcile 是否正確
- worktree 被外部刪除（操作員手動 rm）→ shim 是否 graceful fail
- binding.json 損壞 → fail-safe 退回 unbound

### 5.4 回歸保護

- 既有 §10.4 worktree 命名約定相容性測試
- 既有 fleet.yaml schema 不破壞（新欄位都 optional）
- `agend git status` 等老指令在 unbound 模式行為跟過去一致
- benchmark：100 次 git commands 通過 shim vs 直接，平均延遲 diff < 10ms

---

## 6. 風險登記

| ID | 風險 | 嚴重度 | 緩解 |
|---|---|---|---|
| R1 | shim panic 卡死整個 fleet 的 git | 高 | 強制 `AGEND_GIT_BYPASS=1` 逃生 + Phase 2 上線前 fuzz test |
| R2 | binding.json 並發寫 race | 中 | flock + atomic rename + daemon 是唯一 writer |
| R3 | Worktree GC 刪到人類正在用的 | 高 | GC 只回收 daemon 紀錄為 idle 的；保留 N 天；操作員可 pin |
| R4 | hook fail 阻擋 commit | 中 | hook 永遠 `exit 0`，trailer 失敗只 log |
| R5 | 弱 backend 不理解 ERROR 訊息持續 retry | 中 | daemon 偵測連續 deny 通知 lead |
| R6 | shim 性能不夠 | 低 | Rust release + 不每次重讀 binding（mtime cache） |
| R7 | 跨 backend 行為不一 | 中 | Phase 2 結束跑 cross-backend smoke test |
| R8 | Windows 不支援 | 低 | Phase 5 後評估，現階段 macOS/Linux only |
| R9 | 既有 worktree 跟新命名衝突 | 中 | reconcile 邏輯保留人類命名；只接管自動建立的 |
| R10 | binding.json 路徑被 agent 推測並偽造 | 低 | 只有 daemon 有寫權限，shim 唯讀 |

---

## 7. 開工 Checklist

實作開始前要確認：

- [ ] 確認 `target/release/` vs `$AGEND_HOME/bin/` 哪個放 shim（傾向後者，避免污染 build dir）
- [ ] 確認 `crates/agend-git` 在 workspace 還是獨立 crate
- [ ] §10.4 既有 worktree 命名 `<agent>-<repo-path>` vs 新提議 `<agent>-<branch>` 是否衝突 → 決定要不要改命名
- [ ] task board 結構是否有「task kind」欄位區分 impl/review/plan？沒有要加
- [ ] 既有 `worktrees/` 下的人類手建 worktree 怎麼處理（Phase 3 reconcile 規則）
- [ ] hook 安裝策略：每 worktree 一份 vs `core.hooksPath` 指向統一目錄（傾向後者，方便升級）
- [ ] shim ERROR 走 stderr 還是 stdout（傾向 stderr，跟 git 原生一致）
- [ ] AGEND_HOME 注入是否在所有 backend 都 OK（已確認 claude，其他要驗）
- [ ] 是否要支援 `agend git override` 操作員 CLI（用於緊急 debug）

---

## 8. 預估工時

| Phase | 樂觀 | 悲觀 |
|---|---|---|
| Phase 1 trailer | 1 天 | 2 天 |
| Phase 2 binding + shim | 3 天 | 6 天 |
| Phase 3 worktree pool | 2 天 | 4 天 |
| Phase 4 deny list | 1 天 | 3 天 |
| Phase 5 hotspot | 2 天 | 3 天 |
| **合計** | **9 天** | **18 天** |

不含跨 backend smoke test（每 phase 結束加 0.5 天）。

---

## 9. Out-of-scope（明確排除）

- mrq-style 連續快照（補 commit-之間的縫，獨立 product）
- bash 整體劫持（safeexec 範疇）
- Windows native 支援（macOS/Linux 為主，WSL 等於 Linux）
- 完整 D 群（Agentic Drift）解決方案 — 本計畫只解到 hotspot warning，不做 spec-driven 任務拆分
- 替換 git（GitButler 路線）

---

## 附錄 A：Phase 0 PoC 驗證紀錄

PoC 程式碼留在 `/tmp/agend-shim-poc/`。

### A.1 沙盒結構

```
/tmp/agend-shim-poc/
  shim/
    git                # bash + python shim
  hooks/
    prepare-commit-msg # trailer hook
  runtime/
    binding.json       # daemon-driven binding state
    shim.log
  repo/                # bare-ish main repo
  worktrees/
    feature-x/
    scratch/
```

### A.2 關鍵實測 trace

```
agent (cwd: /Users/suzuke/.agend-terminal/workspace/claude-76f359)
   └─ git commit -m "..."          ← agent 從錯誤 cwd 呼叫
        └─ shim
             ├─ 讀 binding.json: worktree=worktrees/feature-x
             ├─ chdir(worktrees/feature-x)
             └─ exec /usr/bin/git commit
                  └─ git 觸發 prepare-commit-msg
                       ├─ 讀 binding.json: task=T-PoC-trailer, branch=feature-x
                       ├─ 讀 env: AGEND_INSTANCE_NAME=claude-76f359
                       └─ 寫入 trailer

實際 commit message:
   test commit from /tmp

   Agend-Agent: claude-76f359
   Agend-Task: T-PoC-trailer
   Agend-Branch: feature-x
```

### A.3 驗證範圍

| # | 假設 | 結果 | 證據 |
|---|---|---|---|
| A1 | PATH shim 攔得到 git | ✅ | `which git` 指向 shim；passthrough 正常 |
| A2 | shim 自動 chdir 對 agent 透明 | ✅ | 從 `/tmp` / `$HOME` / agend workspace 跑 git，全部看到 bound worktree（feature-x）狀態，agent 完全不需 cd |
| A3 | binding.json 動態切換瞬間生效 | ✅ | feature-x → scratch → unbound → feature-x 連續切換，每次下次 git 立即生效；overhead ~30ms（bash+python）|
| A4 | git 原生 hook 注入 fleet trailer | ✅ | 從 `/tmp` 跑 commit → shim chdir → hook 觸發 → trailer 正確寫入 |
| A5 | daemon spawn 各 backend 能注入 PATH | ✅ | agent.rs:368-376 已有 PATH prepend 邏輯；line 344 已注入 `AGEND_INSTANCE_NAME`；自驗 `AGEND_HOME` / `PATH 第一順位` 都已就位 |

### A.4 PoC 性能

bash + python 版每次 ~30ms overhead。

預期 Rust release build < 5ms（無 fork python）。

---

## 附錄 B：背景文件交叉引用

- 痛點 landscape：見另一份 git multi-agent pain points research（A-F 痛點群）
- 既有工具參考：Worktrunk、git-worktree-runner (gtr)、AI Trailers、ai-session、entireio/cli、airlock 架構
- Fleet protocol §10.4：worktree mandatory rule (v1.2 amendment)
- Fleet protocol §10.5：spawn-site rationale

---

**End of Plan**
