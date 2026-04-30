# 過度工程審計報告

**日期**: 2026-04-28
**範圍**: agend-terminal codebase
**審計角度**: 以實際威脅模型檢視安全/防禦措施的必要性

---

## 威脅模型前提

agend-terminal 是一個 **localhost-only、單用戶 daemon**：

- 所有 socket bind `127.0.0.1`，外部網路不可達
- Cookie 檔案 `~/.agend-terminal/run/<pid>/api.cookie` 權限 0600
- 能讀到 cookie 的人 = 已有本機 shell access
- 唯一的 client 是用戶自己的 CLI 工具和 MCP bridge

**核心推論**：能通過 cookie auth 的 caller 已經擁有完整的本機權限。針對「已認證但惡意的遠端攻擊者」設計的防禦在此模型下沒有實際價值。

---

## 發現

### 高影響

#### 1. Outbound Capability RBAC 系統

**位置**: `src/channel/auth.rs`

整套 `ChannelOpKind` / `OutboundCapabilityDecision` / `evaluate_outbound_capability` / `gate_outbound_for_agent` 實作了 per-instance、per-operation-type 的授權系統：

- 4-variant `ChannelOpKind` enum（Reply / React / Edit / InjectProvenance）
- 3-variant decision enum（Allowed / Rejected / OpenDefault）
- 每次 outbound 訊息都讀 fleet.yaml（磁碟 I/O per call）
- 全域 `Mutex<HashSet<String>>` 做 warn-once dedup
- 橫跨 4 個 sprint 的 policy 翻轉（Sprint 21 → 22 → 23 → 24，default-open → fail-closed → default-open）

**設計假設**: 被 prompt injection 的 agent 可能透過 MCP spam 壓制告警升級。

**為何過度**: Agent 是用戶自己啟動的 process，用戶在 TUI 裡看得到所有輸出。4 個 sprint 的 policy churn 本身就說明團隊無法確定這在防禦什麼威脅。

**建議**: 移除整套系統，或簡化為單一 boolean flag。

---

### 中影響

#### 2. Slow-Loris 5s Read Timeout

**位置**: `src/api/mod.rs:237-248`

```rust
let read_timeout_secs: u64 = std::env::var("AGEND_API_READ_TIMEOUT_SECS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(5);
```

Sprint 25 P3 從 30s 收緊到 5s，防禦 drip-feed / slow-loris 攻擊。

**為何過度**: Slow-loris 是遠端 DoS 攻擊手法。此 daemon 沒有不受信任的遠端 client。5s 太緊直接導致 idle MCP bridge 斷線（PR #276 的根因），還需要 `AGEND_API_READ_TIMEOUT_SECS` env var 作為 workaround，並有專屬測試 `real_serve_env_override_extends_timeout` 測試這個 workaround。

**實際代價**: 為不存在的攻擊者犧牲了可靠性，產生了 ~30% 的 MCP tool call 失敗率。

**建議**: 放寬到 60s 或移除，依賴 TCP EOF 偵測。

#### 3. Peer PID Watcher Thread

**位置**: `src/api/mod.rs:310-340`

每個 API 連線 spawn 一個 OS thread，每 2 秒 `kill(pid, 0)` 偵測 dead peer。

**為何過度**: Localhost 上 client process 死亡時，OS 關閉 socket → `read_line` 立刻回 EOF。加上 #2 的 5s timeout，PID watcher 只為省 ~3 秒偵測時間而增加 thread-per-connection 開銷。兩套機制（timeout + PID watcher）偵測同一件事。

**建議**: 移除。合理的 read timeout + TCP EOF 已足夠。

#### 4. Working Directory Symlink Escape Prevention

**位置**: `src/api/mod.rs:119-155`

`validate_working_directory` 實作：
- 拒絕 `..` 路徑元件
- 向上遍歷找最深存在的 ancestor
- Canonicalize 解析 symlink
- 檢查 allowed roots
- 6 個專屬測試（含 symlink escape regression test）

**為何過度**: Daemon 以用戶身份執行，本來就有完整 filesystem access。能呼叫此 API 的 caller（用戶自己或 agent）已經有 PTY shell access，可以 `cd` 到任何地方。

**建議**: 移除 canonicalization 和 ancestor walk。如需保留，簡單拒絕 `..` 即可。

#### 5. MCP Bridge 重複實作

**位置**: `src/bin/agend-mcp-bridge.rs`

Bridge 為了「zero crate dependencies」自行重寫：
- Cookie 讀取邏輯
- Port 檔案讀取邏輯
- Home 目錄解析
- Auth handshake

**為何過度**: Bridge 是同一個 crate build 出來的 binary，可以直接 `use agend_terminal::auth_cookie` 等 library functions。兩份程式碼意味著協議變更需要同步修改兩處，存在 protocol drift 風險。

**建議**: 使用 crate library functions，移除重複實作。

---

### 低影響

#### 6. Constant-Time Cookie 比較

**位置**: `src/auth_cookie.rs:56-63`

```rust
pub fn verify(expected: &Cookie, actual: &[u8]) -> bool {
    let mut diff: u8 = 0;
    for i in 0..COOKIE_LEN {
        diff |= expected[i] ^ actual[i];
    }
    diff == 0
}
```

手寫 XOR loop 防禦 timing side-channel attack。

**為何過度**: Timing attack 需要遠端攻擊者精確測量回應延遲。Localhost loopback 的 jitter 遠大於 32-byte 比較的時間差異。能連上 daemon 的人要嘛已有 cookie（直接用），要嘛沒有 cookie（pre-auth timeout 擋掉）。

**建議**: 替換為 `==`，加註解說明 timing attack 在此模型下不適用。

#### 7. Heartbeat-Spam Attack 分類

**位置**: `src/health.rs:218-235`

Health classifier 有專門分支處理「agent 用 MCP spam 保持 heartbeat fresh 來逃避 hang detection」。

**為何過度**: 假設 adversarial agent，但 agent 是用戶自己啟動的 process。合法場景（agent 在 tight MCP loop）和「攻擊」場景共用同一 code path，adversarial 假設只增加了分類複雜度。

**建議**: 簡化 classifier，移除 adversarial-agent 假設。

#### 8. Cross-Platform PID Alive Check

**位置**: `src/api/mod.rs:295-308`

Windows 路徑每 2 秒建立 `sysinfo::System` 物件、掃描全部 process 只為查一個 PID。引入 `sysinfo` crate 依賴。

**建議**: 隨 #3 一起移除。

#### 9. Frame Size Limit Env Override

**位置**: `src/framing.rs:11-17`

1MB frame limit 本身合理，但 `AGEND_FRAME_LIMIT` env var override 增加了不必要的配置面。正常操作下不應觸及此限制。

**建議**: 移除 env var override，保留 hardcoded limit。

---

## 未列入的項目（localhost 下仍合理）

| 項目 | 理由 |
|------|------|
| Cookie auth 本身 | 防止共用機器上的誤存取 |
| Cookie 檔案 0600 權限 | 標準 Unix 實踐 |
| Bind 127.0.0.1 | 防止意外暴露 |
| NDJSON 協議設計 | 乾淨、可除錯 |
| Health tracking / crash respawn / backoff | 運維必要 |
| Frame size limit（不含 env override） | 合理的防禦性程式設計 |

---

## 總結

Codebase 將 localhost TCP loopback 當作 internet-facing service 來防禦。最大的問題是 outbound capability RBAC（#1，高複雜度、4 sprint churn、不明確的威脅模型）和 slow-loris 5s timeout（#2，直接導致可靠性問題）。建議以實際威脅模型為基準，移除或大幅簡化上述防禦措施。
