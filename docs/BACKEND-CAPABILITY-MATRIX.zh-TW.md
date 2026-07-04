[English](BACKEND-CAPABILITY-MATRIX.md)

# Backend 能力矩陣

每個 backend 都跑在同一套 PTY orchestration 機制上，但各自呈現的 UI——以及 agend-terminal 對這個 UI 能信任到什麼程度——並不一致。本文檔針對每個 `Backend` enum 變體，記錄它今天實際使用的 state 偵測訊號、是否曝光 context usage、submit/inject 行為、resume 能不能用、MCP 是否接好，以及已知的脆弱點。

**紀律**：本文每一格結論，要嘛有 `path:line` 程式碼依據（本 repo，`main` branch），要嘛明確標示**未查證**。沒有任何一格是用猜的。查不到依據的地方就老實寫出來——誠實的空白比一個聽起來合理但編造出來的結論更有價值。

## 與 #2413 的分界

本文檔記錄的是**現況**——每個 backend 今天在 production 裡實際依賴的 state 偵測訊號。這是一張快照，不是一份計畫。

[#2413](https://github.com/suzuke/agend-terminal/issues/2413)（「Out-of-path API-activity probe to fix false-idle blind spot in pattern-based agent_state」）才是**改善未來的 roadmap**：這是一項持續進行中的實證研究（Shadow Observer，見 `docs/SHADOW-OBSERVER-QUANT-AGY-2413.md` 及其 claude/codex 對照版本，位於 `docs/archived/` 下），目的是逐 backend 量測「除了原始 PTY pattern 比對之外，加上更多 structured 訊號能不能補上 false-idle 的盲點」。當本文說某個 backend 是「PTY heuristic」時，#2413 那邊的工作可能正在量化評估該 backend 能不能升級——本文不涉及那份 roadmap 或其研究結論，只記錄今天的實況。想了解那份改善工作目前進度如何，請直接讀 #2413 本身的文檔。

## 訊號權威性階梯（參考用）

`src/daemon/shadow/evidence.rs:65-90` 把訊號權威性由高到低排列：**Hook**（`Confirmed` confidence）→ **Stream**（session log tail，例如 Kiro 的 jsonl）→ **Screen**（PTY pattern match）→ **ProcessHeuristic** → **Inferred**。下面各 backend 的小節說明的是它今天在 `agent_state` 上實際站在哪一階——不是它理論上能爬到哪一階。

## 總覽

| Backend | Agent-state 訊號 | Context usage | Submit / inject | Resume | MCP | 脆弱點摘要 |
|---|---|---|---|---|---|---|
| **ClaudeCode** | Hook，`Confirmed` authority——唯一站上階梯頂端的 backend | StatusLine（僅限 fleet 自訂格式） | Bulk，`submit_key="\r"` | `--continue`，有 on-disk session 檢查把關 | 有——明確的 `--mcp-config` flag | 覆蓋最完整的 backend；但仍綁定特定 Claude Code 版本 |
| **Codex** | 純 PTY/Screen heuristic——無 hook | Unavailable | Typed/paced（`typed_inject=true`，#1670） | 硬編在 spawn args 裡的 `resume --last`，未走通用的 `ResumeMode` 抽象 | 有——per-project `.codex/config.toml` | 最依賴 PTY 的 backend；根因脆弱點是 #1670 的 ratatui input widget |
| **KiroCli** | PTY/Screen heuristic 主導 `agent_state`；另有一條 jsonl tail 但只是 Stream authority，從不 promote | StatusLine | Bulk，`submit_key="\r"`，固定 50ms pre-submit sleep | `--resume` | 有——自己的 auto-discovery（`.kiro/settings/mcp.json`） | 唯一需要 `redraw_after_resize` 的 backend；有數個 input-line 誤鎖 guard |
| **OpenCode** | 純 PTY/Screen heuristic——無 hook | Unavailable（footer 有 token/cost 字串但沒被解析） | Typed/paced（`typed_inject=true`） | `--continue`；帶一個**未關閉**的 dummy-session-id 事故 | 有——`opencode.json` 的 `"mcp"` 欄位 | 未關閉：dummy-session wedge（罕見的「process 永不退出」變種能躲過偵測） |
| **Agy** | 混合——真的有 hook，但只涵蓋 busy/idle 轉換；其餘更細的狀態（rate-limit、API error、git conflict、permission prompt……）全靠 PTY/Screen heuristic | Unavailable | Typed/paced，約 2ms/byte | `--continue`——程式碼註解說「operator-verified」，但查無自動化行為測試 | 有——標準 `mcpServers` schema，位於 `.agents/mcp_config.json` | 最新加入的 first-class backend（#987，Gemini 的繼任者）；hooks 曾經失效過，後來才修回來 |
| **Shell** / **Raw(String)** | 無——完全沒有偵測 pattern；`agent_state` spawn 時就硬鎖 `Idle` | 不適用 | Bulk；文字仍會真的寫入並送出（不是真的 no-op），只是沒有 backend 專屬客製化 | 不支援——`args_for()` 回空，Resume 與 Fresh 產生的 spawn args 完全一樣 | 無——這個 backend 完全跳過 MCP config | Utility 層級；CHANGELOG 查無事故記錄（**未查證**這代表「從沒出過包」還是「沒人記錄」） |

---

## ClaudeCode

**Agent-state 訊號**：Hook-based，`authority=Hook`、`confidence=Confirmed`——階梯最高階（`src/daemon/shadow/evidence.rs:65-90,92-109`）。`has_state_hooks()` 只列出 `ClaudeCode` 與 `Agy`（`src/backend.rs:55-76`）。
未查證：初始 ready-gate（`ready_pattern: "bypass permissions|❯"`）是否曾經會參考 hook，還是跟其他所有 backend 的 ready-gate 一樣純靠 screen pattern——沒找到任何 hook-based ready 路徑，但這只是「沒找到」，不等於「確認為否」。

**Context usage**：`ContextProvider::StatusLine`，靠 `CLAUDE_CONTEXT_PATTERN`（`src/backend_profile.rs:39-46,86-92`）——但這個 regex 只吃 fleet 自訂 statusline 格式（`Ctx Used: N%`）。原生 Claude Code 裝機顯示的是反向的「剩餘 %」字串，這個 pattern 刻意不吃——這是尚未實作的落差，不是 bug。

**Submit / inject**：Bulk inject，`submit_key: "\r"`，`typed_inject: false`（繼承 `DEFAULTS`，`src/backend.rs:382-399`；測試陣列 `:1474-1486` 有 pin 住）——跟 Codex/OpenCode/Agy 用 paced typed inject 不同。沒有 pre-send confirm-first 閘門；取而代之的是事後、靠 hook 才會觸發的 delivery-verification watchdog（`src/daemon/inject_delivery.rs:1-20`，30 秒視窗）——實務上只有有 hook 的 backend 才會真的觸發，等於只對 Claude 生效。

**Resume**：`ResumeMode::ContinueInCwd { flag: "--continue" }`（`src/backend.rs:405`），靠讀取 `~/.claude/projects/<encoded-cwd>/*.jsonl` 的 on-disk session 偵測把關（`src/backend.rs:878+`）——查無可恢復 session 時自動降級成 Fresh。

**MCP**：有，`fleet_mcp_supported: true`，透過 spawn 時注入的明確 `--mcp-config <workdir>/mcp-config.json` CLI flag（`src/backend.rs:784-799`），由 `configure_claude` 寫入（`src/mcp_config.rs:177-201`）——是 project-local 的檔案發現機制，不是被動的 `.mcp.json` auto-discovery；全域的 `~/.claude` 完全不會被碰。

**已知脆弱點**：#468（針對操作者文字誤觸自動 dismiss 的錨定 dismiss regex）、#996 Phase 1（trust-dialog 按鍵從 up+up+Enter 改成純 `\r`，起因是一次 37 次訊息重複迴圈事故）、#1001（相關的較早修復）、#1944/#1947/#1948（input-box 空白標記 `❯`，已對照真實 capture 驗證——明確不適用於 Codex）、#2044（inject-delivery watchdog，起因是一次 `/model` picker 悄悄吞掉一次 dispatch）。Pattern 集合最後一次校準對象是 Claude Code `2.1.89`（`src/backend.rs:868`）——這是所有依賴 PTY pattern 的 backend 共有的版本漂移風險。

---

## Codex

**Agent-state 訊號**：純 PTY/Screen heuristic——沒有 lifecycle hook。`has_state_hooks()`（`src/backend.rs:74-76`）沒有列出 Codex。

**Context usage**：Unavailable——`context_pattern: None`（`src/backend_profile.rs:365-415`，`codex_profile()`）。

**Submit / inject**：`submit_key` 預設 `"\r"`；`typed_inject: true`——逐 byte paced 寫入，因為 Codex 的 ratatui 風格 input widget 沒辦法可靠地接受 bulk 寫入（#1670，完整理由見 `src/backend.rs:491-565`，尤其 `508-522`；pin 住的測試 `codex_uses_paced_inject_and_wake_pointer_is_not_a_system_header_1670` 在 `:1934-1963`）。除了這個 pacing 之外，查無 confirm-first/readback 機制——**未查證**。

**Resume**：`resume_mode` 在 `ResumeMode` 層級預設是 `NotSupported`，但 Codex 實際的 resume 是硬編在 `args`/`fresh_args` 裡的 `resume --last`——繞過了其他所有支援 resume 的 backend 都在用的通用 `ResumeMode` 抽象。這點值得提醒未來的維護者這是個不一致之處，不只是個小怪癖。

**MCP**：有，`fleet_mcp_supported: true`（預設值），透過 per-project 的 `.codex/config.toml`（不是全域的 `~/.codex/config.toml`），由 `configure_codex_with_home` 寫入（`src/mcp_config.rs:677-772`）。

**已知脆弱點**：#1670（paced-inject 的根因）、#1944/#1948 系列（ghost placeholder——空的 input box 被誤讀成真正的 prompt marker）、`src/state/mod.rs:2196-2202` 一則沒有編號的發現（ready screen 可能誤鎖 `Active` 之後就不再重新偵測）。使用者提到的「Codex PTY 注入故障」事故——**未查證**，查無完全符合這個描述的獨立 ticket；最接近的候選是 `CHANGELOG.md:237`（#603/PR #629，stdin-only delivery），但這個機制在目前 `src/` 已無痕跡，看起來已被 #1670 取代。

---

## KiroCli

**Agent-state 訊號**：PTY/Screen heuristic 是今天實際驅動 `agent_state` 的機制（`src/backend_profile.rs:220-273`，`kirocli_profile()`）。另外存在一條 `~/.kiro/sessions/cli/<uuid>.jsonl` 的唯讀 tail（`src/daemon/shadow/kiro.rs:1-23`，在 production 中確實有接上，見 `src/app/mod.rs:2774-2797`），但整個 Shadow Observer 系統明文寫著「additive only……從不驅動 `agent_state`」（`src/daemon/shadow/mod.rs:15-16`）——它站在 `Authority::Stream`，低於 `Authority::Hook`。Kiro 完全沒有 lifecycle hook。附帶說明：這條 jsonl 只在 tool-round/turn 結束時 flush（不是 prompt-submit 時），所以一個沒有呼叫任何 tool 的純思考回合，連 Stream-authority 的 shadow 都抓不到中途狀態。

**Context usage**：StatusLine，可用——`KIRO_CONTEXT_PATTERN = r"◔\s*(\d+(?:\.\d+)?)\s*%"`（`src/backend_profile.rs:97`）。`:38-46` 的文件註解明確點名 Claude 與 Kiro 是僅有的兩個 `StatusLine` provider。

**Submit / inject**：Bulk，`submit_key: "\r"`，`typed_inject: false`（pin 住的陣列，`src/backend.rs:1472-1474`）。送出前有固定 50ms 的 sleep（`src/agent/mod.rs:2813-2818`）；`readback_confirm_typed` 機制（#1912，`:2824-2831`）只對 `typed_inject` 的 backend 生效，所以 Kiro 從來不會用到它。

**Resume**：有——`ResumeMode::ContinueInCwd { flag: "--resume" }`（`src/backend.rs:457`）。

**MCP**：有，自己的 auto-discovery——Kiro 讀取 `.kiro/settings/mcp.json`（沒有像 Claude 那樣明確的 CLI flag），由 `configure_kiro()` 透過一個 wrapper script 寫入，理由是「因為 Kiro 會忽略 env block」（`src/mcp_config.rs:321-367`）。

**已知脆弱點**：#7（`SIGWINCH` 後不重繪——唯一需要 `redraw_after_resize` 的 backend）、#996 Phase 2a（trust-modal 預設是破壞性選項；需要 Down+Enter，已對照 fixture byte 分析驗證）、#468（啟動卡住的 dismiss regex）、#1005（完成橫幅誤判 guard）、#1947（`>` input-line 誤鎖引號 guard）、#1948（無 prompt marker 的 placeholder heuristic）、#2413（jsonl observer 的量測研究——即上方分界說明提到的 Shadow Observer 工作）。未查證：#2413 的 jsonl Stream plane 是否也對 Kiro 帶有 token/context-usage 的證據——只找到 turn/tool-lifecycle 層級的 evidence mapping。

---

## OpenCode

**Agent-state 訊號**：純 PTY/Screen heuristic——沒有 structured 訊號。`has_state_hooks()` 不包含 OpenCode（`src/backend.rs:74-76`）；全部靠 regex pattern 比對（`src/backend_profile.rs:282-355`）。

**Context usage**：程式碼明文宣告 Unavailable，不只是「還沒實作」。`context_pattern: None`（`src/backend_profile.rs:352`）；`src/state/mod.rs:1221-1235` 把 Codex/OpenCode/Agy 歸為同一組，寫著「no trustworthy passive context signal」——畫面 footer 確實有顯示 token/cost 字串，但沒有被解析進 `context_pct`。

**Submit / inject**：`submit_key` 預設 `"\r"`（沒有覆寫），`inject_prefix: "\r"`，`typed_inject: true`——逐 byte paced 寫入，跟 Codex（#1670）記錄的理由相同。除了這個 pacing 之外查無 confirm-first/readback 機制——**未查證**。

**Resume**：`ResumeMode::ContinueInCwd { flag: "--continue" }`（`src/backend.rs:571`）。有兩個各自獨立追蹤的事故：
- **#2020（已修復）**：resume 出來的 pane 不顯示「Ask anything」placeholder，只有純粹的 `┃` statusline 行——過去會被誤判成 `AwaitingOperator`。修法是新增一個低優先序的 `ctrl+p commands` Idle pattern，並已做回歸測試（`src/backend_profile.rs:330-340,564-598`）。
- **Dummy-session wedge（未關閉，待重現）**：記錄於 `docs/KNOWN_ISSUES.md:24-46`——OpenCode 上游的一個 bug，`--continue` 可能送出一個 placeholder session id。常見情況已緩解（#1519/#1526，fresh-session fallback），但更罕見的「process 永不退出」變種能躲過全部三層偵測。這就是派工筆記提到的那個 task board 事故：`t-20260702144219394508-56872-6`（`docs/KNOWN_ISSUES.md:41`），結構上追蹤在 #2549 底下。

**MCP**：有——project-local 的 `opencode.json` `"mcp"` 欄位發現機制，`fleet_mcp_supported: true`（pin 住的測試在 `src/backend.rs:1432`），由 `configure_opencode` 實作（`src/mcp_config.rs:599-638`），另外還有一個 auto-approve permission block，避免 OpenCode 自己的「Permission required」提示卡住 MCP tool call。

**已知脆弱點**：#2020（已關閉——resume 後 pane 誤判 idle）與上述 dummy-session wedge（未關閉，`t-20260702144219394508-56872-6`，#2549）。這是兩個各自獨立的 bug，隨口聊天時很容易被混為一談——一個已修，一個還沒。

---

## Agy

**Agent-state 訊號**：混合。真的有 lifecycle hook（`has_state_hooks()` 回傳 true，`src/backend.rs:74-76`），但只在 busy/idle 轉換時觸發（`PreInvocation`→`UserPromptSubmit`/`Stop`，`src/mcp_config.rs:470`）——已有實測證據確認它不帶任何 tool-call 顆粒度的訊號（task board 發現 `t-...93090-0`）。其餘所有更細的狀態（`Active`、`UsageLimit`、`RateLimit`、`ApiError`、`GitConflict`、`PermissionPrompt`）全靠純 PTY/Screen heuristic——8 個依序比對的 regex（`src/backend_profile.rs:129-213`）。

**Context usage**：Unavailable——`context_pattern: None`（`src/backend_profile.rs:209`），明確與 Codex/OpenCode 歸為同一組「no trustworthy passive context signal」（`src/backend_profile.rs:43-45`）。

**Submit / inject**：`typed_inject: true`，`inject_prefix: "\r"`，預設 `submit_key: "\r"`（`src/backend.rs:608-609`）；paced 約 2ms/byte 的分段寫入——`src/backend.rs:519` 那份共用的理由文件明確點名 Agy 跟 Codex 並列。查無 confirm-first/readback 機制。

**Resume**：`ResumeMode::ContinueInCwd { flag: "--continue" }`（`src/backend.rs:613`）——程式碼註解寫著「operator-verified in issue body」，但查無自動化的 CLI 行為測試，只有針對 args 本身的單元測試 pin 住。

**MCP**：有——標準的 `{command, args, env}` `mcpServers` schema，位於 `<workdir>/.agents/mcp_config.json`（`src/mcp_config.rs:391-436`，`mcp_server_entry` 在 `:86-99`）；`fleet_mcp_supported: true`（`src/backend.rs:641`，測試 `:1436`）。在 `src/backend.rs:1416-1421` 發現一則**過時的文件註解**，聲稱 Agy 的 MCP 支援是 `false`/不支援——這已經過時，且被兩行之後的實際斷言直接打臉。值得另外開一個 follow-up 清掉這則註解，跟本文件無關。

**已知脆弱點**：#987（Agy 加入）、#995（workspace-trust dismiss + 一條已死的 `.antigravitycli/` MCP 寫入路徑）、#1547（把真正的 MCP 路徑修正為 `.agents/`）、#1580（Gemini 退役，Agy 是繼任者）、#1523 / #2413 Phase D（hooks 曾經失效，後來才修回來——見上方分界說明）、#2236（quota-wall pattern 順序）、#2409（一個暫時性的高流量 `ApiError` pattern）、#2524 P1b-r1/r2（缺一個 `GitConflict` pattern；`RateLimit` pattern 被標記為低信心/合成，尚未對照真實 Agy 輸出驗證）。

---

## Shell 與 Raw(String)

**Agent-state 訊號**：無。完全沒有偵測 pattern——`agent_state` 在 spawn 時就硬鎖 `Idle`，跳過其他所有 backend 都會走的 `Starting → Idle` 握手（`src/backend_profile.rs:506-520` `empty_profile()`；`src/state/mod.rs:1002-1013`；`src/state/patterns.rs:204`）。

**Context usage**：不適用——`context_pattern: None`，`ContextProvider::Unavailable`（`src/backend_profile.rs:516,44-46`，測試 `:541-542`）。

**Submit / inject**：所有欄位都落回 `DEFAULTS`（`submit_key: "\r"`、`inject_prefix: ""`、`typed_inject: false`；`src/backend.rs:382-399,648-656`）。重要細節：inject 本身**不是真的 no-op**——文字確實會被寫入 PTY 並送出（`src/agent/mod.rs:2776-2818`）。真正 no-op 的是*preset 客製化*（沒有 dismiss pattern、沒有特殊前綴、沒有 readback confirm）。

**Resume**：不支援——`resume_mode: ResumeMode::NotSupported`（`src/backend.rs:389`），`args_for()` 對它回傳空 `Vec`（`:275-277`）——所以 Resume spawn 跟 Fresh spawn 產生的（空）args 完全一樣，等於沒有區別。

**MCP**：無——`mcp_config::configure()` 對 `Backend::Shell | Backend::Raw(_) | None` 直接 return，不呼叫任何 `configure_*` 函式（`src/mcp_config.rs:817-830`）；`fleet_mcp_supported: false`（`src/backend.rs:648-654`）。

**已知脆弱點**：`CHANGELOG.md` 或已追蹤的事故清單裡查無記錄。**未查證**這代表 Shell 真的從沒出過包（有可能——它的 backend 專屬程式碼最少，能壞的地方也最少），還是單純沒人記錄過。附帶一個結構性觀察，不算脆弱點：`Backend::from_command`（`src/backend.rs:663-687`）實務上從不會回傳 `Some(Shell)` 或 `Some(Raw(_))`——它只認得已知的 preset 二進位檔名，所以這兩個 `match` 分支是防禦性的 exhaustive-match；Shell/Raw 實際的執行路徑走的是 `None` 分支。

**Raw(String)**：跟 Shell 共用完全相同的 preset arm 與 `empty_profile()`（`src/backend.rs:648-656`）——行為一致。唯一差異是 `command_string()`（`src/backend.rs:228-229`），它用儲存的路徑字串本身，而不是 `$SHELL`。
