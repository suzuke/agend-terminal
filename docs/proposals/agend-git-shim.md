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

> **Amendment 2026-05-05**：phase 重組以收斂最小可交付 (per team review synthesis lead m-, dev m-67, reviewer m-66)。
> 原 Phase 3 將 worktree GC 跟 lifecycle 綁在一起放大 R3；GC 切出獨立 phase 才能 dry-run 觀察後再實刪。Phase 1 加 minimal binding.json writer 以支撐 trailer metadata。
>
> **Amendment 2026-05-06 (general m-16)**: Windows native 從 out-of-scope 撤回、納入 Phase 3 + Phase 1/2 retro patch。Cross-platform 從 Phase 3 起。R8 撤回、新增 R15 (Windows hook engine PowerShell vs bash)。Phase 1+2 已 ship 的 unix-only assumptions（cfg(unix) gating、`exec()` unix-only、bash hook only）需 retro patch 在 Phase 3 PR 同 ship。

### Phase 1 — Hook trailer + minimal binding writer + telemetry (1.5–2 天)

**目標**：先把 commit 脈絡這個零風險獨立功能做掉，順便驗證 daemon → env → hook 鏈路、並建立 deny telemetry baseline。

**Deliverables**:

- `assets/hooks/prepare-commit-msg` 模板（PoC 已有，改 production-ready）
- `WorktreePool` 雛形：只實作 `install_hook(worktree)`
- **Minimal `BindingManager` writer**：只寫 `task_id` + `branch` + `agent_name` + `issued_at` 到 `runtime/<agent>/binding.json`（任務 dispatch 時觸發）。lifecycle / unbind / reconcile 等延後。Hook 從這個檔讀 trailer metadata。
- daemon 啟動時對既有 `worktrees/*` 補裝 hook（向下相容）
- `fleet_events.jsonl` 加 `git_event` kind（先不寫 deny，預留 telemetry channel）
- 文件：trailer 格式規範

**Exit criteria**:

- 手動建 worktree + 派任意 dummy task，commit 看到 trailer
- 三個 backend（claude / codex / kiro / gemini 任選兩）都驗證一次
- binding.json 在 task dispatch 後寫入，內容可被 hook 讀

**風險**：極低（不動 git 行為，只多寫 metadata）

---

### Phase 2 — Binding lifecycle + shim binary + deny (合併原 Phase 2/3 binding-shim+原 Phase 4 deny；4–6 天)

**目標**：核心 component + 防禦面一起上線；GC 留給後面 phase。

**Deliverables**:

- `crates/agend-git/` 完整 binary（passthrough / chdir+pass / deny 三 mode）
- `BindingManager` 補完 unbind / reconcile / flock + atomic rename
- `agent.rs`: PATH 注入加 `$AGEND_HOME/bin/`
- daemon 注入 `AGEND_REAL_GIT=$(which git)` env (R12 mitigation, 見 §6/§7)
- shim 完整行為矩陣（含 deny）
- ERROR 訊息統一格式
- deny 事件寫 `fleet_events.jsonl`（Phase 1 已開 channel）
- daemon 偵測連續 N 次同 deny → inbox 通知 lead
- bypass：`AGEND_GIT_BYPASS=1` 全程逃生 + `AGEND_GIT_BYPASS_AGENT=<name>` per-agent + `AGEND_GIT_BYPASS_UNTIL=<epoch>` 絕對 epoch (R1 mitigation 見 §6/§7)

**Exit criteria**:

- bound 模式：agent 從任意 cwd 跑 git，shim 自動 chdir 對 worktree
- unbound 模式：行為跟原本 git 一樣
- 動態切換 binding 立即生效
- shim overhead < 10ms（Rust release build）
- 故意製造 cross-branch checkout 攻擊，全部被擋
- ERROR 訊息對 LLM 可解析
- 連續 deny 觸發 lead 通知
- bypass 三層機制驗證（global / agent / epoch）

**風險**：中（首次動 PATH，但 bypass 三層 + AGEND_REAL_GIT 注入解 recursion）

---

### Phase 3 — Worktree lifecycle (lease/release，cross-platform，3.5–5 天)

**目標**：自動建/接管 worktree，**不含 GC**；同 PR ship Phase 1+2 retro patch 把 unix-only assumption 改 cross-platform。

**Deliverables**:

#### Phase 3 main scope
- `src/worktree_pool.rs` lease / release 實作（GC 移到 Phase 4）
- task dispatch 加 `lease/release` 鉤子
- task done 觸發 release（標候選即可，不刪）
- 啟動時 reconcile 孤兒 worktree（只 log，不刪）
- `.env*` 複製、port offset hash（從 worktree path）
- `worktree.list` MCP tool
- E4.5 enforcement runtime check（reject lease for `main` branch）

#### Phase 1+2 retro patch (cross-platform)
- `src/bin/agend-git.rs` 移除 cfg(unix) gating；`std::os::unix::process::CommandExt::exec` → cross-platform：unix 仍 exec()、Windows 走 `Command::status()` + `process::exit(code)` 等價 status forwarding
- `assets/hooks/prepare-commit-msg.ps1` (PowerShell) — Windows native git hook
- `install_hooks` 平台偵測：unix → bash hook、Windows native git → PowerShell hook
- `symlink_shim` Windows fallback：`fs::copy` 而非 symlink (避 admin 需求)、create `.bat` shim or `.exe` copy
- Path normalization：`std::path::Path` cross-platform OK

**Exit criteria**:

- lead 派 impl task → daemon 自動建 worktree + install hook + bind
- task done → worktree 標候選（保留磁碟）
- 至少跑一次「同一 agent 接續做兩個不同 branch task」整個流程不 respawn
- **Cross-platform CI green：mac / ubuntu / windows 三平台**
- Windows 平台 shim binary 可執行、PowerShell hook 注入 trailer 正確

**風險**：中（Phase 1+2 retro 加 Windows、爆雷面變大；GC 仍 defer Phase 4）

---

### Phase 4 — Worktree GC dry-run + cutover (2 天)

**目標**：把 Phase 3 標候選的 worktree 真的回收，但用 dry-run 雙階段。

**Deliverables**:

- GC 第一輪 **dry-run**：daemon 找出可刪 worktree（候選 + 過 grace TTL + 不在 binding + 不被 pin），只寫 log + 通知 lead，**不實刪**
- 操作員可用 `agend worktree gc-dry-run` MCP tool 觀察會刪什麼
- 觀察 N 天後 enable 第二輪 **actual GC**（feature flag `AGEND_WORKTREE_GC=1`）
- pin 機制：操作員可標 worktree 不被 GC（即使到 TTL）
- only-daemon-tagged invariant：人類手建 worktree 不被回收

**Exit criteria**:

- dry-run mode 列出候選清單正確
- pin 過的不出現在候選
- 人類命名的不出現在候選
- 第二輪實刪只動 daemon-tagged + 過 TTL + 未 pin 的
- 整個 enable / disable 透過 env flag

**風險**：中（R3 風險面，dry-run 雙階段是緩解）

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

> **Amendment 2026-05-05**：補 R11–R14 + R1/R3 緩解強化（per team review synthesis）。

| ID | 風險 | 嚴重度 | 緩解 |
|---|---|---|---|
| R1 | shim panic 卡死整個 fleet 的 git | 高 | (a) `AGEND_GIT_BYPASS=1` 全程逃生（人工硬逃） (b) `AGEND_GIT_BYPASS_AGENT=<name>` per-agent scope (c) `AGEND_GIT_BYPASS_UNTIL=<epoch>` 絕對 epoch、daemon 設時算 now+ttl、shim 比 now（消 time-of-use drift） (d) **shim health-check + auto-bypass fallback**：shim 自偵 panic 過 N 次 / binding.json 反復解析失敗 → 自動 bypass，不靠人工 set env (e) Phase 2 上線前 fuzz test |
| R2 | binding.json 並發寫 race | 中 | flock + atomic rename + daemon 是唯一 writer |
| R3 | Worktree GC 刪到人類正在用的 | 高 | (a) **only-daemon-tagged 才回收**：人類手建 worktree 永不被 GC (b) **grace TTL** 緩衝 (c) 操作員可 **pin** 標記不被 GC (d) **dry-run 雙階段**（Phase 4）：第一輪只 log + 通知 lead，operator review 後第二輪才實刪；feature flag `AGEND_WORKTREE_GC=1` |
| R4 | hook fail 阻擋 commit | 中 | hook 永遠 `exit 0`，trailer 失敗只 log |
| R5 | 弱 backend 不理解 ERROR 訊息持續 retry | 中 | daemon 偵測連續 deny 通知 lead |
| R6 | shim 性能不夠 | 低 | Rust release + 不每次重讀 binding（mtime cache） |
| R7 | 跨 backend 行為不一 | 中 | Phase 2 結束跑 cross-backend smoke test |
| ~~R8~~ | ~~Windows 不支援~~ | — | **撤回 2026-05-06**：Windows 已納入 scope（Phase 3 + Phase 1/2 retro patch per general m-16）。見 R15。 |
| R9 | 既有 worktree 跟新命名衝突 | 中 | reconcile 邏輯保留人類命名；只接管自動建立的 |
| R10 | binding.json 路徑被 agent 推測並偽造 | 低 | 只有 daemon 有寫權限，shim 唯讀 |
| **R11** | **Hook drift 跨 worktree 版本不一致** | 中 | `core.hooksPath` 統一目錄（daemon 一次更新全 worktree pick up）；不用 per-worktree copy mode |
| **R12** | **Shim recursion**（shim 在 PATH 第一順位 → fork plain `git` 無限遞迴） | 高 | daemon spawn 時注入 `AGEND_REAL_GIT=$(which git)` env（pattern matches existing AGEND_HOME / AGEND_INSTANCE_NAME injection at agent.rs:344）；shim 讀 env first；fallback `which`-strip excluding `$AGEND_HOME/bin/` if env missing |
| **R13** | **Multi-platform PATH priority**（macOS / Linux / WSL / Windows git resolution 順序差） | 中 | shim 不依賴 PATH 解析自身路徑（用 `AGEND_REAL_GIT` env or absolute path）；cross-backend smoke test 涵蓋四平台 |
| **R14** | **既有 human-managed worktree 命名衝突誤判** | 中 | daemon reconcile 用 daemon-tagged metadata（如 `binding.json` 紀錄 + worktree name pattern `<agent>-<branch-sanitized>`）區分；只接管 daemon-tagged，human-managed 保持中立 |
| **R15** | **Windows hook engine 差異**（PowerShell vs bash） | 中 | daemon `install_hooks` 平台偵測：unix → bash hook、Windows native git → PowerShell hook（`.ps1`）；git Bash on Windows 也可走 bash hook、靠 git 原生 hook resolution。Cross-platform symlink → file copy fallback (Windows symlink 需 admin、避用)。 |

---

## 7. 開工 Checklist

> **Amendment 2026-05-05**：補 alignment items 1–3（per operator counter-proposal m-73 + dev m-77 + reviewer m-78 confirmed align）。

實作開始前要確認：

### Resolved (本 amendment 拍定)

- [x] **shim binary 放 `$AGEND_HOME/bin/`**（避免污染 build dir）；workspace 內 `crates/agend-git` 用 `[[bin]]`、daemon 啟動 symlink `$AGEND_HOME/bin/git → target/release/agend-git`
- [x] **`crates/agend-git` 在 workspace** 共享 cargo lockfile（matches existing `agend-mcp-bridge` pattern）
- [x] **hook 安裝策略：`core.hooksPath` 統一目錄**（unified update path、不用 per-worktree copy）
- [x] **shim ERROR 走 stderr**（與 git 原生一致）
- [x] **R12 shim recursion fix**：daemon 注入 `AGEND_REAL_GIT=$(which git)` env at spawn time（pattern matches existing AGEND_HOME / AGEND_INSTANCE_NAME injection at agent.rs:344）；shim 讀 env first，fallback `which`-strip excluding `$AGEND_HOME/bin/`
- [x] **Bypass mechanism 三層**：
  - `AGEND_GIT_BYPASS=1`：全 process 硬逃生（人工 emergency override）
  - `AGEND_GIT_BYPASS_AGENT=<name>`：per-agent scope
  - `AGEND_GIT_BYPASS_UNTIL=<epoch>`：絕對 epoch（daemon 設時算 now+ttl，shim 比 now）— 消 time-of-use drift
- [x] **Phase 1 binding.json minimal writer**：option (a)—Phase 1 ship minimal writer（task_id + branch + agent_name + issued_at），lifecycle manager 移到 Phase 2。Cross-backend principle 維持（option b 需 backend-fragmented per-task env mechanism、會破）。

### Open（仍要決定）

- [ ] §10.4 既有 worktree 命名 `<agent>-<repo-path>` vs 新提議 `<agent>-<branch>` 是否衝突 → 決定要不要改命名（搬到 Phase 3 worktree lifecycle 才需要 finalize）
- [ ] task board 結構是否有「task kind」欄位區分 impl/review/plan？沒有要加（dev m-67 確認 current TaskEntry 沒這欄位、Phase 1 不需、defer 到 Phase 2 整合 dispatch 時加）
- [ ] 既有 `worktrees/` 下的人類手建 worktree 怎麼處理（Phase 3 reconcile 規則）— R14 緩解：daemon-tagged-only invariant
- [ ] AGEND_HOME 注入是否在所有 backend 都 OK（已確認 claude，其他要驗）— Phase 1 cross-backend smoke test 一併驗
- [ ] 是否要支援 `agend git override` 操作員 CLI（用於緊急 debug）— Phase 4 + dry-run GC 已部份覆蓋此用途，視 Phase 4 觀察期 feedback 決定

---

## 8. 預估工時

> **Amendment 2026-05-05**：Phase 重組後重估；Phase 1 加 minimal binding writer + telemetry；原 Phase 2 binding+shim 跟原 Phase 4 deny 合併；Phase 3 拆出 lease/release，GC 獨立成 Phase 4 dry-run cutover。
>
> **Amendment 2026-05-06 (general m-16)**：Phase 3 加 Windows + Phase 1/2 retro patch、+1.5-2 天。

| Phase | 樂觀 | 悲觀 |
|---|---|---|
| Phase 1 trailer + minimal binding writer + telemetry | 1.5 天 | 2 天 (✓ shipped) |
| Phase 2 binding lifecycle + shim binary + deny + bypass 三層 + AGEND_REAL_GIT | 4 天 | 6 天 (✓ shipped) |
| **Phase 3 worktree lease/release + Phase 1/2 retro patch (Windows)** | **3.5 天** | **5 天** |
| Phase 4 worktree GC dry-run + cutover | 1.5 天 | 2 天 |
| Phase 5 hotspot（選做） | 2 天 | 3 天 |
| **合計** | **12.5 天** | **18 天** |

不含跨 backend smoke test（每 phase 結束加 0.5 天）。Phase 4 dry-run 觀察期另計（建議 N 天觀察後 cutover）。

---

## 9. Out-of-scope（明確排除）

> **Amendment 2026-05-06 (general m-16)**: Windows native support 已從 out-of-scope 撤回、納入 Phase 3 + Phase 1/2 retro patch。R8 撤回、新增 R15 (Windows hook engine)。

- mrq-style 連續快照（補 commit-之間的縫，獨立 product）
- bash 整體劫持（safeexec 範疇）
- ~~Windows native 支援~~（**撤回**：已納入 scope per general m-16，cross-platform 從 Phase 3 開始）
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
