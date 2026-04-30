# 測試審計報告

**日期**: 2026-04-29
**基準**: main branch HEAD (102e6ad)
**結果**: 1302 tests, 0 failures, 8 ignored

---

## 總覽

| 類別 | 數量 |
|------|------|
| Inline tests (src/) | 1194 |
| Integration tests (tests/) | 108 |
| **Total** | **1302** |
| Ignored | 8 |
| Failures | 0 |
| Integration test files | 18 |
| Inline test modules | ~70 |

---

## Dead Tests — 已移除功能的殘留

**結論：HEAD 上沒有 dead tests。** Sprint 29 的清理已經同步移除了對應的測試：

| 移除的功能 | 測試狀態 |
|-----------|---------|
| RBAC outbound capability (#285) | ✅ 測試已移除 |
| Slow-loris timeout (#282) | ✅ `slow_loris_timeout.rs` 已移除 |
| Self-healing supervisor (#287) | ✅ `self_healing_supervisor.rs` 已移除 |
| Symlink escape validation (#286) | ✅ 相關測試已簡化（只保留 `..` 拒絕） |
| Const-time cookie (#283) | ✅ 測試已移除 |
| Heartbeat-spam (#283) | ✅ 測試已 reframe |
| Frame env override (#283) | ✅ 測試已移除 |

---

## 8 個 Ignored Tests

| Test | 原因 | 建議 |
|------|------|------|
| `backend_harness::test_backend_semantics_claude` | 需要真實 CLI binary | 保留 — CI 無法跑，本地手動驗證用 |
| `backend_harness::test_backend_semantics_codex` | 同上 | 保留 |
| `backend_harness::test_backend_semantics_gemini` | 同上 | 保留 |
| `backend_harness::test_backend_semantics_kiro` | 同上 | 保留 |
| `tasks::test_claimed_task_not_touched_by_dep_eval` | PR3 cutover 後場景不可達 | **可刪除** — 註解自己說 "legacy bypass scenario unreachable" |
| `tasks::test_list_default_hides_done_older_than_14d` | PR3 cutover 後需要 backdated writes | **可刪除** — 邏輯已被其他 Done tests 間接覆蓋 |
| `signals::install_term_only_catches_sigterm` | 修改 process-global SIGTERM | 保留 — 需要隔離執行 |
| `state::replay_session` | 未標明原因 | **檢查** — 確認是否仍有價值 |

**建議刪除 2 個**（tasks 的 2 個 legacy ignored tests），它們的註解明確說場景已不可達。

---

## 覆蓋缺口 — 無測試的大型模組

| 模組 | LOC | 測試 | 風險 | 建議 |
|------|-----|------|------|------|
| `mcp/handlers/instance.rs` | 644 | 0 | **高** — instance create/delete/replace 的核心邏輯 | 需要測試 |
| `verify.rs` | 616 | 0 | **高** — 驗證邏輯 | 需要測試 |
| `cli.rs` | 521 | 0 | 中 — CLI arg parsing，但有 clap 保護 | 低優先 |
| `app/session.rs` | 395 | 0 | 中 — TUI session 管理 | 低優先（UI 層） |
| `agend-mcp-bridge.rs` | 350 | 0 | **高** — bridge binary，有 integration tests 間接覆蓋但無 unit tests | 需要測試（尤其 retry 邏輯） |
| `app/dispatch.rs` | 330 | 0 | 中 — TUI dispatch | 低優先（UI 層） |
| `app/commands.rs` | 325 | 0 | 中 — TUI commands | 低優先（UI 層） |
| `tray/mod.rs` | 297 | 0 | 低 — system tray，平台相關 | 低優先 |
| `api/handlers/team.rs` | 228 | 0 | 中 — team API handlers | 有 mcp_roundtrip 間接覆蓋 |
| `connect.rs` | 189 | 0 | 中 — daemon 連線邏輯 | 需要測試 |

**最需要補測試的 3 個模組**：
1. `mcp/handlers/instance.rs` — 644 LOC 的 instance lifecycle 邏輯完全沒有 unit test
2. `verify.rs` — 616 LOC 的驗證邏輯沒有測試
3. `agend-mcp-bridge.rs` — retry/reconnect 邏輯只有 integration test，沒有 unit test

---

## 測試品質觀察

### 強項
- **State machine 測試**（state.rs, 88 tests）：覆蓋所有 5 個 backend 的 pipeline 狀態轉換，非常完整
- **MCP roundtrip**（33 tests）：端到端覆蓋 tool call → handler → inbox 的完整路徑
- **Invariant tests**：`spawn_rationale_audit`、`no_dual_track_drift`、`task_events_invariant`、`file_size_invariant` 用 source grep 確保架構約束
- **Telegram**（73 tests）：覆蓋了大量 edge case

### 可改進
- **Integration tests 太慢**：`integration.rs`（9 tests, 14s）佔了總測試時間的一半，因為 spawn 真實 process
- **Backend harness 4 個 ignored**：考慮用 mock backend 替代，讓 CI 也能跑
- **mcp_bridge_idle_reconnect**（2 tests）：spawn 真實 bridge binary + mock daemon，容易 flaky

---

## 總結

| 指標 | 狀態 |
|------|------|
| Dead tests | ✅ 無（Sprint 29 已清理） |
| Ignored tests 合理性 | ⚠️ 2 個可刪除 |
| 覆蓋缺口 | ⚠️ 3 個高風險模組無測試 |
| 整體健康度 | 良好 — 1302 tests, 0 failures |
