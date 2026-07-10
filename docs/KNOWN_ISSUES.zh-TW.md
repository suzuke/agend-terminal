[English](KNOWN_ISSUES.md)

# Known Issues — 已知問題

這是一份持續更新的清單，列出**已知但目前刻意不處理**的問題，並說明原因以及
在什麼條件下會重新考慮。

**在開 issue 或 PR 之前，請先查閱這份清單。**如果回報的內容只是重提這裡列出的
項目卻沒有新證據——或是提議去做某項已基於明確理由而延後的工作——可能會直接關閉，
並附上指向本頁的連結。如果你有新證據，或是有能影響「重新考慮的時機」條件的變動，
請在回報中明確說明。

狀態說明：**Upstream-blocker**（修正屬於另一個專案）·
**Unsupported**（目前刻意不維護）· **Needs-operator-input**
（卡在只有 maintainer 能提供的擷取資料或決策）。

---

## Upstream / external（不在 agend-terminal 內修正）

### `opencode --continue` 偶爾無法 resume
- **Status：** Upstream bug（已緩解）
- **Why：** OpenCode TUI 在 resume 時可能送出一個佔位用的（「dummy」）session id，
  導致「Unexpected server error」。agend-terminal 透過降級為全新 session 來緩解這個
  問題（#1519）——功能上可運作，但先前的 session 不會被恢復。這是 OpenCode 端的
  bug，不是 agend 的根本修正。
- **Revisit when：** OpenCode 在 upstream 修正 dummy-session id 的問題。
- **Refs：** #1526（agend 緩解：#1519）
- **半死 wedge 變體（2026-07-02，待重現）：** 同一個 dummy-session bug 也可能表現成
  OpenCode 行程「不退出」——TUI 框架持續渲染，未捕捉例外的堆疊卻疊加在畫面上，agend
  現有三層偵測（state-pattern 分類器、respawn-stuck watchdog、backend-exit 偵測）全
  都接不住，因為行程從未真的崩潰，也沒有已知錯誤簽名能匹配這段堆疊。目前已存到一份
  真實 capture（推翻了先前一次 session 認為證據不可回收的判斷）；要再有第二個樣本才
  會補偵測 pattern，避免誤判合法輸出（見 t-20260702144219394508-56872-6）。
  respawn_watchdog 側的結構性加固併入 round-3 #2549 的範圍，不另立單。**若再次遇到：
  務必在 restart 或任何介入之前先存證**——對卡住的 instance 呼叫
  `pane_snapshot(to_file=true)` 會把完整畫面寫進 `$AGEND_HOME/captures/`；一旦
  restart/replace，唯一的證據就沒了。

## Deferred — 等待 operator 擷取資料或決策

### 真實 PTY corpus（5 個 backend × 2 種情境）尚未完成
- **Status：** Needs-operator-input
- **Why：** 穩健的狀態偵測工作需要橫跨各支援 backend 的真實終端擷取資料，作為驗證
  關卡；目前這份 corpus 還不完整。
- **Revisit when：** operator 擷取完剩下的 corpus。
- **Refs：** #1014

### Claude Code「Yes, proceed」modal——預設游標位置尚未驗證
- **Status：** Needs-operator-input
- **Why：** 確認該 modal 的預設游標位置需要一份真實擷取資料。
- **Revisit when：** operator 擷取了該 modal。
- **Refs：** #1054

### Operator Mode（active / away / sleep / dnd + delegation）
- **Status：** Needs-operator-input
- **Why：** 在開始實作之前，需要先凍結 operator 政策並做分階段拆解。
- **Revisit when：** operator 凍結政策且工作完成分階段拆解。
- **Refs：** #1339

<!--
#1521 Schedule fire-strategy — 已出貨（2026-07 自本清單移除）。
`FireStrategy::{Always, UntilSuccess}` 定義於 `src/schedules.rs`，
由 `src/daemon/cron_tick.rs` 執行（linked-task gate + 當日 suppress）。
勿再標為「尚未拍板」。
-->