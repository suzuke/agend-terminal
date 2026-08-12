[English](REMOTE-NOTIFICATION-POLICY.md)

# 遠端通知政策

Telegram 與 Discord 是遠端操作者介面，只應顯示回覆操作者要求，或需要操作者
判斷的訊息。詳細診斷仍保留在 TUI、inbox、事件紀錄與應用程式 log。

## 傳送來源盤點

| 來源 | 觸發條件 | 遠端行為 | 理由 |
|---|---|---|---|
| Agent 回覆（`reply`、進度 mirror） | 存在來自 channel 的操作者對話 | 保留原始回覆、編輯與 reaction 行為 | 這是操作者要求的對話，不是系統通知 |
| Inbound receipt／task-created acknowledgement | 操作者從 channel 傳送訊息或建立 task | Reaction／edit 或一則短 acknowledgement | 確認操作已送達 |
| Fleet activity mirror | Delegate、report、decision、broadcast 或未送出的 pane input 事件 | 僅 Telegram，且只有設定 `fleet_binding` 時；限制為單行摘要 | 明確 opt-in 的稽核串流；Discord 尚未實作此 sink |
| 互動／權限停滯 | 確認 active agent 卡在 prompt | 每個 blocked episode 一次；只保留最新操作介面，限制 10 行／820 字元 | 舊版 40 行 pane transcript 會淹沒遠端聊天，且可能含完整 shell command |
| Prompt 恢復 | 已通知的 prompt 維持 blocked 足夠久後恢復 | 一則 silent 短訊息 | 避免操作者處理已失效的警示；快速自行恢復的 prompt 只記錄在 log |
| Agent lifecycle P0 | Crash、terminal respawn failure、backend exit、auth expiry、確認的 orchestrator hang | 每個設定 channel 各一則去重 Error 警示 | 可能需要操作者立即處理 |
| Infrastructure P0 | Tick stall、canonical repo 遺失、CI handoff 過期、offline unread obligation | 一則 latched／deduplicated Error 警示 | 否則工作可能遺失或永久停滯 |
| Recovery exhaustion | Rate-limit retry 用盡、inject 連續失敗、reclaim 達上限 | 一則 terminal 警示 | 自動恢復已停止 |
| Context handoff | Context 過高，且 nudge 後仍沒有 durable handoff | 一則 warning | 可能需要手動 handoff／restart |
| CI provider warning | CI provider polling／auth／rate-limit 失敗 | 依 provider backoff 發送一則 warning | 表示無法觀測 CI，不是一般 CI 進度 |
| PR compliance | PR 首次違反必要 compliance check | 每個 PR 一則 warning | 合併前需要處理 |
| Reply discharge | Agent 明確關閉 channel reply 而未回答 | 一則可稽核通知 | 操作者有權知道回覆被刻意省略 |

## 不傳到遠端 channel

下列訊號保留在 TUI／inbox，不會由通知層升級到 Telegram 或 Discord：

- 一般 `[AGEND-MSG-PENDING]` pointer 與 poll reminder；
- 一般 agent state transition 與 heartbeat 診斷；
- 成功的背景 cleanup、retry 與 scheduler tick；
- 完整 pane snapshot、shell command body、stack trace 與 test log；
- 一般 CI／PR workflow handoff；它們送到負責 agent 的 inbox。只有 CI provider
  本身失敗才走遠端 warning。

## 路由規則

- `gated_notify` 是系統通知共用的 authorization 與 operator-mode gate。
  Channel allowlist 缺漏或為空時會 fail closed。它會先遮蔽常見的 `token`、
  API key、password、authorization 與 credential assignment，再套用最後一道 12 行／
  1,200 字元保護：若 emitter 誤傳 transcript 大小的內容，會保留事件開頭與
  最新操作介面。
- `Sleep` 只接收 Error；`Away` 接收 Warn 與 Error；`Active` 接收所有嚴重度。
- Error 等級 P0 會送到所有已設定 channel。一般 Info 不會只因為有多個
  channel 就 fan out。
- 明確的 agent 回覆不會被通知政策縮短。
- 必要的系統通知應先說明事件與受影響 agent，再指出操作者是否需要處理。
  原始 TUI、command、stack 與 log 只能是輔助資訊，不能成為通知主體。
- Prompt 的完整 pane 文字只留在本機。遠端 prompt preview 採 tail-biased
  截斷，確保 warning、choice 與 cancel hint 能保留下來。
