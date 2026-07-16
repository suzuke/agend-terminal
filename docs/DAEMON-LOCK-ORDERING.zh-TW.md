[English](DAEMON-LOCK-ORDERING.md)

# Daemon 鎖定順序

**Sprint 23 P0 交付項目**（依 dev-reviewer-2 的跨視角要求）——
明確記錄鎖的取得順序，以避免 supervisor tick 與 MCP handler
並行負載下發生死鎖。

**狀態**：ACTIVE——所有 daemon 程式路徑都必須遵守此順序。
**維護方式**：與 `tests/heartbeat_pair_atomicity_audit.rs`
不變量測試一併維護（Sprint 23 P0 防繞過措施）。
**重新驗證**：已對照 `main@1d83b423`（2026-07-16）驗證具名鎖與
writer/reader 錨點。下方概念層級從 Level 0 開始；`sync_audit` 的
runtime tier 編號則從 1 開始。若未先計入這個位移，請勿直接比較數字標籤。

**範圍**（Sprint 23 P0 r2 F4 釐清）：daemon runtime 熱路徑
（supervisor tick + MCP handler dispatch + agent lifecycle）所取得的鎖。
僅啟動時使用的鎖（`identity::LOCK`、`fleet_normalize::WARNED`）、
僅清理時使用的鎖（`worktree_cleanup::ENV_LOCK`），以及測試 fixture 的鎖
不在範圍內——它們不屬於 runtime，因此不會形成本文處理的並行取得類別。

---

## 階層（依此順序取得；反向釋放）

```
Level 0 (root):
  agent_registry            — global HashMap<InstanceId, AgentHandle>
                              (`crate::agent::AgentRegistry`)
  external_registry         — global HashMap<String, ExternalAgentHandle>
                              (`crate::agent::ExternalRegistry`)
                              NOTE (G3 M5): `register_external` acquires
                              external_registry THEN agent_registry (read).
                              Never acquire agent_registry first then
                              external_registry — deadlock risk.
  configs                   — daemon-only HashMap<String, AgentConfig>
                              (`src/daemon/mod.rs::AgentConfig`)

Level 1 (per-agent, accessed via root):
  agent_handle.core         — Mutex<AgentCore>
                              (vterm + state + health + subscribers)
  agent_handle.child        — Mutex<Box<dyn portable_pty::Child>>
  agent_handle.pty_writer   — Mutex<Box<dyn Write>>
  agent_handle.pty_master   — Mutex<Box<dyn MasterPty>>

Level 2 (storage / transactional):
  task_events_jsonl_lock    — file lock around `<home>/task_events.jsonl`
                              (`crate::task_events::append`); anti-bypass
                              invariant `tests/task_events_invariant.rs`
                              enforces single-writer (Sprint 24 P0 PR #236)
  decision_store_lock       — per-decision file lock beside
                              `<home>/decisions/<id>.json`
                              (`crate::decisions::with_decision_lock`)
  inbox_jsonl_lock          — per-agent flock around append/fsync or
                              rewrite of `<home>/inbox/<agent>.jsonl`
                              (`crate::inbox::storage::with_inbox_lock`)

Level 3 (leaf-level):
  heartbeat_pair (per-agent) — `Mutex<HeartbeatPair>`
                              (`crate::daemon::heartbeat_pair::pair_for`)
  heartbeat_pair_registry    — outer `Mutex<HashMap<String, Arc<…>>>`
                              (`crate::daemon::heartbeat_pair::registry`).
                              Brief-acquire-only inside `pair_for()`;
                              never held across pair-lock acquisitions.
  TelegramState              — `Arc<Mutex<TelegramState>>`
                              (`crate::channel::telegram::lock_state`)
  channel sink registry      — `Mutex<Vec<Arc<dyn UxEventSink>>>`
                              (`crate::channel::sink_registry`)
  thread census              — `Mutex<HashMap<&'static str, AtomicU32>>`
                              (`crate::thread_census::census`).
                              Sprint 26 PR-B counter-only registry; brief
                              acquire on register/Drop/snapshot, never held
                              during nested acquisitions.
```

## 階層規則

1. **由上而下取得**：持有 Level N 鎖的 thread 可以取得 Level N+1
   或更高層級的鎖。持有較高層級鎖時，絕不可取得較低層級的鎖。

2. **向上攀升前先釋放**：如果 thread 持有 Level 1 鎖（例如 core）並需要
   Level 0 鎖（例如 agent_registry），就必須先釋放 Level 1 鎖。否則會和
   另一個依由上而下順序取得鎖的 thread 形成死鎖。

3. **取得任何其他鎖時，絕不可持有葉節點鎖（Level 3）**：
   heartbeat_pair / TelegramState / sink_registry 一律最後取得、最先釋放。
   這是最嚴格的規則——即使只是短暫地在持有葉節點鎖時取得另一把鎖，
   也屬禁止行為。

4. **Level 1+ 不可跨 instance 串接鎖**：禁止在持有 agent A 的 `core`
   鎖時取得 agent B 的 `core`。這不是常見情境，但仍完整記錄於此——
   未來的 fleet-broadcast 重構必須遵守。（同一 instance 內競爭同一把
   Level 1 鎖是標準 Mutex 排隊：先取得者先執行，第二個會阻塞；因為沒有
   cycle，所以不存在死鎖風險。）

---

## 這些規則為何能避免死鎖

（Sprint 23 P0 r2 F1——依 dev-reviewer-2 的跨視角要求，提供明確的
死鎖預防證明概要。）

死鎖需要鎖取得圖中存在 cycle（thread A 等待 thread B 持有的 lock X，
而 thread B 又等待 thread A 持有的 lock Y）。規則 1（由上而下）強制
每個 thread 依相同的偏序 Level 0 → Level 1 → Level 2 → Level 3 取得鎖，
排除跨層級的反向 edge。規則 3（持有葉節點時不取得其他鎖）把 Level 3
壓縮為「短暫取得後立即釋放」，從取得 edge 圖中完全移除葉節點鎖。
規則 2（向上攀升前先釋放）可避免需要重新進入 root 鎖的程式路徑意外
違反規則 1。規則 4（Level 1 不跨 instance）則避免兩個 thread 操作不同
agent 時形成同層 cycle。合併來看：每個 thread 的鎖取得軌跡都是跨層級
嚴格遞增的全序，而葉節點鎖只作為立即釋放的 sink → 不可能形成 cycle
→ 不會死鎖。

---

## heartbeat_pair 為何是葉節點層級（Sprint 23 P0 F6）

`heartbeat_pair` 鎖最初只擁有下列三個計時欄位，現在也擁有每個 turn 的
reply routing / settlement 欄位。`snapshot_for` 在單一短暫 guard 下複製
完整的 `HeartbeatPair`，使每個 tick 與 MCP heartbeat/write 競爭視窗中的
檢視保持一致（Sprint 20 audit 所辨識的競爭視窗）。計時欄位如下：

- `heartbeat_at_ms: u64`——上次 MCP tool call 的 timestamp（Sprint 23 P0 PR #235）
- `waiting_on_since_ms: Option<u64>`——目前 `waiting_on` 開始的時間（Sprint 23 P0 PR #235）
- `last_input_at_ms: u64`——上次 daemon→agent input delivery 的 timestamp（Sprint 24 P1 PR #243）

其餘 `reply_to_*`、mirror、pending-turn 與 settled-group 欄位列在
`src/daemon/heartbeat_pair.rs` 的 `HeartbeatPair` 上；它們遵守同一套
葉節點鎖規則，因此本文刻意不重複列出。

依 dev-reviewer-2 的 threat model 綜合結論，pair 外圍加鎖的設計優於
每欄位各用 `AtomicU64`，因為實際 fleet 威脅是 correctness corruption
（prompt injection、capability bypass）；各欄位 atomic 會暴露 pair
不一致的視窗。

位於葉節點層級代表：

- **MCP heartbeat 寫入點**（`src/mcp/handlers/mod.rs`，`handle_tool` 中的
  implicit heartbeat）：取得 pair 鎖、更新欄位、釋放鎖，然後才呼叫
  `save_metadata` 做 crash-recovery persistence。Disk I/O 在鎖外執行。

- **MCP `set_waiting_on` 寫入點**
  （`src/mcp/handlers/instance_metadata.rs::handle_set_waiting_on`）：
  取得 pair 鎖、同時更新兩個欄位、釋放鎖，然後才呼叫
  `save_metadata_batch` 做原子 disk write。這是兩階段流程：先更新
  in-memory pair，再持久化到 disk。Sprint 22 P2a 的
  `save_metadata_batch` helper（PR #233，由本文原作者撰寫）處理 disk
  端的 atomicity；pair 鎖處理記憶體端的 atomicity。

- **Hang detection 讀取點**
  （`src/daemon/per_tick/hang_detection.rs::HangDetectionHandler::run`）：
  取得 pair 鎖以建立 pair snapshot，立即釋放鎖，接著用複製出的 snapshot
  執行 `check_hang`。這次取得發生於 per-agent core 鎖已持有時，符合允許的
  Level 1 → Level 3 由上而下順序。Pair guard 在 `snapshot_for` 內、
  `check_hang` 繼續前就已釋放，因此葉節點鎖不會逸出呼叫範圍。

若未來 contributor 必須在持有 pair 鎖時取得另一把鎖，就必須先重構為
先取得另一把鎖（依階層向下），或調整結構，等其他所有鎖都釋放後才取得
pair 鎖。

---

## 防繞過不變量測試

`tests/heartbeat_pair_atomicity_audit.rs`（Sprint 23 P0 交付項目）強制：

1. **Source-grep guard**：每個寫入 `last_heartbeat` 或
   `waiting_on_since` 的 `save_metadata` / `save_metadata_batch` call site，
   在前面數行內都必須伴隨 `heartbeat_pair::update_with` 或
   `heartbeat_pair::pair_for(...).lock()` 呼叫。凡跳過 in-memory 更新的
   pre-pair write 都會被標記。

2. **EXEMPTED_LEGACY_FILES 防增長合約**：刻意保持空白。除非 dispatch
   scope 明確授權，否則禁止新增項目。此模式源自已退役的 Sprint 22
   outbound-path audit；現行 enforcement 位於
   `tests/heartbeat_pair_atomicity_audit.rs`。

---

## 操作說明

- **Crash recovery**：heartbeat_pair 是 in-memory cache；daemon 重新啟動時，
  pair 會從空值開始（`heartbeat_at_ms == 0`）。在下一次 MCP heartbeat
  填入 pair 前，supervisor 會退回使用 `read_heartbeat_age`（disk read）。
  這是 graceful degradation——競爭視窗只存在於 daemon 重啟後第一個 tick
  內 supervisor read 與 MCP write 同時發生的情況，視窗極小。

- **Per-instance key**：pair 鎖以 agent 名稱為 key。兩個 agent 不會在彼此的
  pair 鎖上死鎖（不同 key、不同 Mutex）。Sprint 24 P0 task sweep daemon
  對 `task_events.jsonl` 使用相同的 per-key 模式，因此葉節點規則可自然延伸。

- **與 graceful-join 的向前相容性（Sprint 25+）**：
  `daemon::ticker::DaemonTicker` primitive（同為 Sprint 23 P0）已保存
  JoinHandle，供未來 graceful-join consumer 使用。Join 重構不影響 pair 鎖——
  `Mutex<HeartbeatPair>` 不受 thread affinity 限制。

- **F1 heartbeat-spam 交叉檢查（Sprint 24 P2 PR #249）**：
  `check_hang` classifier 交叉比對 `heartbeat_at_ms` freshness 與 PTY silence。
  「Heartbeat fresh」表示 agent 最近呼叫過 MCP tools（由 MCP dispatch
  chokepoint 的 implicit heartbeat 更新）。「PTY silent」表示沒有產生 operator
  可見輸出。若 heartbeat fresh，但 PTY silence 已超過 hang threshold，
  classifier 會把 `IdleLong` 覆寫為 `Hung`——藉此捕捉遭 prompt injection、
  只靠大量 MCP tool call 卻不產生實際輸出來壓制 escalation 的 agent。
  此交叉檢查從 core 鎖（Level 1）之下取得的 pair snapshot（Level 3 leaf）
  讀取 `heartbeat_at_ms`，符合規則 1 的 Level 1 → Level 3 由上而下順序；
  `snapshot_for` 同步取得並釋放 pair 鎖，因此後續執行 `check_hang` 時不再
  持有它（規則 3）。

---

## 相關資料

- Sprint 20 Track B daemon audit（歷史資料；請從 repository history 取回）：
  §1 F6（此競爭視窗）+ F7（disk 端的配套問題，於 Sprint 22 P2a PR #233
  透過 `save_metadata_batch` 修正）。
- Sprint 22 P2a：目前已退役的 outbound-path audit 建立了
  EXEMPTED_LEGACY_FILES 防增長範本，現行 heartbeat atomicity audit
  保留並沿用此範本。
- Sprint 21 PR #226：protocol §12.5 spawn site rationale（平行的 doc-doc
  慣例；這份 lock-ordering 文件是 Sprint 23 對 shared-state primitive
  的對應物）。
- Sprint 24 P0：PR #236（`task_events.jsonl` storage substrate）、PR #239
  （`tasks.json` 退役至 `.legacy_pre_v2` archive——已移除 Level 2 條目）。
- Sprint 24 P1：PR #243（daemon health classifier——`HeartbeatPair`
  新增 `last_input_at_ms`，用來區分 `IdleLong` 與 `Hung`）。
- Sprint 24 P2：PR #249（F1 heartbeat-spam 交叉檢查——比較
  `heartbeat_at_ms` freshness 與 PTY silence 並覆寫分類）。
