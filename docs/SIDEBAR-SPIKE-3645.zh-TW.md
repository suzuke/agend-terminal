[English](SIDEBAR-SPIKE-3645.md)

# Spike：常駐側欄（spaces × agents）與 attention queue（issue #3645）

> Issue #3645 的設計 spike。這是一份純決策文件：這裡沒有任何東西會改動生產
> 程式碼。它逐一回答七個 spike 問題，並附上每一刀會動到的確切 `path:line`，
> 再把工作切成一連串可獨立審查的切片，讓第一刀就是一條唯讀側欄，重複使用
> tab bar 每幀已經算好的狀態快照。要借鑒 herdr 的是常駐的 spaces/agents 側欄
> 與 attention queue；它把狀態偵測規則完整外部化的做法，以及它的通知機制，
> 都明確不在本文件範圍內。

## 0. 狀態、範圍與非目標

目前 TUI 是單一直向堆疊：tab bar、pane 區、status bar
（`src/render/core_render.rs:469-476`）。agent 狀態只能從每個 tab 的彩色小點看
出來（`state_color` 於 `src/render/core_render.rs:18`、`highest_priority_state`
於 `:507`），而唯一的 fleet 總覽是 Board overlay 裡的全螢幕 Fleet 檢視
（`Ctrl+B f`，dispatch 於 `src/app/dispatch.rs:243-253`，由
`src/render/panels_fleet.rs:83` 的 `render_fleet_view` 繪製）。既沒有常駐的
一瞥，也沒有「跳到下一個需要我的 agent」。

這個 spike 要驗證的省力路徑是：`build_agent_state_snapshot`
（`src/render/core_render.rs:364`，remote 版本 `:371`）每一幀都已經為 tab bar
解析好每個 pane 的狀態，而 team 分組也早已存在於 `plan_team_order`
（`src/team_order.rs:47`）。因此側欄大致上只是對 render loop 已經握有的資料
換一個投影方式。

- 目標：用 `path:line` 回答問題 1–7；給出有明確依賴的實作切分；證明資料
  已經現成。
- 非目標：狀態偵測規則外部化（#3647，另有 spike）；toast 或音效通知；任何
  daemon 端或狀態機的行為變更；`context%` 欄位只指出它落在哪，不在此實作。
- 交付物：這份文件對。不含生產碼變更。

## 1. 問題 1 — space 對應什麼

決策：預設 space 就是 **team**，而 worktree 是巢狀在來源 space 底下的第二個
維度。第三個維度（source repository 或專案看板）延後，不在本文件設計。

Team 是現成、已建好的正規分組：`plan_team_order` 以設定的成員順序回傳
deterministic 分組（`src/team_order.rs:47`、`src/team_order.rs:118-154`），
Fleet 檢視（`src/render/panels_fleet.rs:127`）與 remote roster
（`src/app/app_state_remote.rs:43`）都已消費它。#3630 的 deterministic
ordering 契約就是這個函式，所以側欄共用它，而不是另造一套順序。

Worktree 巢狀使用 binding 紀錄，它已持久化 `branch`、`worktree` 與所屬的
`source_repo`（`src/binding.rs:233`、`:380`）。某個 space 的 pane 帶有
managed-worktree binding 時，就渲染成 `source_repo` 相符的那個 space 的子節點；
binding 不存在時，該 space 維持平坦。

沒有 team 的 instance 會落進一個合成的 `unassigned` space，與 Fleet 檢視現在
的做法完全相同（`src/render/panels_fleet.rs:156-161`）；`plan_team_order` 會把
它們放在 `ungrouped` 清單裡回傳（`src/team_order.rs:156-161`）。

| 維度 | 真實來源 | 決策 |
|---|---|---|
| Team | `plan_team_order`（`src/team_order.rs:47`） | 主要 space；與 Fleet 檢視共用 |
| Worktree | binding `source_repo`/`branch`（`src/binding.rs:233,380`） | 巢狀子 space，掛在來源 space 下 |
| 沒有 team | `TeamOrderPlan.ungrouped`（`src/team_order.rs:156-161`） | 合成 `unassigned` space |
| 專案 / repo | 只有本地 `fleet.yaml` | 延後；第一刀不作為可選維度 |

## 2. 問題 2 — agents 列的內容

每一列都是每幀已算好資料的固定欄位投影，只有一個誠實的例外：`context%` 目前
只以私有的 `Option<(f32, Instant)>` 存在 tracker 上（`src/state/mod.rs:322`），
沒有像 `published_state`（`src/agent/mod.rs:105`、`:141`）那樣 lock-free 的
published handle。要顯示它就得照抄那個模式；在那之前側欄對它顯示 `—`。

名稱、backend 與 team 都來自 pane 紀錄；task 與 branch 則是 Fleet 檢視已經在
做的相同查找（`src/render/panels_fleet.rs:114-125`、`:209`）。狀態圖示與顏色
重用 `state_color`（`src/render/core_render.rs:18`），所以側欄與 tab 小點永不
分歧。

| 欄位 | 來源 | 備註 |
|---|---|---|
| 狀態圖示＋顏色 | snapshot ＋ `state_color`（`src/render/core_render.rs:18`） | 與 tab bar 用同一份解析後 map |
| 名稱 | `pane.agent_name` | 短顯示名，比照 Fleet 的 `build_agent_line` |
| Backend | `pane.backend` / `Backend::name()`（`src/backend.rs:828`） | 僅供顯示；不涉行為 |
| Team | `plan_team_order` 成員身分 | 同時決定所屬 space |
| Task 標題 | 本地看板比對（`src/render/panels_fleet.rs:114-125`） | 會截斷；未認領時為 `—` |
| Branch | `binding::read`（`src/binding.rs:710`） | 會截斷（`build_agent_line`，`:209`） |
| Context % | tracker 欄位（`src/state/mod.rs:322`） | 需要新的 published handle；在此之前為 `—` |

窄寬度截斷沿用 tab bar 的先例：以 `unicode_width` 量測，只渲染價值最高的前綴
（`src/app/mouse.rs:391`）。寬列顯示圖示、名稱、branch 與 task；窄列先捨
branch 再捨 task；compact 模式（第 5 節）只留圖示。截斷僅影響顯示，永不改變
排序鍵。

## 3. 問題 3 — 排序與分組

兩種模式，都建立在同一份正規成員身分上：

- `spaces` 模式把列分組到各自的 space 下，成員順序完全依 `plan_team_order`
  （`src/team_order.rs:118-154`），與 Fleet 檢視一致。
- `priority` 模式把所有列攤平成單一 attention queue。

Attention 順序**不能**用 `AgentState::priority()`（`src/state/mod.rs:148`）。
那個尺度是為 latch 與 tab 小點的競爭調校的，不是為「誰需要人」：`Active`（6）
排在 `Idle`（4）前面，但兩者都不需要任何人，而 `AwaitingOperator`（2）幾乎墊底，
儘管它是對人的硬阻塞。因此側欄另外從既有的 predicate 推導一個專用的
`attention_rank(AgentState) -> u8`（`is_error` 於 `src/state/mod.rs:181`、
`is_unavailable` 於 `:186`、`wants_raw_keystrokes` 於 `:194`），再加上明確的
match。

同一層內以正規 team-order 索引、再以名稱決勝，因此 queue 每幀穩定。這個順序
是全序且 deterministic，正是第 3 刀（跳到下一個）能重現的前提。

| 層級 | 成員狀態 | 為何排在這 |
|---|---|---|
| 1 | PermissionPrompt、InteractivePrompt、AwaitingOperator | 人的關卡正在阻塞 agent |
| 2 | AuthError、ApiError、ModelUnsupported、UsageLimit | 需要 operator 修復或確認的錯誤 |
| 3 | GitConflict | 受阻，但 agent 端動作可清除 |
| 4 | Crashed、Restarting、Hang | 生命週期故障；復原可能需要人手 |
| 5 | RateLimit、ServerRateLimit、ContextFull | 通常自癒，但值得看一眼 |
| 6 | Starting、Active | 忙碌；對 operator 無所欠 |
| 7 | Idle | 沒有待辦；排最後 |

## 4. 問題 4 — 跟 Fleet overlay 怎麼分工

側欄是同一份資料的常駐、精簡投影；Board overlay 的 Fleet 檢視維持為全螢幕的
細節介面，與相鄰的 Status、Monitor、Tasks 分頁並存
（`src/render/panels.rs:233-272`）。Overlay 保留，不合併。

理由是結構性的：overlay 是 Board overlay 內的一個模態模式
（`src/app/dispatch.rs:243-253`），它的列來自一組不同的來源——instance metrics、
task 看板，以及同步的 `binding::read` 與 `FleetConfig::load` 呼叫
（`src/render/panels_fleet.rs:89-98`、`:209`）。合併會把那些每幀磁碟讀取拉進
永遠開啟的 render 路徑。側欄則只讀取記憶體中的 snapshot 與 pane 紀錄。

去重之後再做，且是單向的：抽出共用的 row builder，讓 overlay 與側欄在格式上
一致（第 4 刀），而不是刪掉 overlay。Overlay 仍保留側欄刻意省略的欄位（health、
memory、CPU、uptime）。

| 介面 | 生命週期 | 資料來源 | 決策 |
|---|---|---|---|
| 側欄 | 常駐（可切換） | 每幀 snapshot ＋ pane 紀錄 | 新增，第一刀唯讀 |
| Fleet 檢視 | Board overlay 內模態 | Metrics ＋ tasks ＋ binding 磁碟讀取 | 保持不變 |
| Status/Monitor/Tasks | 模態相鄰分頁 | 各自來源 | 不動 |
| 共用格式 | n/a | n/a | 第 4 刀抽出 row builder |

## 5. 問題 5 — 版面、寬度與 hit-testing

側欄只加在中間帶：把目前的 `chunks[1]`（`src/render/core_render.rs:469-476`）
包進一個水平 layout，寬度為 `sidebar_width` 加剩餘 pane 區。tab bar 與 status
bar 維持滿寬，因此它們的渲染與 hit-testing 不受影響（tab bar hit test 於
`src/app/mouse.rs:382-399`；共用寬度契約記於 `src/render/core_render.rs:527-529`）。

Pane tree 本來就會依傳入的 area 記錄自己的 rect（`render_pane_tree`，
`src/render/core_render.rs:616-674`），而 `pane_at` /
`title_bar_at_with_team` 讀取那些 rect（`src/layout/tab.rs:346`、`:362`）。所以
唯一的滑鼠改動是那個寫死的 pane 區 rect：`handle_down` 建構
`Rect::new(0, 1, c, r-2)`（`src/app/mouse.rs:169`），而 mouse-forward 路徑用
同一個假設（`:108`）。兩者都必須改成 `x = sidebar_width`、`width = c -
sidebar_width`。

側欄點擊另有一組專屬 hit test，在 tab bar 那列檢查之後
（`src/app/mouse.rs:144-207`）、pane/border 處理之前評估，這樣點在某一列上就
不會穿透到底下的 pane。寬度是 runtime-config 值，比照 `observed_badge` 的
先例（`src/runtime_config.rs:75-78`），並夾在合理的最小與最大值之間。

```text
+--------------------------------------------+  tab bar（滿寬，第 0 列）
| spaces            |                         |
|   dev             |                         |
|     dev/wt-3645   |      pane grid          |
| agents            |   （向右位移           |
|   * dev-1  busy   |     sidebar_width）     |
|   ! dev-2  perm   |                         |
+--------------------------------------------+  status bar（滿寬）
```

Compact 模式值得做，但排最後：它是側欄寬度與列內容的渲染改動（只剩一或兩欄的
狀態圖示條），不碰幾何管線，所以屬於第 4 刀，與寬度設定一起。

| 關注點 | 位置 | 改動 |
|---|---|---|
| 頂層切割 | `src/render/core_render.rs:469-476` | 只把中間帶水平切開 |
| Pane rect | `src/render/core_render.rs:616-674` | 不變；由傳入 area 推導 |
| Pane hit test | `src/layout/tab.rs:346`、`:362` | 不變；讀取已記錄的 rect |
| 滑鼠 pane 區 | `src/app/mouse.rs:169`、`:108` | 把 `x` 位移 sidebar 寬 |
| 列 hit test | `src/app/mouse.rs:144-207` | 新增側欄分支，在 pane 處理之前 |

## 6. 問題 6 — 互動與鍵綁

所有鍵都留在既有的 `Ctrl+B` prefix 內（`src/keybinds.rs:102`），而兩個新字母
目前在 `dispatch_prefix` 裡都未映射（`src/keybinds.rs:167-239`），所以與現有
鍵位表沒有衝突。

- 切換側欄：`Ctrl+B v`（新的 `Action::ToggleSidebar`）。`v` 未被佔用。
- 跳到下一個需要處理的 agent：`Ctrl+B u`（新的 `Action::NextAttentionAgent`）。
  `u` 未被佔用，且應把該 action 加進 `is_repeatable`
  （`src/keybinds.rs:242`），讓 `u u u` 能像 `Ctrl+B o` 循環 pane 那樣走訪
  queue。
- 切換 `spaces` / `priority`：用 runtime config（`:set sidebar_sort=...`），不
  綁鍵，免得把 prefix 字母花在罕用的切換上。
- 點某一列：聚焦該 agent 的 pane（僅滑鼠），重用既有的 focus/`goto_tab` 管線
  （`src/app/mouse.rs:146-153`）。

新增 `Action` variant 會被 exhaustive 的 `app::dispatch` match 強制
（契約記於 `src/keybinds.rs:62-66`），因此新鍵不會出現
live-in-attach-but-dead-in-app 的情況。

| 互動 | 綁定 | 衝突檢查 |
|---|---|---|
| 切換側欄 | `Ctrl+B v` | `v` 在 `dispatch_prefix` 未映射 |
| 下一個 attention agent | `Ctrl+B u` | `u` 未映射；加入 repeat 鍵 |
| 排序模式 | `:set sidebar_sort` | 非 prefix 鍵 |
| 聚焦一列 | 滑鼠點擊 | 新的 hit test，在 pane 處理之前 |

## 7. 問題 7 — 遠端與多 daemon 一致性

狀態在結構上就是一致的，因為側欄讀的是 tab bar 所用的*同一份解析後 snapshot*：
`render_with_team` 會從 `remote_states` 選擇本地的
`build_agent_state_snapshot` 或 remote-aware 版本
（`src/render/core_render.rs:478-483`），而 remote map 就是 daemon RPC 產生、
只在 attached 模式傳入的 name-keyed `remote_agent_states`
（`src/app/app_state.rs:247`、`:943`，於 `:1217` 設定）。因此排序與導覽在本地與
遠端 agent 之間保持一致。

Metadata 是弱的那一半。Backend 來自本地 pane 紀錄，但 branch、task 與 team
都來自本地磁碟（`src/binding.rs:710`、本地 task 看板、本地 `fleet.yaml`）。在
attached 或多 daemon 模式下，遠端 instance 的 binding 存在 daemon 主機上，所以
本地 `binding::read` 可能落空，本地 `fleet.yaml` 也可能不同。規則是：顯示 `—`
而非捏造，並把那些欄位視為 best-effort 的加值。

把 daemon 的 agent-state snapshot 擴充 metadata 能修正這點，但那是 daemon 端
改動，不在本 spike 範圍。它被列為後續項，而不是被默默假設。

| 資料 | 本地來源 | 遠端來源 | 決策 |
|---|---|---|---|
| 狀態 | registry atomics（`src/agent/mod.rs:105`） | `remote_agent_states`（`src/app/app_state.rs:247`） | 與 tab bar 同一份解析後 map |
| Backend | `pane.backend` | 本地 pane 紀錄 | 可靠 |
| Branch | `binding::read`（`src/binding.rs:710`） | 僅 daemon 主機 | Best-effort；不存在時 `—` |
| Task / team | 本地看板 / `fleet.yaml` | daemon 主機 | Best-effort；不存在時 `—` |

## 8. 實作切分與依賴

每一刀都可獨立審查，且第一刀保持唯讀。

| 刀 | 改動 | 依賴 |
|---|---|---|
| 1 | 唯讀側欄：版面切割 ＋ 由既有 snapshot 渲染 `spaces`/`priority` | 無 |
| 2 | `attention_rank` 排序 ＋ 共用 `plan_team_order` 分組；模式用 config | 第 1 刀 |
| 3 | 點擊聚焦 hit test ＋ `Ctrl+B u` 下一個 attention 導覽 | 第 2 刀 |
| 4 | 抽出共用 row builder；compact 模式；`sidebar_width` 設定 | 第 1、3 刀 |
| 5 | 發布 lock-free `context%` handle 並顯示 | 第 1 刀 |

第 5 刀刻意分開：它是唯一需要新增 producer 端管線的欄位，且不得卡住唯讀側欄。

## 附錄 A — 決策摘要

| 問題 | 決策 |
|---|---|
| 1 Space 身分 | 主要 space 是 team；worktree 巢狀於 `source_repo` 下；無 team 落 `unassigned`；專案延後 |
| 2 列內容 | 圖示/名稱/backend/team/task/branch 取自現有資料；`context%` 需新的 published handle |
| 3 排序 | `spaces` 用 `plan_team_order`（#3630）；`priority` 用專用 `attention_rank`，非 `AgentState::priority()` |
| 4 Fleet overlay | 保留為模態細節介面；側欄是常駐精簡投影；之後共用 row builder |
| 5 版面 | 側欄只在中間帶；tab/status bar 維持滿寬；滑鼠 pane 區 `x` 位移；compact 屬第 4 刀 |
| 6 互動 | `Ctrl+B v` 切換、`Ctrl+B u` 下一個 attention、`:set sidebar_sort`；無 prefix 衝突；點擊聚焦一列 |
| 7 遠端 | 狀態用同一份解析後 snapshot；metadata best-effort 顯示 `—`，daemon 加值是後續項 |
