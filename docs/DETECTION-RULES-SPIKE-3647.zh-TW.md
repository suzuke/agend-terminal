[English](DETECTION-RULES-SPIKE-3647.md)

# Spike：畫面偵測規則資料化（issue #3647）

> 這是 issue #3647 的設計 spike。本文只是決策文件，不動生產碼。提案把目前寫死在
> Rust 裡的各 backend 畫面偵測 regex 外部化成每個 backend 一份資料 manifest，
> 狀態機與結構性 false-positive guard 留在程式。本文回答 spike 的九個問題、盤點
> 現有全部 pattern，並把工作切成一連串刀次，讓第一刀行為完全不變。

## 0. 狀態、範圍與非目標

畫面層把渲染後的 pane 分類成 `AgentState`（`src/state/mod.rs:36`）。目前每個
backend 的 pattern 都是 Rust 字面值：`src/backend_profile.rs` 裡共置的
`BackendProfile` bundle、`src/state/patterns.rs:42` 的共用網路錯誤 alternation，
以及 `src/state/mod.rs:781` 的全域 hint token。CLI 一改 banner 措辭，寫死的
regex 就會安靜地停止命中。issue #3534 是動機案例：UsageLimit regex 寫死
`You've reached your Fable 5 limit`，實際 banner 是 `Fable limit`，某個
instance 在死 pane 上空轉 82 分鐘沒被偵測到。

要向 herdr 借的是「規則資料化」＋「比對畫面結構」＋「explain」；它的狀態模型
與無簽章的遠端更新路徑不值得照抄。本 spike 決定這個外部化的形狀。

- 目標：回答下列九問；產出完整盤點；把實作切成第一刀逐位元組相同的搬遷。
- 非目標：側欄 UI（#3645）；toast 或音效通知；遠端熱更新的實作（只決定要不要
  做與其前提）；修改任何狀態語意或新增狀態。
- 交付物：本文件。不含生產碼變更。

## 1. 盤點與切分

目前每一條偵測 pattern 都列在附錄 A。切分原則很簡單：**regex 字面值與 hint
token 清單搬進資料檔；任何控制流、時序、或跨欄位的結構性檢查留在程式。** 資料
可以選擇一條規則要掛哪些具名 guard，但資料不能定義新的 guard 邏輯，所以純資料
的修改永遠無法弱化 guard。

逐層切分如下：

| 層 | 目前位置 | 決定 |
|---|---|---|
| 各 backend 的 `AgentState` regex | `src/backend_profile.rs` 的 `*_profile()` | 搬進資料檔，順序保留 |
| 共用網路錯誤 alternation | `src/state/patterns.rs:42` | 搬進資料檔成具名 fragment，由 rule id 引用 |
| 尾端錨定的 update menu | `src/backend_profile.rs:178` | 搬進資料檔，掛上程式端的 tail-anchor guard |
| context 百分比 regex | `src/backend_profile.rs:108,113` | 搬進資料檔，region 固定為底部狀態列 |
| throttle hint token | `src/state/mod.rs:781` | 搬進資料檔成 token 清單 |
| input-line marker | `BackendProfile.input_line_markers` | 搬進資料檔（本來就是各 backend 一份） |
| 狀態機與優先序 | `src/state/mod.rs:148,2368` | 留在程式 |
| latch、expiry、oscillation guard | `src/state/mod.rs:979-1012,2402` | 留在程式 |
| `HIGH_FP` 判定與 anchor 選擇 | `src/state/mod.rs:563,603` | 留在程式 |
| 紅色 anchor 與內容 anchor | `src/state/mod.rs:1648` | 留在程式 |
| position gate（live tail） | `src/state/mod.rs:1691` | 留在程式 |
| working-marker override | `src/state/mod.rs:1863` | 留在程式 |
| hard-wrap flatten rescue | `src/state/mod.rs:833-963,1751,1780` | 留在程式，guard 語意固定 |
| proximity guard | `src/state/mod.rs:912,944` | 留在程式，由名稱引用 |
| hash-dedup 與 heartbeat gate | `src/state/mod.rs:1515,1186` | 留在程式 |
| 啟動 prompt 結構性 fallback | `src/state/patterns.rs:368` | 留在程式 |
| productive marker 與 behavioral 時序 | `src/behavioral.rs:26,111,179-244` | 本 spike 範圍外，不動 |
| dismiss pattern 與 re-arm hint | `src/backend.rs:321,390` | 相鄰桶，暫且不動 |

pattern 層級的決定因此幾乎一致是 `data`；例外在附錄 A 標出，只有 `data+anchor`
（regex 搬走但掛上具名程式 guard）與 `code`（結構性啟動 prompt 辨識器留在
Rust）。沒有任何狀態、優先序、latch、clear 或 gate 行為被搬進資料。

## 2. Schema 與結構性 guard

Schema 必須能表達既有的結構性 guard，又不能把語意搬進資料。每條規則宣告一個
region、一個 matcher，以及一串**具名 guard**。Guard 是一組封閉詞彙，實作在
Rust；資料只能指名它們，並帶入 loader 會夾限的參數。未知的 guard 名稱是載入
錯誤而非無聲 no-op，所以 manifest 永遠無法安靜地丟掉一個 guard。

```toml
schema = 1
backend = "claude"
source = "builtin"

[[rules]]
id = "claude.usage_limit.model_tier"
state = "usage_limit"
priority = 60
region = "whole_screen"
matcher = { kind = "regex", value = "..." }
guards = ["input_line_excluded", "usagelimit_banner_adjacent"]

[[rules]]
id = "claude.context_full.bounded"
state = "context_full"
priority = 30
region = "error_tail"
matcher = { kind = "regex", value = "..." }
guards = ["red_anchor"]
```

保住現有強度的 guard 詞彙：

| Guard | 語意 | 目前位置 |
|---|---|---|
| `red_anchor` | 某個畫面上的命中至少有一個紅色 cell | `src/state/mod.rs:1648` |
| `error_line_content` | 命中位在錯誤形狀的行上，排除 input 行 | `src/state/patterns.rs:121` |
| `input_line_excluded` | 命中不位在 backend 的 input 行 | `src/state/patterns.rs:153` |
| `usagelimit_banner_adjacent` | 前面要有 box-draw、後面要有 reset stamp，各在 40 字內 | `src/state/mod.rs:944` |
| `throttle_indicator_adjacent` | 命中前 80 字內要有錯誤指示字串 | `src/state/mod.rs:912` |
| `tail_anchored` | 整個區塊位在畫面尾端（`\z`） | `src/backend_profile.rs:178` |
| `position_error_tail` | 命中仍在 live 的最後 15 行內 | `src/state/mod.rs:1691` |
| `status_rows` | 只掃底部狀態列找 context 百分比 | `src/state/mod.rs:530` |

proximity guard 的數值常數留在 Rust，不接受任何由資料提供的值，所以
`usagelimit_banner_adjacent` 無法被本機或遠端 manifest 放寬。`region` 選擇一個
畫面切片的 primitive（`whole_screen`、`error_tail`、`hard_wrap_tail`、
`status_rows`），其視窗就是現有的程式常數。matcher 是 `all`、`any`、`not`
組成的樹，葉節點是 `literal`、`regex`、`line_regex`，所以今天是一條 regex 的
規則就是一條 regex，第一刀完全精確。

## 3. 載入與本機覆寫

內建 manifest 用 `include_str!` 打包進 binary，所以與 release 一起版控。本機
覆寫放在 `~/.config/agend-terminal/detection/<backend>.toml`。

- 覆寫粒度：**整份取代**，不是逐條合併。逐條合併讓「實際跑了什麼」難以重建，
  也可能藏住被丟掉的 guard；整份取代加上 `explain` 讓實際生效的規則集可稽核。
  這沿用 herdr 的先例。
- fail-closed：任何載入失敗——TOML 解析、schema 不合、regex 編不過、未知 guard
  名稱、或缺了健康不變量要求的規則——都回退到內建 manifest 並發出可見警告
  （log 一行、health 欄位、以及 `explain` 的 banner）。壞掉的覆寫永遠不會無聲
  關掉偵測。
- 整份取代仍無法弱化 guard：覆寫受同一組封閉 guard 詞彙與同一批 Rust 端
  proximity 常數約束。
- loader 記下來源（`builtin` 或 `local`）與內容 hash，兩者都由 `explain` 與
  telemetry 揭露，所以生效中的 manifest 永遠可辨識。

## 4. 遠端更新

決定：**這輪不做遠端熱更新。** herdr 的遠端路徑只在啟動檢查一次，且沒有簽章或
雜湊驗證，等於把偵測規則變成遠端可寫、能左右 daemon 行為的控制通道；對一個
很少變動的規則檔來說，這個風險不值得冒。

若日後重提，前提依序是：(a) 對 manifest 的簽章，用 binary 內固定的公鑰驗證；
(b) 單調遞增的版本號並拒絕回滾；(c) 與本機覆寫相同的 fail-closed 載入驗證；
(d) 明確的 operator kill-switch。四個都到位之前，輸入只有內建 manifest 與本機
覆寫，兩者本來就受 operator 控制。

## 5. explain 介面

兩個介面都出，因為兩者服務不同呼叫者：

- CLI：`agend-terminal state explain [target] [--file SCREEN_FILE] [--json]`。
- MCP tool：`explain_state`，給想要同一份資料的 agent 與 orchestrator 用。

`--file` 形式就是回歸測試入口：它把一段擷取的原始畫面（或已經渲染好的 pane
dump）餵進同一個分類器並印出穩定紀錄，所以 fixture 可以斷言 golden 輸出。
欄位如下：

| 欄位 | 意義 |
|---|---|
| `backend` | 被評估 manifest 的 backend |
| `manifest_source` | `builtin` 或 `local` |
| `manifest_hash` | 生效 manifest 的內容 hash |
| `region` | 每條規則被評估時用的畫面 region |
| `rules` | 每條 rule id 與是否命中，以及其證據片段 |
| `guards` | 每條規則的 guard 判定，以及任何被拒的原因 |
| `winner` | 勝出的 rule id、狀態與優先序，或無 |
| `fallback_reason` | 為何沒 latch：無命中、anchor 失敗、位置過舊、dedup 跳過、或 latch 維持中 |
| `throttle_hint` | 便宜的 hint 前置過濾是否觸發 |
| `hard_wrap_rescue` | flatten rescue 是否執行過，以及找到什麼 |

`explain` 是唯讀的，且絕不碰 `State::current`，符合 shadow observer 已遵守的
cycle-proof 不變量（`src/daemon/shadow/mod.rs:88`）。

## 6. 與 hook 權威的關係

優先序不變。畫面偵測產生 `raw` 狀態；Shadow Observer 的 hook 或 stream 證據只能
透過既有的共用 gate（`src/daemon/shadow/gate.rs:95`）覆寫它。該 gate 只在
authority 為 `Hook` 或 `Stream`、confidence 為 `Confirmed` 或 `Strong`、raw
畫面不是權威性 gate 畫面、且 observed 狀態在粗略層級確實不同時才會觸發。不帶
任何 rate-limit 資訊的 hook 訊號在證據契約裡是明寫的
（`src/daemon/shadow/evidence.rs:38,152`）。

| 畫面狀態 | hook 或 stream 可否覆寫 | 原因 |
|---|---|---|
| `Approval`、`PermissionPrompt` | 否 | 人工 gate 永遠是權威 |
| `UsageLimit`、使用者 `RateLimit` | 否 | operator 必須永遠看到這道牆 |
| `ServerRateLimit` | 只有 fresh 的 post-SRL Active episode 可以 | 過期 banner 不該釘住已恢復的 agent |
| `Active`、`Idle`、`AwaitingOperator` | 可以 | 即時 lifecycle plane 更強 |
| `ApiError`、`ContextFull`、`ModelUnsupported`、`AuthError`、`GitConflict` | 實務上只能靠畫面 | 沒有 hook 或 stream plane 可靠地發出它們 |

外部化不會移動這條線：它改變的是 `raw` 怎麼算，永遠不是 `raw` 與 `observed`
怎麼合併。這也是為何 UsageLimit 與 RateLimit 仍是只能靠畫面的狀態，必須維持強
fixture（第 7 節）。

## 7. 測試策略

已有兩層，應該擴充而非取代。

- 單元 fixture：`src/state/tests.rs` 裡的 inline 正負 pane，例如 #3534 那對
  `src/state/tests.rs:2952-3028`。
- 語料回放：`tests/fixtures/state-replay/*.raw` 搭配 `MANIFEST.yaml` 的預期轉移，
  由 `replay_manifest_regression`（`src/state/tests.rs:1778`）經 vterm 與
  `StateTracker` 驅動；存在性由 `tests/state_pattern_coverage.rs` 強制。

外部化規則的新強制：

- 每條 rule id 必須在語料索引宣告至少一個正向與一個負向 fixture。新增一個
  invariant 測試，在 CI 列舉 manifest 規則，當某條規則沒有正向或沒有負向
  fixture、或某個 fixture 沒引用任何規則時失敗。
- 改了規則卻沒更新其 fixture 會讓該測試失敗，因為 fixture 需求跟著 rule id 走。
- banner 類規則（`UsageLimit`、`RateLimit`、`AuthError`）必須至少有一個真實
  擷取的正向 fixture，且必須同時以結構與措辭命中。

這能從根本防住 #3534 那類失敗嗎？部分是，而且值得誠實說明極限。CI 無法知道
廠商改了 banner 文字。語料 gate 保證的是：被改的規則帶著證據抵達，且以結構
為鍵的規則保留一個負向引用 fixture。對抗 #3534 的真正防線是結構鍵本身
（box-draw chrome 加上 `/usage-credits` remedy，模型名自由），而語料 gate 把
這個紀律強加在每一條將來的 banner 規則上。選配的後續是定期重新擷取的 canary，
比對 live banner 形狀與語料；那需要 live CLI，屬於 CI 之外。

## 8. 防抖

herdr 在約 700 ms 內對 Working 到 Idle 連續確認三次。本程式碼庫已有更強且更便宜
的等效機制：screen hash dedup gate 跳過相同的重繪
（`src/state/mod.rs:1515`），transition 函式在降優先序前套用被動 5 秒或主動
2 秒的最短維持（`src/state/mod.rs:2381`），oscillation guard 抑制 Active 的
反覆跳動（`src/state/mod.rs:2402`），latch 的狀態則有自己的計時器到期
（`src/state/mod.rs:979-1012`）。

決定：不加全面性的多次取樣確認。那會給錯誤偵測加上延遲，而錯誤偵測的即時
latch 是刻意的，也會重複已有的 hysteresis。唯一合理的缺口是某條已知 idle
marker 會閃爍的特定規則；若真出現，schema 增加一個選配的 per-rule
`confirmations` 次數，只用在 fixture 證明有需要之處。那是後續，不是第一刀。

## 9. 遷移路徑

第一刀是純資料抽取，且必須行為完全相同。

1. 逐 backend 產生一份 manifest，依序轉錄現有 `*_profile()` 的 pattern 向量，
   把陣列順序映射成嚴格遞減的 priority。共用的 alternation 保留為具名
   fragment。
2. 讓 `StatePatterns::for_backend` 經同一條 `Regex::new` 管線從 manifest 編譯，
   且所有 guard 呼叫點不變。
3. 用一個臨時 parity 測試證明同一性：把 legacy pattern 向量凍結在測試碼裡，對
   每個 backend 斷言載入的 manifest 產生相同順序的 `(state, regex 來源)` 清單。
   profile 搬遷列車（#1683 及其後續）在刪除 legacy 來源前用的正是這個逐位元組
   同一 harness。
4. 之後才個別改規則，每條都帶自己的正負 fixture。

因為 manifest 經既有管線編譯、guard 未動，第一刀之後任何畫面的判定都不變。

## 附錄 A — 完整 pattern 盤點

分類說明：`data` 原封不動搬進 manifest；`data+anchor` 搬走並掛上具名程式
guard；`data(field)` 是既有的每 backend 資料欄位；`code` 留在 Rust。來源行若無
特別註明即為 `src/backend_profile.rs`。

```text
BACKEND  STATE             RULE-ID                            CLASS        PATTERN / SOURCE
grok     PermissionPrompt  grok.perm.trust                    data         Run Grok Build in a project directory\?|Do you trust
grok     Active            grok.active.stop                   data         \[stop\]|Ctrl\+c:cancel|Esc:cancel|Ctrl\+;:queue
grok     Idle              grok.idle.legacy                   data         Turn completed in \d|Space:prompt|Enter:open
grok     Idle              grok.idle.footer                   data         (?m)^[ \t]*Shift\+Tab:mode[ \t]*│[ \t]*Ctrl\+\.:shortcuts[ \t]*$
agy      UsageLimit        agy.usage_limit.quota              data         Individual quota reached|Contact your administrator to enable overages
agy      RateLimit         agy.rate_limit.capacity            data         exhausted your capacity on this model
agy      ApiError          agy.api_error.high_traffic         data         servers are experiencing high traffic
agy      PermissionPrompt  agy.perm.request                   data         Requesting permission for:|Do you trust the contents of this project|tab Amend · e edit command
agy      GitConflict       agy.git_conflict.merge             data         Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in
agy      Active            agy.active.tool_bullet             data         ●\s+[A-Z][a-zA-Z]+\(
agy      Active            agy.active.esc_cancel              data         esc to cancel
agy      Idle              agy.idle.shortcuts                 data         \? for shortcuts
agy      Idle              agy.idle.chrome                    data         Antigravity CLI|Type your message
kiro     AuthError         kiro.auth.not_authenticated        data         Not authenticated|AccessDenied|denied access
kiro     UsageLimit        kiro.usage_limit.service_quota     data         ServiceQuotaExceeded|InsufficientModelCapacity|you have reached the limit
kiro     RateLimit         kiro.rate_limit.throttle           data         Too Many Requests|ThrottlingError|ThrottlingException|Rate exceeded|\b429\b
kiro     ServerRateLimit   kiro.server_rate_limit.net_errors  data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
kiro     ContextFull       kiro.context_full.overflow         data         context window overflow|compacting context
kiro     ModelUnsupported  kiro.model_unsupported.sentence    data         (?m)^\s*The\s+model\s+'[^'\r\n]+'\s+is\s+not\s+available\.\s+Please\s+use\s+'/model'\s+to\s+select\s+a\s+different\s+model\s+and\s+try\s+again\.
kiro     PermissionPrompt  kiro.perm.approval                 data         requires approval|ESC to close \| Tab to edit
kiro     GitConflict       kiro.git_conflict.merge            data         shared merge literal (see agy.git_conflict.merge)
kiro     Active            kiro.active.tools                  data         execute_bash|fs_read|fs_write
kiro     Active            kiro.active.working                data         Kiro is working|esc to cancel
kiro     Idle              kiro.idle.pct                      data         ◔\s*\d+(?:\.\d+)?%\s*$|ask a question or describe a task
kiro     Idle              kiro.idle.trust_all                data         Trust All Tools active|/quit to exit
opencode RateLimit         opencode.rate_limit.api            data         API rate limited \(429\)|Rate limited\. Quick retry|API rate limit exceeded
opencode ServerRateLimit   opencode.server_rate_limit.net     data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
opencode UsageLimit        opencode.usage_limit.quota         data         Quota Limit Exceeded|monthly usage limit reached
opencode AuthError         opencode.auth.invalid_key          data         Invalid API key
opencode ApiError          opencode.api_error.provider        data         Error from provider:|request validation errors
opencode ContextFull       opencode.context_full.overflow     data         ContextOverflow
opencode PermissionPrompt  opencode.perm.required             data         Permission required|Allow once\s+Allow always\s+Reject
opencode GitConflict       opencode.git_conflict.merge        data         shared merge literal (see agy.git_conflict.merge)
opencode Active            opencode.active.tool_marker        data         ✱\s+(Read|Write|Edit|Glob|Grep|Bash|List|Task)\b|~\s+(Reading|Writing|Editing|Searching|Listing|Globbing|Grepping)\b
opencode Active            opencode.active.esc_interrupt      data         esc interrupt
opencode PermissionPrompt  opencode.perm.update               data         Update Available|Skip\s+Confirm
opencode Idle              opencode.idle.ask_anything         data         Ask anything
opencode Idle              opencode.idle.ask_anything_tab     data         Ask anything|tab agents
opencode Idle              opencode.idle.statusline           data         ctrl\+p commands
codex    AuthError         codex.auth.api_key                 data         OPENAI_API_KEY|Incorrect API key|invalid_api_key
codex    UsageLimit        codex.usage_limit.hit_limit        data         hit your usage limit|try again at
codex    RateLimit         codex.rate_limit.hit_rate          data         rate_limit_exceeded|RateLimitError|hit your rate limit
codex    ServerRateLimit   codex.server_rate_limit.net        data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
codex    ContextFull       codex.context_full.overflow        data         ContextOverflow
codex    ModelUnsupported  codex.model_unsupported.invalid    data         invalid_request_error|model is not supported|Model metadata for .*? not found
codex    PermissionPrompt  codex.perm.run_command             data         Would you like to run the following command\?|Press enter to confirm or esc to cancel|No, and tell Codex what to do differently
codex    PermissionPrompt  codex.perm.update_menu             data+anchor  CODEX_UPDATE_MENU_LIVE (src/backend_profile.rs:178), guard tail_anchored
codex    GitConflict       codex.git_conflict.merge           data         shared merge literal (see agy.git_conflict.merge)
codex    Active            codex.active.working               data         Working|esc to interrupt
codex    Idle              codex.idle.prompt                  data         ›
codex    Idle              codex.idle.chrome                  data         OpenAI Codex|gpt-.*left
claude   AuthError         claude.auth.api_key                data         Invalid API key|invalid x-api-key|authentication_error|authentication failed|OAuth token has expired|Please run /login|API Error: 40[13]\b
claude   ServerRateLimit   claude.server_rate_limit.temp      data         Server is temporarily limiting requests|temporarily limiting.*not your usage|API Error: 5\d{2}\b|server-side issue.*temporary|API Error: Repeated 529 Overloaded|overloaded_error|api_error|timeout_error
claude   ServerRateLimit   claude.server_rate_limit.net       data         SERVER_RATE_LIMIT_NET_ERRORS (src/state/patterns.rs:42)
claude   RateLimit         claude.rate_limit.429              data         API Error: Request rejected \(429\)|rate_limit_error|hit a rate limit
claude   UsageLimit        claude.usage_limit.model_tier      data+anchor  ⎿[ \t]+You've reached your [^\n]{0,32}? limit\.[ \t]+Run /usage-credits|You've hit your session limit|You've hit your weekly limit|You've hit your Opus limit|Credit balance is too low|credit_balance_too_low; guard usagelimit_banner_adjacent
claude   ContextFull       claude.context_full.bounded        data         compacting context|context.{0,16}(full|limit)
claude   PermissionPrompt  claude.perm.amend                  data         Esc to cancel · Tab to amend|allow all edits during this session|Enter to confirm · Esc to cancel
claude   GitConflict       claude.git_conflict.merge          data         shared merge literal (see agy.git_conflict.merge)
claude   Active            claude.active.spinner              data         (?m)^(?:[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+(?:Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)|[✓●⏺]\s+(?:Listing|Reading|Writing|Searching|Editing))\b
claude   Active            claude.active.sparkle              data         (?i)[✻✢✶✳✽*·]\s*\w+\x{2026}|\w+\x{2026}\s*\((?:\d+[smh]|running )|thought for [0-9]+s
claude   Idle              claude.idle.prompt                 data         ❯
claude   Idle              claude.idle.bypass                 data         bypass permissions
claude   telemetry         claude.context.pct                 data(field)  CLAUDE_CONTEXT_PATTERN (src/backend_profile.rs:108), region status_rows
kiro     telemetry         kiro.context.pct                   data(field)  KIRO_CONTEXT_PATTERN (src/backend_profile.rs:113), region status_rows
all      global            global.throttle_hint_tokens        data         THROTTLE_HINT_TOKENS (src/state/mod.rs:781)
all      Starting          generic.startup_prompt             code         (?i)\(y/n\)|\(yes/no\)|\[y/n\]|press\s+(enter|return|any\s+key) (src/state/patterns.rs:368)
```

## 附錄 B — 實作刀次與依賴

| 刀 | 變更 | 依賴 | 同一性證據 |
|---|---|---|---|
| 1 | Manifest schema 與 loader；把所有 pattern 轉錄進內建 manifest | 無 | 凍結的 legacy 向量等於每個 backend 載入的 `(state, regex)` 清單 |
| 2 | 讓 `StatePatterns::for_backend` 走 loader；掛上具名 guard | 刀 1 | 全 fixture 回放不變；所有 state 單元測試綠 |
| 3 | 加入 rule 對 fixture 的語料索引與 CI 覆蓋 invariant | 刀 1、刀 2 | 新 invariant 測試對沒有正或負 fixture 的規則失敗 |
| 4 | 加入本機覆寫載入，含 fail-closed 回退與警告 | 刀 2 | 載入失敗測試回退內建並警告 |
| 5 | 加入 `state explain` CLI 與 `explain_state` MCP tool | 刀 2 | 對語料 fixture 的 golden `--file` 輸出 |
| 6 | 個別修改規則，每次一份 manifest 編輯 | 刀 3、刀 5 | 每次變更都帶自己的正負 fixture |

刀 1 就是 spike 要求的行為不變搬遷。刀 4 到刀 6 在刀 2 落地後彼此獨立。

## 附錄 C — 決策摘要

| 問題 | 決定 |
|---|---|
| 1 盤點與切分 | regex 與 token 清單進資料；狀態機、latch、guard 與 rescue 邏輯留程式 |
| 2 Schema | 封閉的具名 guard 詞彙；資料選 guard，Rust 定義其語意與 proximity 常數 |
| 3 載入與覆寫 | 內建用 `include_str!`；本機整份取代；fail-closed 回內建並發可見警告 |
| 4 遠端更新 | 暫不做；若做，簽章加單調版本加 fail-closed 載入加 kill-switch 為前提 |
| 5 explain | CLI 與 MCP 都出，共用一個分類器；`--file` 是回歸入口 |
| 6 Hook 權威 | 不變；hook 只能經既有 gate 覆寫 `raw`，UsageLimit 與使用者 RateLimit 仍只靠畫面 |
| 7 測試策略 | rule 對 fixture 覆蓋 gate 加上真實擷取 banner fixture；結構鍵才是 #3534 的真正防線 |
| 8 防抖 | 不加全面確認；沿用 hash dedup、min-hold 與 oscillation guard，只在 fixture 證明需要時加 per-rule 確認 |
| 9 遷移 | 第一刀是凍結向量 parity harness 證明的逐位元組抽取 |
