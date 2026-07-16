[English](FEATURE-ci-watch.md)

# CI Watch — 自動 PR CI 監看

CI Watch 會針對 repository 與 branch 輪詢 forge CI provider、記錄 PR／CI 狀態，並把終態結果送給 subscribers 與可選的後續 agents。

## 使用情境

> **目標讀者：** agent 基礎設施。Agent 通常由任務派送自動取得 feature-branch watch，或透過 MCP 明確建立。

- **CI 到 review handoff：** branch task 帶有 `next_after_ci`；目前 head 通過後，daemon 會把 `[ci-ready-for-action]` 送給指定的一個或多個 target。
- **Subscriber notification：** 每位 subscriber 都會收到 informational pass/fail update，但不會增加 provider polling 次數。
- **衝突預警：** mergeability 轉為 conflicting 時會送出 `[ci-conflict-detected]`。
- **Merge 後驗證：** 經授權的 exact-head watch 可鎖定 `main`／`master` 上某個 immutable commit，後來的 push 不會誤完成原本的任務。

CI Watch 會評估 selected head 所回傳的所有最新 CI runs。MCP schema 沒有公開 `required_checks` filter。

## 建立 Feature-Branch Watch

```json
{
  "tool": "ci",
  "action": "watch",
  "repository": "owner/repo",
  "branch": "feat/my-feature",
  "next_after_ci": ["reviewer-a", "reviewer-b"],
  "task_id": "t-...",
  "interval_secs": 60
}
```

| 參數 | 必填 | 說明 |
|---|---|---|
| `repository` | 視情況 | Forge repository slug。`watch` 可從 caller binding 推導；否則必須明確提供。 |
| `branch` | 否 | 要監看的分支；預設 `main`，但一般 protected-branch watch 會被拒絕。 |
| `interval_secs` | 否 | 基準 polling interval；預設 60 秒。 |
| `next_after_ci` | 否 | CI 成功後接收 `[ci-ready-for-action]` 的單一 instance 或 array。 |
| `task_id` | 否 | 複製到 CI handoff 的 durable back-link。 |
| `review_class` | 否 | PR readiness 的 `single` 或 `dual` review threshold。 |
| `ci_provider` | 否 | Provider override，通常為 `github` 或 `bitbucket_cloud`；`bitbucket_server` 會被拒絕。 |
| `ci_provider_url` | 否 | 自訂 provider base URL；credentials 只會送到 trusted host。 |
| `head_sha` | 僅 protected ref | Exact-head protected-ref watch 的完整 40/64-hex SHA。Feature branch 會忽略。 |

參數名稱是 `repository`，不是 `repo`。

對相同 key 再次呼叫 `watch` 具 append-idempotency：會保留 poll state 與其他 subscribers，並在需要時加入 caller。它也會更新指定的 interval/provider 設定，並清除先前 explicit-unwatch tombstone。

## Unwatch 與 Status

```json
{
  "tool": "ci",
  "action": "unwatch",
  "repository": "owner/repo",
  "branch": "feat/my-feature"
}
```

`unwatch` 必須明確提供 `repository`。它只移除 caller 的 subscription、resolve 該 caller 相符的 CI-handoff obligation，並保留 co-subscribers。移除最後一位 subscriber 時，daemon 會保留 opt-out tombstone 而不是刪檔，避免自動 re-arm；直到 PR 進入終態或有人明確再次呼叫 `watch`。

```json
{
  "tool": "ci",
  "action": "status",
  "repository": "owner/repo",
  "branch": "feat/my-feature"
}
```

兩個 status filter 都是可選的。結果會顯示持久化 watch 與最新 polling diagnostics。

釋放 worktree 不會隱式取消 CI Watch。Watch 的生命週期由 terminal state、TTL、明確 `unwatch` 與自身 cleanup 規則管理。

## Dispatch 自動啟用

真正帶有 feature `branch` 的 `send` task dispatch，會在建立 dispatch lease 時自動啟用 CI Watch。`next_after_ci` 可省略：省略時 subscriber 仍收到 informational CI result，但系統不會依名稱或 role 猜測額外 handoff target。

```json
{
  "tool": "send",
  "instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "branch": "feat/my-feature",
  "next_after_ci": "reviewer",
  "message": "Implement the task and open a PR"
}
```

用於 self-claim／recovery 的 `bind_self` 與 `repo action=checkout bind:true` 不會默默啟用新 watch。這些流程需要監看時，請明確呼叫 `ci action=watch`。

## Protected Ref Exact-Head Watch

一般 `main`、`master` 等 protected ref watch 仍會被拒絕。狹窄的 post-merge 例外會鎖定單一 immutable GitHub SHA：

```json
{
  "tool": "ci",
  "action": "watch",
  "repository": "owner/repo",
  "branch": "main",
  "head_sha": "0123456789abcdef0123456789abcdef01234567",
  "task_id": "t-...",
  "next_after_ci": "release-owner"
}
```

以下條件全部必須成立：

1. `head_sha` 是完整 40 或 64 hex commit ID，不是縮寫。
2. `task_id` 非空。
3. `next_after_ci` 明確且非空。
4. Provider 是 GitHub。
5. Caller 是 operator，或每個 target 所屬 team 的 orchestrator。

Sidecar key 包含 repository、branch 與 SHA。Poller 只查詢該 SHA 的 runs，因此較新的 `main` run 不會誤完成 pinned episode。

## Polling 與結果聚合

每個 watch 會持久化在 `$AGEND_HOME/ci-watches/` 下。Daemon 會盡可能依 repository 將到期 watches 分組、輪詢一次，再把結果 fan out 給 subscribers。

每個 head 會選取各 workflow 最新 attempt，並推導 aggregate terminal result。Pending run 會讓 watch 繼續存在。Feature branch head 改變時會重設 run tracking；notification 依 immutable head 與 terminal episode 去重。

設定的 interval 會依 provider quota 調整：

| 剩餘 quota | 有效 interval |
|---|---:|
| 超過 50% | 基準的 1× |
| 10%–50% | 基準的 2× |
| 10% 以下 | 基準的 4× |

Multiplier 上限為 4×。沒有可用 quota header 的 provider 維持基準 interval。

Repository-level rate-limit/provider skip 連續三次後，subscriber 會收到 `[ci-watch-stalled]`。之後輪詢成功時會送出 `[ci-watch-resumed]`。

## Subscribers 與投遞

- 多個 instance 共用一個 watch 與一組 poll stream。
- Subscriber 同時不存在於 runtime registry 與 fleet roster 時會跳過投遞。
- 適用時，同一 delivery class 較新的 branch notification 會 supersede 較舊 pending row。
- `next_after_ci` 產生 action handoff；一般 subscriber 收到 informational CI event。
- Terminal exact-head watch 在 pinned run 到達終態後移除。

`send` 的 `triaged:{head,job,reason?}` 目前會記錄 durable triage ledger entry，且 `head` 與 `job` 必須同時提供。該 ledger 現階段是 audit／data-layer surface；尚未承諾所有重複 notification path 都會被 suppression。

## 衝突偵測

建立 watch 與之後 polling 時，若 provider 支援，daemon 會檢查 PR mergeability。狀態轉為 `CONFLICTING` 時會對 subscribers 送出 `[ci-conflict-detected]`。Mergeability 未知時會 fail safe，不會捏造 conflict result。

## 生命週期與 Cleanup

- Subscription 會更新 72 小時 expiry。
- Terminal inactivity 也使用相同 72 小時 cleanup window。
- 七天 absolute age cap 防止持續被更新的 leaked watch。
- Startup sweep 會移除 daemon 停止期間已過期的 watch。
- Terminal PR／CI path 可能更早移除 watch。
- 明確 `unwatch` 只移除 caller；最後一位 subscriber 被移除後會留下不 polling 的 opt-out tombstone，直到 terminal cleanup、re-watch 或 tombstone age backstop。

## Provider 與 Credential 規則

Provider detection 支援 GitHub、GitLab 與 Bitbucket Cloud。明確指定 `bitbucket_server` 目前會被拒絕。Exact-head protected watch 僅支援 GitHub。

GitHub token discovery 順序是：

1. `GITHUB_TOKEN`；
2. 已驗證的 `gh` CLI（先 `gh auth status`，再 `gh auth token`）；
3. unauthenticated access。

`GH_TOKEN` 不在此 discovery chain。Discovery 在每個 daemon process 只快取一次，因此 `gh auth login` 或 token rotation 後請重啟 daemon。沒有 token 時，`watch` 會回傳 `setup_warning`；GitHub 一般 unauthenticated 額度約為每小時 60 次，authenticated 則為每小時 5,000 次。

自訂 `ci_provider_url` 的 credentials 只會送到 trusted HTTPS SaaS host、loopback，或由 `AGEND_CI_TRUSTED_HOSTS` 明確允許的 host。未受信任的 custom host 會以不帶 forge token 的方式 polling 並產生 warning，而不會收到 credentials。

## 原始碼位置

- `src/mcp/handlers/ci/watch.rs` — watch／unwatch validation、persistence、subscriber removal 與 opt-out tombstone
- `src/daemon/ci_watch/` — polling、providers、aggregation、delivery、stall detection 與 cleanup
- `src/github_token.rs` — GitHub credential discovery 與 cache
- `src/mcp/handlers/dispatch_hook/` — task-dispatch auto-arm
