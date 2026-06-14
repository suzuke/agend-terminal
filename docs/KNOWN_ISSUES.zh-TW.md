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
（卡在只有 maintainer 能提供的擷取資料或決策）·
**Stale**（目前無人負責）。

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

## Stale — 目前無人負責

### Schedule fire-strategy
- **Status：** Stale
- **Why：** 目前無人負責；策略尚未拍板。
- **Refs：** #1521