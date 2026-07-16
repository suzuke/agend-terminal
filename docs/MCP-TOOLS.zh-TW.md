[English](MCP-TOOLS.md)

# AgEnD MCP Tools Reference — 工具參考（32 個工具）

Daemon registry 與即時 `tools/list` schema 才是權威來源。依 instance role 不同，實際顯示的工具可能是這 32 個已註冊工具的子集。

## 動作型工具（Action-based Tools）

### `task`

管理 task board。動作：`create`、`list`、`get`、`claim`、`done`、`update`、`sweep`、`health`、`activity`、`metadata_set`、`metadata_get`、`ack_plan`。

- 主要欄位包括 `id`／`task_id`、`title`、`description`、`assignee`、`priority`、`status`、`branch`、`depends_on`、`result`、`due_at`、`project` 與 `scope`。
- `list` 預設只回傳可執行任務；用 `include_history:true` 納入 done/cancelled 任務，並可用 `filter_status`、`filter_assignee` 等條件縮小範圍。
- `list` 預設為 terse。用 `verbose:true` 取得完整文字，或用 `fields:"minimal"` 只取精簡 identity/status 投影。
- `get` 依 `id` 或 `task_id` 回傳一筆完整記錄。
- Metadata 與 plan-ack action 會操作 durable task record；必填鍵請以即時 schema 為準。

### `decision`

管理 durable decision 與 operator question。動作：`post`、`list`、`update`、`answer`。

- Decision 欄位：`id`、`title`、`content`、`tags`、`scope`、`supersedes`、`archive`、`include_archived`、`ttl_days`。
- Question 使用 `needs_answer`、`options`、`allow_free_text`、`timeout_secs` 與 `timeout_default`；`answer` 記錄選項或自由文字答案。

### `team`

管理 team。動作：`create`、`delete`、`list`、`update`。

- 欄位：`name`、`members`、`orchestrator`、`description`、`repository_path`、`project_id`、`accept_from`、`add`、`remove`。
- `project_id` 覆寫 project-board 推導；`accept_from` 是跨 team sender allowlist。

### `schedule`

管理定時投遞。動作：`create`、`list`、`update`、`delete`。

- 欄位：`id`、`label`、`instance`、`message`、`cron`、`run_at`、`timezone`、`enabled`。
- `list` 預設回傳最新三筆 history 與 `runs_total`；設 `full_history:true` 可取回最多 50 筆保留記錄。
- `fire_strategy` 可為 `always` 或 `until_success`；後者必須搭配 `linked_task_id`。

### `deployment`

管理批次 deployment。動作：`deploy`、`teardown`、`list`。

- 欄位：`name`、`template`、`branch`、`directory`。

### `ci`

管理 CI watch。動作：`watch`、`unwatch`、`status`。

- 欄位：`repository`、`branch`、`interval_secs`、`next_after_ci`、`review_class`、`ci_provider`、`ci_provider_url`、`task_id`、`head_sha`。
- 使用 `repository`（GitHub `owner/repo`），不是 `repo`。`watch` 可從 caller binding 推導；`unwatch` 必須明確提供。
- 一般 `main`／`master` watch 會被拒絕。Protected ref exact-head watch 需要完整 40/64-hex `head_sha`、`task_id`、明確 `next_after_ci`、GitHub，以及已授權的 orchestrator/operator caller。

### `repo`

管理 repository worktree、branch cleanup 與 PR merge。動作：`checkout`、`release`、`cleanup_init_commits`、`cleanup_merged_branches`、`merge`。

- 常用欄位包括 `repository_path`、`repository`、`branch`、`path`、`instance`、`bind`、`task_id`、`expected_head` 與 `checkout_purpose`。
- `checkout bind:true` 會建立並綁定；`bind:false` 建立檢查用 worktree。
- `checkout_purpose:"disposable_review"` 會建立 typed review provenance；它要求 `bind:true`、非空 `task_id`、完整 `expected_head`，而且 branch 必須能證明在本地與 `origin` 都是新建的。
- `cleanup_merged_branches` 預設為 dry-run；實際套用時需要 `confirm_ids` 與 `audit_reason`。
- `merge` 使用 `pr`；`force:true` 需要 `force_reason`，並會留下 audit。

### `health`

管理 blocked health state。動作：`report`、`clear`。

- `report` 使用 caller identity，接受 `reason`（`rate_limit`、`quota_exceeded` 或 `awaiting_operator`）、可選 `retry_after_secs` 與 `note`。
- `clear` 需要目標 `instance`；可選 `reason` 可限制要清除的 blocked reason。

## 通訊（Communication）

### `send`

傳送給單一 instance 或廣播。這是統一的 inter-agent messaging 工具。

- 必填：`message`。以 `instance`、`instances`、`team` 或 `tags` 其中一種路由。
- `request_kind`：`query`、`task`、`report` 或 `update`；typed report 應設定 `report_purpose`。
- Task 欄位包括 `task_id`、`success_criteria`、`context`、`branch`、`bind`、`worktree_binding_required`、`eta_minutes`、`reporting_cadence`、`expect_reply_within_secs` 與 `next_after_ci`。
- Broadcast task dispatch 必須帶既有 `task_id`。目前 single-target 相容路徑可在省略時自動建立，但穩定契約是先 `task action=create`，再明確傳入 `task_id`。
- Thread／correlation 欄位：`correlation_id`、`parent_id`、`thread_id`。
- Busy／review 欄位包括 `force`、`force_reason`、`second_reviewer`、`second_reviewer_reason`、`review_class`、plan-ack 欄位、typed review-assignment 欄位、`reviewed_head` 與 `artifacts`。
- Report control 包括 `terminal`、`ack_inbox` 與 `triaged`；fire-and-forget task 可使用 `no_report_expected`。

### `inbox`

Drain 或管理 caller 的 durable inbox。

- 不帶參數會 drain unread 訊息並標為 `delivering`；此時尚未標為 processed。
- `message_id` 描述單一訊息；`thread_id` 取得 thread。可選 `instance` 可限定已授權查詢範圍。
- `action:"ack"` 確認一個 delivering `message_id`；省略 ID 時確認整批 in-flight batch。
- `action:"clear"` 精簡清除非 obligation 訊息，未回答的 query/task 仍維持 unread，並列在 `requires_response`。
- `action:"discharge"` 需要 `message_id` 與非空 `reason`；它會在不回答的情況下關閉 channel-reply obligation，並通知 operator。
- 再次 drain 會隱式 ack 前一批 delivery；未確認的 batch 約十分鐘後可能被 reclaim 並重新投遞。

### `reply`

透過外部 channel 回覆 user/operator；不要用於 agent 之間的通訊。

- 必填：`message`。
- `message_id` 會依原始 inbox message 的 channel 路由，傳送成功後 settle 該列。
- 可選 `task_id` 與 `correlation_id` 保留 reply-to correlation。
- `default_action` 應搭配 `timeout_secs`，以記錄有 timeout default 的 decision。

### `download_attachment`

下載 Telegram multimedia attachment 並回傳本機路徑。

- 必填：`file_id`。

## Instance 生命週期（Instance Lifecycle）

### `create_instance`

建立單一 instance，或同質／異質 team。

- 欄位包括 `name`、`backend`、`model`、`model_tier`、`args`、`working_directory`、`branch`、`task`、`role`、`env`、`topic_binding`、`team`、`count`、`backends`、`layout` 與 `target_pane`。

### `delete_instance`

停止並移除 instance。

- 必填：`instance`。Creator-path 若要刪除仍有 in-flight work 的 instance，還需要 `force:true` 與非空 `force_reason`；override 會留下 audit。

### `start_instance`

啟動已停止的 instance。

- 必填：`instance`。

### `restart_instance`

重啟 instance。

- 必填：`instance`；可選 `mode`（`resume` 或 `fresh`）、`reason` 與 `force`。
- `resume` 是預設值，保留 backend conversation state。
- `fresh` 從乾淨狀態啟動；bound worktree 有 dirty changes 時會拒絕，除非明確傳 `force:true`。

### `set_model`

為 instance 持久化恰好一種 model intent（`model` 或 `tier`）；設定一方會清除另一方。`restart:true` 立即套用，否則下次 respawn 生效。

- 必填：`instance`，以及 `model`／`tier` 恰好一個。

### `bind_topic`

建立 deferred／eligible Telegram topic binding。

- 必填：`instance`；可選 `channel` 目前預設為 `telegram`。
- 已綁定時為 idempotent no-op；`skip` mode 不符合資格。

### `list_instances`

列出作用中的 instance，或傳 `instance` 取得詳細資料。輸出預設 compact；`verbose:true` 或 `include_evidence:true` 會包含 observed-status evidence。回應也會顯示 operator mode。

### `set_metadata`

設定 caller 的顯示 metadata。動作：`display_name`、`description`。

- `display_name` 使用 `name`；`description` 使用 `description`。

### `set_waiting_on`

宣告 caller 目前等待的 condition；傳空 `condition` 可清除。

### `interrupt`

向目標 PTY 傳送 ESC。

- 必填：`instance`；可選 `reason` 與 `snapshot`。設 `snapshot:true` 可回傳 ESC 後的 diagnostic snapshot。

### `move_pane`

把 instance pane 移到 TUI tab。

- 必填：`instance`、`target_tab`；可選 `split_dir`（`horizontal` 或 `vertical`）。

### `pane_snapshot`

讀取已移除 ANSI 的 PTY scrollback。

- 必填：`instance`；可選 `lines`、`head` 與 `to_file`。
- `to_file:true` 把完整 capture 存到 `$AGEND_HOME/captures/`，並只回傳精簡結果。

### `instance`

唯讀 folded alias。動作：`list`、`pane_snapshot`；語意與上述 standalone tools 相同。

## Worktree 與 Binding（Worktree & Binding）

### `bind_self`

將 caller 復原或重新綁定到 branch worktree。新工作請優先使用 `repo action=checkout bind:true`。

- 必填：`branch`；可選 `repository_path`、與它互斥的 legacy `repository`、`rebase_mode` 與 `task_id`。
- 受保護分支與跨 agent lease conflict 會被拒絕；它不會默默建立 CI continuation。

### `release_worktree`

以 guarded transaction 釋放精確的 daemon-managed worktree 與 binding。正常路徑會保存 WIP 並檢查最新 binding fingerprint；成功後具 idempotency。

- 必填：`instance`；可選 `dry_run` 與 `force`。
- `force:true` 還需要 `branch`；`repository_path` 是可選 cleanup hint。Markerless、opaque、ambiguous 或不相符狀態會被保留。

### `binding_state`

非破壞性回報 binding 內容、worktree／marker 狀態、signature diagnostics、CI subscriptions、in-flight guard 與 branch holders。

- 必填：`instance`。

### `revoke_review_assignment`

以精確 CAS identity 撤銷 reviewer assignment。Owning team orchestrator 或 operator 有權執行；重複撤銷具 idempotency。

- 必填：`assignment_id`。

### `usage_limit_takeover`

針對持久化 usage-limit takeover episode 的 operator-only PREPARE 步驟。它會寫入 durable prepared journal，但不執行 takeover。

- 必填：來源 `instance` 與精確 `episode_id`。

## Daemon 操作（Daemon Operations）

### `config`

讀取 runtime configuration。動作：`get`、`list`；MCP 不支援寫入。

- `get` 需要 `key`。
- 目前的 keys：`dev_idle_threshold_secs`、`fleet_idle_threshold_secs`、`fleet_idle_ack_ttl_secs`、`hang_auto_recovery_enabled`、`usage_limit_propagation_enabled`、`idle_watchdog_enabled`、`show_pane_state`、`copy_on_select`、`dim_unfocused_panes`、`observed_badge`、`context_alert_pct`、`context_handoff_pct`、`context_handoff_escalate_pct`。
- 以 `agend-terminal admin config-set <KEY> <VALUE>` 修改值。

### `restart_daemon`

請求 graceful daemon restart。無參數。

- 預設 standalone mode 會 self-respawn successor，等 health gate 通過後正常退出；不需要外部 supervisor。
- 設 `AGEND_RESTART_HANDOFF=0` 時走 legacy mode，以 code 42 退出，並需要已安裝的 service supervisor 或 wrapper；偵測不到時會回報失敗。
- Unix `agend-terminal app` mode 會先 preflight，再以相同 PID 原地 re-exec。成功回覆 prepared 後，連線會在 re-exec 時中斷。
- Windows app mode 維持 fail-closed；請退出後重新啟動。
- Shared gate 最多允許一個 restart in flight；同時到達的另一個請求可重試。

## Bridge 與 daemon proxy 契約

Daemon 是 tool registry、authorization、task state 與 side effect 的唯一權威。
`agend-mcp-bridge` 是 near-zero-state relay；它沒有本地 tool implementation，
也沒有 filesystem fallback。

```text
MCP client
  │ stdin/stdout: newline-delimited JSON-RPC
  ▼
agend-mcp-bridge
  │ authenticated loopback TCP: newline-delimited JSON
  ▼
AgEnD daemon (`/mcp` dispatcher)
```

### Framing 與 authentication

Stdio 與 TCP 都以每行一個 JSON object 傳輸，不支援 `Content-Length` framing。
Bridge 在本地處理 `initialize`、`ping` 與 JSON-RPC notification；完成 active run
directory discovery、建立 persistent loopback connection，並以 daemon cookie 加上
bridge PID 驗證後，才 proxy `tools/list` 與 `tools/call`。

| Boundary | Timeout | 用途 |
|---|---:|---|
| Daemon，authentication 前 | 5 秒 | 限制 idle 或 partial authentication attempt |
| Bridge，等待 daemon response | 120 秒 | 限制卡住的 proxy request |
| Daemon，authentication 後 | 無 session read timeout | 允許長時間 idle 的 MCP session |
| Daemon tool execution | 5 / 30 / 60 秒 | fast、default、slow execution band |

Daemon 約每兩秒檢查一次已驗證的 bridge PID，PID 死亡或 TCP EOF 時關閉 session。

### Request identity、retry 與 execution timeout

每個 proxied request 都會取得 UUIDv4 `request_id`。遇到可重試的 transport
failure 時，最多 reconnect/retry 一次，且沿用同一個 ID；daemon deduplication
使 side effect 保持 exactly-once。Startup discovery 每 100 ms 重試一次、最多
30 秒。Application error 會立即回傳，不會當成 transport failure。

Read-only 或 idempotent operation 超過自己的 5/30/60 秒 band 時，會回傳可重試
timeout。Side-effecting operation 則在背景繼續並回傳 `accepted_in_progress`；caller
必須觀察 task、inbox 或 status surface，不得重送。Bridge 的 120 秒 timeout 只是
transport backstop。

Bridge 只保留 connection，以及一筆 500 ms 內相同且成功的 `tools/call` 結果，
用來吸收緊接而來的 duplicate；failed call 不會寫入該 cache。

### Fail-closed 行為與 source ownership

- startup 時 daemon unavailable：重試 30 秒，之後回傳可見的 JSON-RPC error；
- request 中途斷線：以相同 ID reconnect 並 retry 一次；
- retry 仍失敗或 daemon application error：回傳可見 error；
- bridge exit：daemon 關閉 authenticated session；
- 沒有 daemon：不存在本地或 filesystem execution path。

實作 owner 是 `src/bin/agend-mcp-bridge.rs`（framing、connection、identity、retry）、
`src/api/mod.rs`（authentication 與 peer-PID monitoring）、
`src/api/handlers/mcp_proxy.rs`（dispatch 與 timeout band），以及
`src/mcp/registry.rs`（authoritative registry 與 execution class）。
