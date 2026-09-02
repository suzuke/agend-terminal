[English](MCP-TOOLS.md)

# AgEnD MCP Tools Reference — 工具參考（33 個工具）

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

- 不帶參數會 drain unread 訊息並標為 `delivering`；此時尚未標為 processed。回傳 row 包含 durable `delivery_count` 與 `first_delivered_at`；redelivery 也會在 production response 的 `redelivery_history` array 顯示 message id 與首次投遞時間，但不改變 canonical identity/text。
- `message_id` 描述單一訊息；`thread_id` 取得 thread。可選 `instance` 可限定已授權查詢範圍。
- `action:"ack"` 可確認目前 delivering 的單一 `message_id`；若 row 曾被投遞後 reclaim/requeue，只有 durable 歷史證明曾投遞時 targeted ack 才會成功。省略 ID 時只確認整批目前 in-flight batch，不會誤確認從未投遞的 unread row；回應會區分 `acked-after-reclaim`、`never-delivered` 與 `already-processed`。Storage 失敗會明確回傳 `outcome:"error"` 與 `code:"inbox_ack_failed"`，不會被當成 `no-delivering-rows`。
- `action:"clear"` 精簡清除非 obligation 訊息，未回答的 query/task 仍維持 unread，並列在 `requires_response`。
- `action:"discharge"` 需要 `message_id` 與非空 `reason`；它會在不回答的情況下關閉 channel-reply obligation，並通知 operator。
- 再次 drain 會隱式 ack 前一批 delivery；未確認的 batch 約十分鐘後可能被 reclaim 並重新投遞。Fresh session reset 也會重新排入未確認 row，交由 successor recovery。這取代 #159 舊 settle rationale：寫入 `read_at` 雖能隱藏 stale delivery，卻可能靜默遺失未確認訊息。

### `reply`

透過外部 channel 回覆 user/operator；不要用於 agent 之間的通訊。

- 必填：`message`。
- `message_id` 會依原始 inbox message 的 channel 路由，傳送成功後 settle 該列。
- 可選 `task_id` 與 `correlation_id` 保留 reply-to correlation。
- `default_action` 應搭配 `timeout_secs`，以記錄有 timeout default 的 decision。

### `operator_page`

在**不需要任何 inbound channel binding** 的情況下，把訊息推到 **operator 的 Telegram**——用於 operator 明確要求「離開或睡覺時有 milestone 要通知我」的場合。這補的是 harness 的缺口：`PushNotification` 的行動推播只有在 Remote Control 連線時才會送達，而 `reply` 需要一則 inbound 訊息才有回覆對象，operator 直接在 TUI 打字則不會產生 binding。

- 必填：`message`。純文字，超過 1000 字元會截斷，並一律加上呼叫者的 instance 名稱前綴。在截斷與加前綴之前，內容會先被正規化成**一行**：所有 **Cc** control character（LF、CR、TAB、VT、FF、NEL 以及其餘 C0/C1）、所有 Unicode **White_Space** 字元（NBSP `U+00A0`、`U+1680`、`U+2000`–`U+200A`、`U+202F`、`U+205F`、`U+3000`，以及兩個強制換行 `U+2028`／`U+2029`）、以及所有 general category **Cf** 的 format 字元（ZWSP `U+200B`、ZWNJ／ZWJ、LRM／RLM／LRE／RLE／PDF／LRO／RLO 這組 bidi 控制、`U+2066`–`U+2069` isolate、`U+FEFF`），一律換成**一個**空白，連續空白再收斂成一個。

  正規化之後若內容仍帶有 daemon 的發件標記 `[operator-page from `——比對**不分大小寫**，所以 `[Operator-Page From ops]` 同樣算——這則 page 會被**拒絕**，代碼 `marker_in_body`。這道檢查緊接在 enabled 開關之後、排在其餘所有 gate 之前：在 authority、可送達性與任何額度扣減之前，所以偽造的內容永遠不會花掉呼叫者的額度；同時以 `warn!` 記下呼叫者名稱，讓 operator 看得到有人試過。它刻意排在開關後面：paging 關閉時整個工具是惰性的，只回 `operator_page_disabled`，所以沒被打開的功能不會變成 agent 拿來灌 daemon log 的管道。先前的版本是把字面標記改寫成 `[quoted: operator-page from ` 後照樣送出；那等於**在沒有任何提示的情況下竄改 operator 看到的文字**（回應裡沒有旗標、log 裡沒有紀錄），而且大小寫變體與 NBSP／ZWSP／RLO 這類同形字根本繞得過去，所以整個作法撤掉。正常的 page 幾乎不可能含有這個標記，拒絕的代價是零，換來的是「攻擊嘗試變成看得見的事件」。

  誠實的界線：client 對長訊息 soft-wrap 時仍可能讓某個視覺行從內容中段開始，而內容本來就可以寫出其他「看起來像發件者」的文字。這道處理擋的是「複製標記」「用同形字拼出標記」與「另起一行」，擋不掉「寫得像」。
- **僅限 orchestrator，且綁定到「活的」instance。** 呼叫所帶的 `instance` 會先拿去 daemon 的 live registry 解析：沒有對應到任何執行中 instance（或對應到兩個）的名字以 `unknown_caller` 拒絕；同時屬於兩個 team 的呼叫者以 `ambiguous_team` 拒絕（而不是拿 map 順序亂猜）；不是所屬 team 現任 orchestrator 的以 `not_orchestrator` 拒絕並告知該找誰轉送。沒有 daemon runtime 的 standalone bridge 呼叫無從解析，以 `no_live_identity` 拒絕。

  誰都別騙自己：所有 agent 與 daemon 共用**同一個 OS 使用者**，所以拿 orchestrator 活著的名字來呼叫的座位**依設計會被放行**。這道 gate 擋的是「指向不存在之物的名字」，擋不住「說謎的座位」。真正限住損害的是下面這些：預設關閉、只有 operator 能開的開關、每小時 3 則、單一專用 topic（仍在 allowlist 群組內），以及 fail-closed 的額度狀態。
- **預設關閉，且開關只有 operator 能動。** 開關位於 daemon 的 runtime config；`config` MCP tool 只能讀、不能寫（`set` 於 #2548 移到 CLI）。operator 以下列指令開啟：

  ```
  agend-terminal admin config-set operator_page.enabled true
  ```

  目的地 topic 則以 `agend-terminal admin config-set operator_page.topic_name <NAME>` 設定（預設 `operator-notifications`）。`fleet.yaml` 裡的 `channel.operator_page` 區塊已完全不再讀取：fleet.yaml 是 agent 寫得了的檔案，把總開關放在那裡等於讓被管的人自己控開關。Telegram 本身仍需要 `fleet.yaml` 的 `channel.user_allowlist`——沒有它 outbound 一律 fail-closed。
- **必須是 Telegram。** 這個工具存在的目的是送到 operator 的**手機**：已授權的 Discord channel 不算數；無法送達 telegram 的呼叫以 `not_delivered` 拒絕，且**不消耗**額度。
- **每位 orchestrator 每滾動小時上限 3 則。** 超出的直接**丟棄**、不排隊；拒絕回應帶有 `retry_after_secs`，呼叫者應改把 milestone 寫進 `SESSION-HANDOFF.md`。計數器以 daemon 記憶體（加鎖）為權威，快照寫到 `$AGEND_HOME/operator_page_rate.json`，以保證重啟不會退款。

  **快照由 operator 種下，daemon 不會自己造。** `agend-terminal admin config-set operator_page.enabled true` 會在快照不存在時放一個空的（已存在則完全不動，所以重跑這道指令不會退還已用掉的額度）。daemon 拒絕自己補檔：啟動時快照**不存在**的處理方式與「壞掉」完全相同——拒絕。這堵住的正是一個繞道：先刪掉檔案再逼 daemon 重啟，舊設計會以空額度重新初始化，等於把「每小時 3 則」變成「每次重啟 3 則」。

  所有不可信狀態一律以 `budget_unavailable` 拒絕（與 `rate_limited` 明確區分），並附上 `cause` 指出是哪一種：`snapshot_absent`、`snapshot_corrupt`、`snapshot_missing`（daemon 執行中被刪）、`snapshot_unusable`、`snapshot_unwritable`。**每一種的解法都是請 operator 重跑那道 enable 指令**來重新種下快照；快照壞掉的情況要先修好或刪掉。

  **執行這道解法之前，先知道它的代價。** 對 `snapshot_absent` 與 `snapshot_corrupt` 來說，daemon 已經不再持有這個 home 的已用次數——它跟著快照一起被毀掉了——所以重新種下的是一份空的計數器，等於**重新開始一個滾動小時**：當前這個小時裡已經用掉的 page 會被遺忘，呼叫者拿回完整額度。拒絕訊息的 hint 會在 operator 動手之前把這件事講明，種快照本身也會以 `warn!` 記下快照路徑。至於 `snapshot_missing` 與 `snapshot_unwritable`，記憶體裡仍有真實的已用次數，重新種下時會把它寫回去，小時不會被重置；這兩種的 hint 不會帶這段警告。這段話**不是**在宣稱快照防得了竄改——見下一段。它只是 tamper-EVIDENT，而且重置一定要經過一個明確的人為動作。

  話說清楚：所有 agent 與 daemon 共用**同一個 OS 使用者**，所以這個檔案**不是防竄改的**，agent 隨時刪得掉。這個設計換到的是：竄改只會導致**拒絕**、會連同快照路徑寫進 log、而且只有 operator 的指令能救回來。Agent 可以讓自己的 pager 靜音，但**自己**拿不回額度——唯一回到滿額度的路徑要經過上面那道 operator 指令，而該指令現在會在執行前就講明它會重開一個滾動小時。
- **路由。** 訊息送到專用的 forum topic（預設 `operator-notifications`），首次使用時自動建立並註冊，讓所有 page 集中在一個 operator 可以靜音的地方。若該 topic 無法建立，則退回發送者自己的 topic——兩者都在同一個 allowlist 群組內。
- **operator 的 Away/Sleep 模式不會抑制 page。** 這是刻意的：這個功能存在的原因，正是 operator 在睡覺而且要求 milestone 要叫醒他。控制 page 的是 `enabled` 開關（總開關）與每小時上限，而不是 mode；一般 daemon 通知仍照舊受 mode 管制。

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

- 必填：`branch`；可選 `repository_path`、`rebase_mode` 與 `task_id`。
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
- 目前的 keys：`dev_idle_threshold_secs`、`fleet_idle_threshold_secs`、`fleet_idle_ack_ttl_secs`、`hang_auto_recovery_enabled`、`usage_limit_propagation_enabled`、`idle_watchdog_enabled`、`show_pane_state`、`copy_on_select`、`dim_unfocused_panes`、`observed_badge`、`context_alert_pct`、`context_handoff_pct`、`context_handoff_escalate_pct`、`experimental.tool_cli_enabled`、`operator_page.enabled`、`operator_page.topic_name`。
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

實驗性的 `agend-terminal tool <NAME>` 指令使用相同的 daemon handler、名稱、
參數與 instance claim。單一 instance 應一次使用一種 invocation surface；與
使用另一種 surface 的 peer 協作時，請做機械式轉譯。

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
