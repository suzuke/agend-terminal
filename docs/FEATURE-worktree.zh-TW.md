[English](FEATURE-worktree.md)

# Worktree 管理

AgEnD 以 daemon 管理的 Git worktree 隔離帶有分支的工作。本文件說明目前的建立、綁定、受保護釋放與自動釋放行為。

## 使用情境

> **目標讀者：** agent 基礎設施。Agent 通常由任務派送取得已綁定的 worktree，並在其中使用一般 Git 指令。

- **新的分支任務：** 派送時指定分支，或呼叫 `repo action=checkout` 並設 `bind:true`，建立並綁定專屬 worktree。
- **復原或重新綁定：** 現有 agent 需要復原或重建自身綁定時使用 `bind_self`。
- **正常清理：** 工作已可安全回收後呼叫 `release_worktree`。
- **依路徑清理：** 已知 linked-worktree 路徑時使用 `repo action=release`。
- **自動清理：** PR／任務生命週期進入終態時會寫入 durable release intent；daemon 只在目前 lease 仍符合釋放不變量時執行。

## 狀態與目錄配置

由 branch dispatch 建立的 worktree 使用標準 pool 配置：

```text
$AGEND_HOME/worktrees/<instance>/<branch>/
```

明確呼叫 `repo action=checkout` 時，目前會在 `$AGEND_HOME/worktrees/` 下使用穩定的
單層 `<instance>-<repository-key>` 目錄。Daemon 回傳的 `worktree` 路徑與
`binding_state` 才是權威來源；caller 不得自行推導或寫死任一配置。

兩類狀態共同建立權威性：

- worktree 內的 `.agend-managed` 證明它由 daemon 管理。
- `runtime/<instance>/binding.json` 記錄 instance、分支、來源 repository、worktree 路徑、任務與 lease identity。

舊版目錄可能為復原而被辨識，但新 worktree 一律使用標準配置。沒有 managed marker 的 operator 自建 worktree，不會被當成一般 daemon 所有的釋放目標。

## 建立與綁定

### 新工作：`repo action=checkout`

```json
{
  "tool": "repo",
  "action": "checkout",
  "repository_path": "/path/to/repo",
  "branch": "feature/example",
  "bind": true
}
```

設為 `bind:true` 時，AgEnD 會在同一個生命週期操作中建立分支 worktree，並寫入 marker 與 binding。受保護分支會被拒絕。若已知來源 repository 路徑，這是新任務的首選入口。

設為 `bind:false` 時，checkout 只建立未綁定的檢查用 worktree；它不是 dispatch lease。

### 可拋棄的 review checkout

需要完整 worktree review 時，請使用 typed disposable provenance，避免留下普通 review branch：

```json
{
  "tool": "repo",
  "action": "checkout",
  "repository_path": "/path/to/repo",
  "branch": "review/2819-r0",
  "from_ref": "<完整 subject-head SHA>",
  "expected_head": "<相同的完整 subject-head SHA>",
  "bind": true,
  "task_id": "t-...",
  "checkout_purpose": "disposable_review"
}
```

`disposable_review` 只接受能由這次 checkout 證明在本地與 `origin` 都是新建的
branch。最初的 signed binding 會直接記錄 `DaemonProvisionedReview` provenance
與精確 `provisioned_head`，不存在事後補 metadata 的空窗。

只有在相符 review task 已終態、沒有其他 binding 持有、沒有 PR 以該 review
branch 為 head，而且實際 tip 仍等於 `provisioned_head` 時，release 才能刪除乾淨
review branch；subject PR 可以仍是 open。Dirty work、divergence、遺失／損壞
provenance、未知 task state，或無法證明 remote branch 狀態時一律 fail closed 並保留。

### 復原：`bind_self`

```json
{
  "tool": "bind_self",
  "repository_path": "/path/to/repo",
  "branch": "feature/example",
  "task_id": "t-...",
  "rebase_mode": true
}
```

`bind_self` 只會綁定呼叫者本身。適用於復原、安全重新綁定，或從 fleet 設定解析 repository 的情境。`repository_path` 與舊版 `repository` 參數互斥；請優先使用 `repository_path`。

`rebase_mode:true` 會先走受保護的修復／重新綁定路徑；它不授權覆寫其他仍有效的 lease。

`bind_self` 與自行 claim 的 checkout 都不會默默創造 CI continuation。CI watch 會由真正帶分支的任務派送啟用，或由明確的 `ci action=watch` 呼叫建立。

## 綁定保證

- 建立前會驗證分支名稱與 repository 路徑。
- 受保護分支不能成為一般 agent lease。
- 分支已被其他 agent 租用時會回報衝突，不會隱式接管。
- 綁定與生命週期操作會序列化，避免 bind、rebase、release 任意競速。
- 新的派送應攜帶 `task_id`；自動生命週期釋放只適用於具有非空 task ID 的 dispatch lease。

操作 worktree 前，可用 `binding_state` 檢查權威綁定。

## 正常釋放

```json
{
  "tool": "release_worktree",
  "instance": "dev-agent"
}
```

目前的 MCP release 是受保護的硬釋放；正常流程中沒有 24 小時「soft release」階段。

釋放交易會：

1. 取得 lifecycle permit；
2. 快照 guarded binding；
3. 重新取得 branch、agent 與 binding locks；
4. 確認最新 binding fingerprint 仍與快照一致；
5. 移除前保存 dirty work；
6. 驗證 managed marker 與精確目標；
7. 移除 linked worktree、prune Git metadata，並清除相符的 binding。

若保存失敗，釋放會 fail closed 並保留 binding。Lease fingerprint 已變更也會停止操作。成功釋放後再次呼叫會得到 idempotent success。

可使用 `dry_run:true` 預覽，不產生破壞性效果。

移除實作會先請 Git 移除 worktree。只有在 guarded transaction 已確認目標確實是該 daemon-managed worktree 後，才允許使用檔案系統 fallback。

## 緊急釋放

`release_worktree(force:true)` 是吸收舊 standalone force-release 工具後的受保護復原路徑。

```json
{
  "tool": "release_worktree",
  "instance": "dev-agent",
  "branch": "feature/example",
  "force": true,
  "repository_path": "/path/to/repo"
}
```

- `branch` 必填。
- 呼叫者必須是 worktree owner、其 team orchestrator，或 operator。
- 目標必須解析到 daemon worktree pool 之下。
- 對 markerless、opaque、ambiguous、ownerless 或不相符的狀態會保留，而不是猜測後刪除。
- `repository_path` 是可選的 Git metadata 清理提示。

Force 用於陳舊狀態復原，不是繞過正常釋放檢查的捷徑。

## `repo action=release`

`repo action=release` 接收 worktree 路徑，並在執行當下重新驗證。

- daemon-managed 目標會依 marker owner、binding fingerprint、精確路徑檢查與 caller authorization，委派給標準 guarded release。
- 未受管理的目標，只有在 Git 證明它是 linked、non-bare worktree 時才符合資格。
- main worktree、bare repository、非 repository、過淺／系統路徑、陳舊 managed marker 與 ambiguous target 都會被拒絕。
- 直接移除 fallback 只會在未受管理的目標重新確認為該精確 linked worktree 後使用。

此 action 以路徑為中心，但不是不受限制的目錄刪除 API。

## 自動釋放

Merge、close、task completion 與符合條件的 reviewer verdict 事件會寫入磁碟上的 recompute intent。Worker 只處理帶有 task ID 的 dispatch lease，且 live lease 必須仍與捕捉到的 identity 一致，包括來源 repository、branch、path 與 issue time。

核心釋放不變量是：

```text
PR 已進入終態
或
已正向確認沒有 PR，且該 repository／branch 的所有相符任務皆為終態
```

PR 開啟中或狀態未知時會保留 worktree。未 merge 而關閉的 PR 依保守 grace 規則處理。其他 repository 的證據不能滿足此 lease。

自動釋放也會：

- 遵守 worktree release opt-out；
- 在釋放符合終態條件的 dirty lease 前保存 WIP，保存失敗則稍後重試；
- 保留尚未終態或不符合條件的 intent，之後重新計算；
- 在釋放前比對精確 lease fingerprint，避免刪除已重新租出的 worktree；
- 只針對符合條件、乾淨的 reviewer binding，在 review task 或 verdict 已終態時使用狹窄的 reviewer-cleanup 路徑。

Typed `disposable_review` binding 會走上方更嚴格的 provenance 路徑。Subject PR
不是 branch-lifecycle signal；必須由 review task 終態，加上精確 provenance／tip／
occupancy 檢查共同授權。

PR-state 通知與 release worker 彼此解耦；consumer 不應假設釋放一定在 `pr-merged` 訊息之前立即完成。

## 分支清理

Worktree 移除與 local branch 刪除是兩個獨立決策。Branch cleanup 會跳過受保護分支；merge 或 lifecycle 證據不足時 fail closed。乾淨分支若尚不能刪除，可能會記錄 cleanup intent。

## 操作檢查表

1. 以帶分支的派送或 `repo action=checkout bind:true` 開始新工作。
2. 確認 daemon 回報的 worktree，並在該目錄工作。
3. 完整 worktree review 應在新 branch 上加入 `checkout_purpose:"disposable_review"`、精確 `expected_head` 與 review `task_id`。
4. 只有復原／重新綁定時才使用 `bind_self`。
5. 狀態不確定時，用 `binding_state` 或 `release_worktree dry_run:true` 檢查。
6. 已獲授權清理時使用正常 `release_worktree`。
7. 只在 guarded recovery 時使用 `force:true`，並提供精確分支。
8. Binding 仍有效時，切勿手動刪除 managed directory。

## 原始碼位置

- `src/mcp/handlers/worktree.rs` — `bind_self` 與 `release_worktree`
- `src/mcp/handlers/ci/release.rs` — `repo action=release`
- `src/mcp/handlers/ci/checkout_disposable.rs` — typed disposable-review admission
- `src/worktree.rs` — 標準 dispatch-worktree 路徑推導
- `src/worktree_pool.rs` — lease、guarded release、WIP preservation 與 branch cleanup
- `src/daemon/auto_release.rs` — durable auto-release intents 與 release invariant
- `src/binding.rs` — binding records 與 guarded fingerprints
