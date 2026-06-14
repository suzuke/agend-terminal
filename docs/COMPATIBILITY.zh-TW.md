[English](COMPATIBILITY.md)

# On-Disk Format Compatibility Policy — 磁碟格式相容性政策

agend-terminal 會在 `$AGEND_HOME` 底下（以及少數幾個 agent 工作目錄內）讀寫
許多檔案。既然已經有外部使用者，這些檔案就屬於產品介面，而非實作細節。本文件
依各層級宣告你可以仰賴哪些保證，依據是 2026-06-11 的格式盤點（#1989）。

簡單來說：**tier (a) 和 (b) 的變更只允許新增（additive-only）**，直到出現
真正的 migration 框架為止。「只允許新增」的意思是：

- 新欄位一律是**選填，並帶有 serde default**——由舊版本寫出的檔案能繼續以
  完全相同的方式反序列化。
- 既有欄位永遠不會被改名、改型別、改用途或移除。
- 無法以新增方式表達的變更就是**破壞性變更（breaking change）**：它會把對應的
  schema 版本往上推、附帶一份 migration（或一個明確的 refuse-with-instructions），
  並在 CHANGELOG 的 migration notes 中特別點出。

## Tier (a) — stable public interfaces (hand-edited or user-visible)

這些檔案你可以手動編輯；升級時絕不能悄悄改變它們的語意。

| Surface | Notes |
|---|---|
| `fleet.yaml` | 最主要的手動編輯介面。帶有一個選填的 `schema_version:`（省略 = `1`，也就是每個 #1989 之前檔案的版本）。當某個檔案宣告的版本比 daemon 支援的還新時，daemon 會發出**警告**（未知欄位會被 serde 靜默忽略——警告就是提醒你 daemon 太舊的訊號），而且絕不會因此拒絕啟動。daemon 絕不會把 `schema_version:` 注入到原本沒有它的檔案裡。 |
| Service templates | 由 `agend-terminal service install` 寫出的 launchd plist / systemd unit / Task Scheduler XML。升級後請用 `service install` 重新產生，而不是手動移植；手動修改只會存活到下一次 `service install` 為止。 |
| Instruction blocks | 注入到 agent 指令檔（例如 `CLAUDE.md`）中、以標記界定的區塊。標記本身就是介面：標記之間的內容由 daemon 擁有並會被覆寫；標記之外的一切都由使用者擁有，永不會被動到。 |
| MCP config | 寫入 agent 工作目錄的 backend MCP 接線設定（例如 `.mcp-config.json`、`.claude/settings.local.json` 內的項目）。daemon 擁有的 key 會原地 upsert；同一檔案中使用者自行加入的 key 會被保留。 |

## Tier (b) — internal persisted state (versioned)

由 daemon 擁有、必須在重啟與升級後存活的狀態：inbox 訊息、task-board 項目與
事件、decision log，以及各 sidecar store（escalation persist、ci-handoff
tracks、pending dispatches……）。這些 schema 要嘛帶有明確的 `schema_version`
欄位，要嘛在與 tier (a) 相同的規則下只允許新增地演進。手動編輯這些檔案不在
支援範圍內；升級後的 daemon 必須能讀取同一 major 版本中任何先前發行版所寫出的
狀態。某個 store 如何看待**比它支援的版本還新**的紀錄則是各 store 自行決定
（#1992）：inbox 會跳過那筆未知紀錄並發出警告、繼續供應其餘的（降級）；
task-events store 則會對整個檔案 fail-close（這是刻意的——board 的完整性優先於
可用性，比層級底線更嚴格）。無論哪一種：都不會 crash，也不會悄悄丟失資料。

`runtime-config.json`、decision log（`decisions/*.json`）和 `binding.json`
都帶有明確的 `schema_version`（#1990）：沒有它的舊檔案會正常讀取；比支援版本還新的
檔案則會 fail-close（runtime-config 依 #1576 保留 last-known-good，並拒絕一次
會覆寫它的寫入；decision 在讀取時會被跳過、在更新時會被拒絕；binding 在
**daemon 端**讀取者眼中會被視為不存在——git shim 有自己的讀取器，它會做 HMAC
驗證，並把一個可解析的未來 binding 視為已綁定，因此 agent 仍被限制在自己的
worktree 內）。

**有兩個 tier (b) store 是未版本化的自由格式 key-value 袋（bag）**，無法在不
破壞形狀的前提下帶上 `schema_version`：`topics.json`（telegram topic 註冊表
——一個裸的 `topic_id → instance` map）和 `metadata/*.json`（per-instance 的
operator metadata——一個開放的 KV bag）。它們的相容性規則更窄也更明確：
**只能新增 key；既有 key 永不改名、改型別或改用途。** 為它們加上版本化被延後
（#1990），直到真的出現非新增式的變更需求為止——把一個 bag 包進版本化的封套裡
本身就是破壞性變更，而且兩者都是低風險（topics 會透過開機時的 orphan-sweep
自我修復；metadata 屬於 operator 的外觀層面）。

## Tier (c) — regenerable / ephemeral (no commitment)

快取、lock 檔、PTY transcript、log、執行期 socket / PID 檔，以及任何 daemon
能從頭重建的東西。沒有格式保證；任何發行版都可能改動或刪除這些檔案。如果刪掉
某個 tier (c) 檔案後，行為的改變超過一次性的重建成本，那就是 bug——請回報。

## Versioning mechanics (tier (a) fleet.yaml)

- `FLEET_SCHEMA_VERSION`（`src/fleet/mod.rs`）是 daemon 讀寫所用的版本；
  `FleetConfig::effective_schema_version()` 會把省略的欄位解析為 `1`。
- 新增的選填欄位**不會**推升版本。
- 未來的破壞性變更會把這個常數往上推，而當時的 daemon 必須對較舊的檔案附帶
  明確的處理（migration 或有文件記載的拒絕）。在這樣的框架出現之前，對
  fleet.yaml 的破壞性變更一律不允許。