[English](MCP-TOOLS.md)

# AgEnD MCP Tools Reference — 工具參考（29 個工具）

## 動作型工具（Action-based Tools）

### `task`
管理 task board。動作：create、list、get、claim、done、update。
- **action**: create / list / get / claim / done / update
- title, description, id, assignee, priority, status, branch, depends_on, filter_status, filter_assignee, result, due_at, duration, fields
- `list` 預設為**精簡模式**（#2475）：`description`／`result` 會限制長度（約 200 字）。傳 `verbose: true` 可取回完整文字；回應中的 `terse: true` 表示已套用精簡。
- `list` 可傳 `fields: "minimal"`（#2475）只投影 id/title/status/assignee/priority；回應帶 `fields: "minimal"|"full"`。
- `get`（#2475）以 `id`（別名 `task_id`）回傳單一任務的**完整**記錄 —— 搭配精簡 `list`，需要單筆完整內容時使用。

### `decision`
管理 decision。動作：post、list、update。
- **action**: post / list / update
- title, content, id, tags, scope, supersedes, archive, include_archived, ttl_days

### `team`
管理 team。動作：create、delete、list、update。
- **action**: create / delete / list / update
- name, members, orchestrator, description, repository_path, add, remove

### `schedule`
管理 schedule。動作：create、list、update、delete。
- **action**: create / list / update / delete
- id, label, instance, message, cron, run_at, timezone, enabled

### `deployment`
管理 deployment。動作：deploy、teardown、list。
- **action**: deploy / teardown / list
- name, template, branch, directory

### `ci`
管理 CI 監看。動作：watch、unwatch、status。
- **action**: watch / unwatch / status
- repository, branch, interval_secs

### `repo`
管理 repo worktree。動作：checkout、release。
- **action**: checkout / release
- repository_path, branch, path

### `health`
管理健康狀態。動作：report、clear。
- **action**: report / clear
- reason (rate_limit / quota_exceeded / awaiting_operator), retry_after_secs, instance, note

## 通訊（Communication）

### `send`
傳送訊息給另一個 instance，或廣播給多個 instance。統一取代 send_to_instance/delegate_task/report_result/request_information/broadcast。
- **message**: 文字內容
- instance, instances, team, tags（路由）
- request_kind: query / task / report / update
- task_id（kind=task 時必填）, success_criteria, branch, working_directory
- context, requires_reply, task_summary, correlation_id, parent_id, thread_id
- force, force_reason, second_reviewer, second_reviewer_reason
- reviewed_head, artifacts

### `inbox`
查看待處理的訊息、以 ID 查詢，或取得某個 thread 的訊息。
- message_id, thread_id, instance

### `reply`
透過目前作用中的 channel 回覆使用者（不用於 agent 之間的通訊）。
- **message**: 回覆內容
- default_action, timeout_secs

### `download_attachment`
下載檔案附件（telegram 多媒體）。回傳本機路徑。
- **file_id**: 附件檔案 ID

## Instance 生命週期（Instance Lifecycle）

### `create_instance`
建立 agent instance。支援同質團隊（count + backend）和異質團隊（backends 列表）。
- **name**: instance 或團隊的基礎名稱
- backend, model, model_tier, args, branch, working_directory, task
- team, count, backends, layout, target_pane

### `delete_instance`
停止並移除一個 instance。
- **instance**: 要刪除的 instance

### `start_instance`
啟動一個已停止的 instance。
- **instance**: 要啟動的 instance

### `restart_instance`
終止並重啟一個 instance。預設模式 `resume` 會保留對話狀態；`fresh` 則從乾淨狀態啟動。
- **instance**: 要重啟的 instance
- mode (resume / fresh), reason, force
- `fresh` 預設會在 bound worktree 有未提交變更時拒絕（#2476）；請先 commit/push 或留下 task-board handoff，或傳 `force: true`。

### `list_instances`
列出所有作用中的 agent instance。可選擇性傳入 `instance` 以取得單一 instance 的詳細資訊。
- 預設為 **compact**（#2475）：每列會移除雜訊較大的 `observed_status.evidence` trail。傳 `verbose: true`（或 `include_evidence: true`）可包含它。

### `set_metadata`
設定 per-instance 顯示中繼資料。#2547：從原本獨立的 `set_display_name` / `set_description` 工具合併而來。
- **action**: display_name / description
- action=display_name：**name** — 新的顯示名稱
- action=description：**description** — instance 描述

### `set_waiting_on`
宣告這個 instance 目前正在等待什麼。傳入空字串以清除。
- **condition**: 你正在等待的內容

### `interrupt`
對目標 agent 的 PTY 送出 ESC，中斷其當前的 LLM turn。
- **instance**: instance 名稱
- reason

### `move_pane`
將某個 instance 的 pane 移動到 TUI 中的另一個 tab。
- **instance**: 要移動的 instance
- **target_tab**: 目的地 tab 名稱
- split_dir (horizontal / vertical)

### `pane_snapshot`
讀取目標 instance PTY scrollback 中的可見文字（已去除 ANSI）。
- **instance**: instance 名稱
- lines（預設 100，最大 10000）
- `to_file: true`（#2478）會把完整 snapshot 寫到 `$AGEND_HOME/captures/`，tool 只回精簡摘要與路徑，避免診斷 dump 灌進 context。

## Worktree 與 Binding（Worktree & Binding）

### `bind_self`
將呼叫端 agent 綁定到指定 branch 上一個全新的 worktree。會拒絕 main/master（E4.5）以及跨 agent 的衝突。
- **branch**: 要綁定的 branch
- repository_path, repository（已棄用）, rebase_mode

### `release_worktree`
釋放由 daemon 管理的 worktree 並清除綁定。只會移除帶有 `.agend-managed` 標記的 worktree。
- **instance**: 要釋放的 instance
- dry_run

### `force_release_worktree`
強制釋放一個殘留的、由 daemon 管理的 worktree 目錄。緊急救援工具。
- **instance**: instance 名稱
- **branch**: branch 名稱

### `binding_state`
回報某個 agent 在 daemon 端的結構化 bind 狀態。非破壞性的內省查詢。
- **instance**: 要檢視的 instance

## Daemon 操作（Daemon Operations）

### `config`
執行期可變更的 daemon 設定。動作：get、list。#2548：set 動作已移至 `agend-terminal admin config-set` CLI（20 天內零 MCP 呼叫）。（可用的設定 key 由 daemon 的 runtime config 推導，列於工具的即時描述中。）
- **action**: get / list
- key（get 必填）

### `mode`
讀取 operator 的可用性／授權模式（對 agent 為唯讀）。設定模式僅限 operator，透過 `agend-terminal mode <active|away|sleep>` CLI 操作。
- **action**: get

### `restart_daemon`

請求優雅地重啟 daemon。Daemon 會以代碼 42 結束，由 wrapper script 重新啟動。可重複呼叫（idempotent）。

**注意**：所有 agent 的 PTY session 都會被中斷。持久化狀態（task、binding、ci_watch）會保留；傳輸中的 inbox 訊息可能會遺失。

**參數**：無。
