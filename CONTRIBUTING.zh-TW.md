[English](CONTRIBUTING.md)

# 貢獻指南

感謝你考慮參與貢獻。這是一個 Rust CLI + daemon 專案；以下工作流程能讓 diff 易於 review、並維持 CI 綠燈。

## 建置與測試

```bash
cargo build                          # debug build
cargo build --release                # release (strip + LTO, matches CI)
cargo test                           # unit + integration + MCP round-trip
cargo test --bin agend-terminal      # unit tests only
cargo test --test integration        # integration tests (Unix-only for now)
cargo fmt --check
cargo clippy -- -D warnings          # must be warning-free
```

`cargo clippy` 會強制 `unwrap_used = "deny"`（參見 `Cargo.toml`）。請用 `?` / `anyhow::Result` 來處理錯誤。

CI 會在 Ubuntu + macOS + Windows 上複製這些步驟（`.github/workflows/ci.yml`）。

### 推送前：`scripts/preflight.sh`

執行這個一次到位、與 CI 對齊的 preflight，把失敗在本地就抓出來，而不是等到推送之後：

```bash
scripts/preflight.sh          # full matrix; --quick skips the Windows cross-check
```

它執行的正是 CI 的 `check` job——`cargo fmt --check`、`cargo clippy --all-targets --features tray -- -D warnings`、`cargo test --tests --features tray`，以及一個 **Windows cross-check**（`x86_64-pc-windows-msvc`），用來抓出 unix 開發機原本會漏掉的 Windows-only 編譯錯誤。Windows 這一步優先採用 [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin)（`cargo install cargo-xwin && rustup target add x86_64-pc-windows-msvc`），因為有一個傳遞性 C 依賴（`ring`）在沒有 MSVC toolchain 的情況下無法在 macOS/Linux 上交叉編譯；若未安裝，這一步會 SKIP 並附上提示，而不是誤判為失敗。

### 覆蓋率（選填，本地）

`coverage` CI job（#686）會在 Ubuntu 上跑 `cargo-llvm-cov`，並把 lcov 報告上傳到 Codecov。要在本地量測：

```bash
cargo install cargo-llvm-cov                  # one-time
cargo llvm-cov --workspace --tests            # text summary
cargo llvm-cov --workspace --tests --html     # HTML report at target/llvm-cov/html/index.html
```

覆蓋率只供觀察用——不是 merge 的關卡。讓專案覆蓋率下降超過 2% 的 PR 會在 Codecov 上顯示為紅色狀態；merge 與否的決定權仍在 reviewer。PR 中的新程式碼預期要達到 80% 覆蓋率（可在 `codecov.yml` 中設定）。

## 工作流程

1. **從 `main` 開分支**——絕不直接修改 `main`。相較於就地切換分支，建議使用 worktree：
   ```bash
   git worktree add ../agend-terminal.feature feature/<short-name>
   cd ../agend-terminal.feature
   ```
2. **保持 commit 原子化。** 一個 commit 只做一個邏輯變更；每次 commit 前都跑測試。
3. **Commit 訊息前綴**——沿用既有歷史：
   - `feat:` 新功能
   - `fix:` 修 bug
   - `refactor:` 不改變行為
   - `test:` 只動測試
   - `docs:` 只動文件
   - `style:` 格式調整（`cargo fmt`）
   - `ci:` GitHub Actions / 工具鏈
   - `chore:` 雜務整理
   - `merge:` PR 風格的彙整（用於合進多輪 review 時）
4. **PR 描述**——說明改了什麼、為什麼改。把 bug fix 連結到證據（stack trace、重現步驟、修復前會失敗的測試）。

## Review 流程

以下是你開 issue 或 PR 之後可以預期的狀況——目標是快速、具體的回饋，而不是繁文縟節。

- **先搜尋；這能讓你最快得到答案。** 在提交之前，先翻一翻開啟中*和已關閉*的 issue，以及相關範圍的近期 commit/PR（issue 和 PR 模板都帶有一份既有成果的 checklist，正是為此而設）。重複或已有定論的提案可能會被關閉；但對某個已關閉主題提出真正*新*的切入角度是受歡迎的——只要說清楚有什麼不同。我們進展最快的外部貢獻，往往是以一段有禮、證據先行的 RCA 開頭——*「這是症狀、這是我追到的 `file:line`、這是我提議的方向」*——而不是直接丟出一大塊沒人要求的 patch。這能讓 maintainer 在你投入大量時間寫程式碼之前，先為方向開綠燈。
- **Review 會帶著結論和證據回來。** Reviewer 會回覆 `VERIFIED` / `REJECTED` / `UNVERIFIED` 其中之一，外加一個證據區塊——他們跑過的指令（`cargo test`、`clippy`、`gh pr checks`），或每項發現背後的 `file:line` 引用。你不必去猜「看起來不錯」是什麼意思；發現都是具體且可重現的。處理完之後就推送——我們**力求當日完成 review**（這是目標，不是 SLA——這是個小專案）。`REJECTED` 是一份具體的待辦清單，不是把門關上。
- **事實陳述是有待驗證的主張，不是證據。** 你在 PR 內文或程式碼註解裡寫下的一句*「這不可能發生」*／*「這是唯一的呼叫端」*／*「X 永遠到不了」*，都會被 reviewer 對照實際程式碼來檢查——guard、match arm、呼叫端——而你被預期要先自己查過。我們靠這個方式經常抓到自信卻錯誤的論點（一個「compaction 會弄丟這個」其實指向死碼；一個「單一瓶頸點」其實有好幾個呼叫端繞過）。在寫下主張之前先把它追溯到原始碼——這能省下一輪 review。
- **測試要透過真正的進入點來證明行為。** 回歸測試應該以 production 的方式去驅動程式碼——透過真正的函式——而不是手動注入它本應自行算出的內部結構。一個 mock 掉它聲稱要涵蓋的探索/接線邏輯的測試，證明的是那個 helper，而不是那個功能。格式忠實度的規則請見下方的 **測試期望**。
- **高風險區域可能會經過不只一次 review。** 對狀態偵測、並行/鎖、或命令授權/安全性的變更，可能會交給兩位 reviewer——有時跑在不同的 AI backend 上。這不是不信任你;而是這些區域的標準門檻,因為逐行的 diff review 可能會漏掉設計層面的瑕疵。敏感變更多跑幾輪是正常的,不是警訊。
- **停滯的 PR：我們會幫忙接力推進，而作者身分仍然是你的。** 如果 review 要求了某項變更、但大約一個工作日都沒有回應，maintainer 可能會 rebase 並把 PR 接力推進，以免它停滯——**你的作者身分始終會被保留**（你的 commit 保留你的名字），而且你隨時可以留言把 PR 收回。我們寧願把一個好的貢獻往前推進，也不願讓它爛掉;這是事先講明的預期,不是接管。

## 範圍紀律

- 不要把 PR 擴張到超出所述的變更。一個 `fix:` commit 不應該順手去改名無關的型別。
- 不要為不可能發生的情況加上臆測性的抽象、設定旗標或錯誤分支。緊湊風格的偏好請參考既有程式碼。
- 註解一律只用英文。

## 測試期望

- **Bug fix** → 一個在修復前會失敗、修復後會通過的回歸測試。daemon 層級的行為放 `tests/integration.rs`;單元測試用每個模組的 `#[cfg(test)]`。
- **格式忠實度——拿 consumer/parser 去測*producer 的真實輸出*，絕不用手工捏造的結構。** 當一個測試在演練的程式碼會去消費另一個函式產生的 string/struct（一個 parser、一個 matcher、一個針對 wire format 的 predicate）時,要透過呼叫 producer 來建構輸入——不要手寫期望的結構。在 review 時要問:*「這個 fixture 跟 production 實際送出的東西一模一樣嗎?」*這就是 #1483 那一類的假綠燈:`notification_is_actionable_wake` matcher 鎖定的是 body 裡的方括號標記,而真正的 `[AGEND-MSG-PENDING]` pointer 根本不含這些標記,它的測試又手工捏造了那個永遠不會送出的結構——於是 matcher 是死碼而測試卻一直綠著(後來 #1487 加上一個手工字串沒有的 `now=` 欄位時又再次漂移)。修法是:讓 producer 和測試都走同一個 builder(`build_pending_pointer`),這樣在一處新增的欄位就會在各處都被演練到。手工捏造的輸入用來測 parser 對*格式錯誤/邊界*輸入的處理(空的/缺漏的欄位)仍然沒問題——只是要把 happy-path 的契約釘在真正的 producer 上(參見 `extract_msg_id_round_trips_real_format_header`)。
- **新的 MCP tool** → 在 `src/mcp/handlers/` 底下對 handler 做單元測試,並在 `tests/mcp_bridge_client_handshake.rs`(handshake/framing)或 `tests/mcp_proxy_parity.rs`(daemon-proxy parity)中演練 bridge 的 wire path。舊有的 `agend-terminal mcp` subcommand 已在 Sprint 56 Track I-Phase2c(#531)中被硬移除;`agend-mcp-bridge` 是規範的 wire 進入點。
- **新的 CLI flag** → 在 `tests/integration.rs` 或一個聚焦的單元測試中涵蓋它。
- **Test fixture**——用 `std::env::temp_dir()` + `std::process::id()` 來隔離,絕不寫死 `/tmp/...`。需要清理的測試應該明確 `drop` 掉 temp dir,或使用 scope guard。
- **確定性的等待,不是 sleep。** SOP 1（§3.20）禁止在等待非同步狀態的測試中使用
  `thread::sleep(N)` 模式。
  改用既有的 `pub(crate)` 原語——它們以較快的
  頻率（10 ms）輪詢、帶有上限的逾時,並回傳你可以斷言的 `bool` /
  `Option<T>`：
  - `admin::cleanup_zombies::poll_until_dead(pid, timeout) -> bool`
    （#934）——等待子程序結束（Unix 上 kill -0 /
    Windows 上 OpenProcess）。
  - `api::handlers::instance::await_sentinel_nonempty(path) -> Option<String>`
    （#949）——等到 sentinel 檔案有非空內容為止。注意這次
    改名:#949 之前這個 helper 是以檔案*存在*來命名,但
    instance-boot 的呼叫端需要的是*內容*已經就位。
  - 新增 fixture 時,先檢查既有的原語,而不要自己捲一個 sleep 迴圈。

## 擷取新的 fixture（#704 子任務 1）

真實 PTY 的擷取會擴充 `tests/fixtures/state-replay/` 中的回歸語料庫,並把 backend 輸出的漂移鎖定下來。擷取流程是操作端的、選擇性加入的。

### 何時擷取

- **新 backend**——每個預設 backend 至少需要一份 `*-thinking.raw` + `*-tooluse.raw` 基線,讓 `replay_manifest_regression` 能涵蓋它（#987 的 agy 是最近一次新增）。
- **新的狀態偵測 regex**——新增狀態模式時（例如某種 Thinking spinner 形狀）,擷取一份能演練它的 fixture,讓 regex 不會在 CLI 更新間悄悄漂移。
- **重現回歸**——為任何操作端回報的狀態偵測 bug 建立一份 fixture,讓修復後的回歸測試有真實的 byte stream 可以重播。
- **F9 語料缺口（#1014）**——每個 backend 的 productive-marker-fire 與 productive-silence 情境;錄製食譜請見 [#1014](https://github.com/suzuke/agend-terminal/issues/1014)。

### 5 步食譜

1. **啟用擷取**（隱私警告會在 daemon 啟動時觸發）：
   ```bash
   export AGEND_CAPTURE_FIXTURES=1
   agend-terminal start --agents capture-target:<backend>
   ```
   `<backend>` 是 `claude` / `kiro-cli` / `codex` / `opencode` / `agy` 其中之一。

2. **驅動目標狀態。** 與 agent 互動,直到它畫出你想修的那個畫面。例如:提示一個 tool call 來讓完成橫幅出現;在 prompt 中途暫停以擷取 Thinking spinner;觸發錯誤路徑以擷取 rate-limit 橫幅。

3. **把擷取提升**成帶有 v2 量測標籤的 fixture：
   ```bash
   agend-terminal capture promote \
     $AGEND_HOME/captures/capture-target/*.cap \
     <scenario-name> \
     --scenario-kind <productive_marker_fire|productive_silence|silent_stuck> \
     --expected-hung <hung|not_hung> \
     --scenario-description "<one-line summary>"
   ```
   Promote 會寫出 `tests/fixtures/state-replay/<scenario-name>.raw`,並把一個 schema-v2 條目附加到 `tests/fixtures/state-replay/MANIFEST.yaml`。Phase 1a（#1020）交付了這個 CLI。`priority_oscillation` 保留給未來的量測類別,目前不是有效的 `--scenario-kind` 值——在這裡列出它之前,先把它加進 #1020 的 parser。

4. **commit 前先檢查 .raw bytes。** 擷取內容包含原始的 PTY 輸出,包括你的 prompt 和任何 tool 輸出。打開檔案（`less tests/fixtures/state-replay/<scenario-name>.raw`）並掃描以下內容:
   - 錯誤路徑中被回顯的 API key / OAuth token
   - 含有你使用者名稱的檔案路徑（`/Users/<you>/...`）
   - prompt 中提到的內部 URL / Slack handle / 客戶名稱
   - 任何來自私有 repo、你不想公開的東西

   如果發現任何敏感內容:刪掉擷取、修掉 prompt、重新擷取。目前還沒有內建的清理工具——操作端的審查就是 v1 的安全網。

5. **commit + PR**：
   - Stage `tests/fixtures/state-replay/<scenario-name>.raw` + `tests/fixtures/state-replay/MANIFEST.yaml`
   - 跑 `cargo test --bin agend-terminal corpus_measurement_smoke_f9_marker_signals` 確認煙霧測試 harness 的分類與 `--scenario-kind` 相符
   - 在 PR 內文中註明擷取所用的分支與錄製條件,讓未來的操作員在漂移逼得重新擷取時可以重播

### 隱私與儲存

- 擷取會落在 `$AGEND_HOME/captures/<agent>/<epoch_ms>.cap` + 旁附的 `.meta.json`。
- 每個 agent 的輪替預算為 50 MB（最舊 mtime 優先）;參見 `src/capture.rs::rotate_captures`。
- 可調的退出方式:`unset AGEND_CAPTURE_FIXTURES` 會讓 daemon 回到零開銷的 NoOp。
- F9 fixture 缺口（每個 backend 的真實 PTY productive marker）請見 [#1014](https://github.com/suzuke/agend-terminal/issues/1014),agy MCP 整合限制則見上游 tracker。

## 風格

- 一律 `cargo fmt`。CI 會在未格式化的 diff 上失敗。
- `cargo clippy -- -D warnings`——修掉 warning,不要 `#[allow]` 掉它們,除非該檢查確實有誤,並留一行註解說明原因。
- 非測試程式碼中不要有 `unwrap()` / `expect()`。用 `?` 搭配 `anyhow::Context` 做錯誤標註。
- production 程式碼路徑中不要有 `println!` / `eprintln!`。用 `tracing::{info, warn, error, debug}`。
- 讓模組職責保持緊湊:
  - `src/agent_ops.rs`——共用 helper（messaging、fleet 變更、分支驗證）,由 daemon API 和 MCP handler 路徑共同呼叫。新的重複邏輯放這裡,而不要在兩處內聯;`tests/no_dual_track_drift.rs` 強制 `src/agent_ops.rs` 與 `src/mcp/handlers.rs` 之間不得漂移。
  - `src/api/`——daemon 的 JSON 控制 API（wire protocol + `src/api/handlers/` 底下的 per-method handler）。
  - `src/mcp/`——給 agent 的 MCP 介面。`handlers.rs` 把大多數 tool call proxy 到 daemon API;`start_instance` 自 Task #12 起在那裡內聯處理（沒有獨立的 `ops.rs`）。
  - `src/<area>.rs`——領域邏輯（agent、fleet、telegram、health、schedules……）。

## 文件

- 架構變更 → 更新 `docs/architecture.md`。
- 新 CLI 指令 → 更新 `docs/CLI.md` 和 `README.md` 的指令表。
- 新 MCP tool → 更新 `docs/MCP-TOOLS.md` 和 `README.md` 中的 MCP Tools 表。
- 重大的使用者面向變更 → 在 `CHANGELOG.md` 的 `## [Unreleased]` 底下加一條。
- Plan / eval 文件（`docs/PLAN-*.md`、`docs/EVAL-*.md`）代表的是某個時間點的意圖——工作交付後,更新狀態或把該文件併入。

## 發布

Tag 由 release workflow（`.github/workflows/release.yml`）在 `main` 上建立。打 tag 前:

1. 在 `Cargo.toml` 中升 `version`。
2. 把 `## [Unreleased]` 的條目移到 `CHANGELOG.md` 中一個新的 `## [x.y.z] — YYYY-MM-DD` 標題底下。
3. Commit、push、打 tag `vX.Y.Z`,讓 workflow 發布。

## Agent 輔助開發

這個 repo 同時也作為 AgEnD 本身的宿主。Agent 設定和 worktree 位於以下位置:

```
.agents/        .continue/      .factory/       .kiro/
.claude/        .worktrees/     fleet.yaml
```

以上全部都被 `.gitignore` 掉了。不要 commit 它們。