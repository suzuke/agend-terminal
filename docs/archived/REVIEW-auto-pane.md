# Code Review: Auto Tab/Pane for MCP-Spawned Instances

> **Status: SHIPPED** — review cycle closed, feature live on `main` (commit `36dc537`). Doc retained for historical/provenance.

**Commit:** `36dc537` feat: auto tab/pane for MCP-spawned instances and teams
**Date:** 2026-04-17
**Scope:** 18 files, +864/-307

## 概述

當 agent 透過 MCP 工具產生時，TUI 自動建立對應的 tab/pane。同時把 `create_team` 合併進 `create_instance`（用 `team` + `count` 參數），並將所有 API method 字串統一成常數。所有測試通過，clippy 零警告。

## 上次 Review 問題追蹤

| 問題 | 狀態 |
|------|------|
| 🔴 TUI 事件後沒設 `needs_resize` | ✅ 已修復（app.rs:779） |
| 🟡 P1: Team 路徑 instructions 競態 | ✅ 已修復（0f33156，pre-generate 在 API spawn 前） |
| 🟡 P2: `create_instance` team 模式下 `name` required | ✅ 已修復（description 已說明 team 模式下 name 被當作 base name，忽略） |
| 🟡 P3: split fallback 重複訂閱 | ✅ 已修復（25337ee，placement 在 attach_pane 前解析） |
| 🟡 P4: 註解殘留 `workspaces` | ✅ 已修復（0c563c5） |
| 🟢 P5: `LayoutHint::from_str` 遮蔽標準 trait | ✅ 已修復（重新命名為 `parse_hint`） |
| 🟢 Team handler 沒有對已存在的 pane 去重 | 未改動（可接受） |

## 好的部分

### `api::method` 常數模組
把散落在 30 多處的魔術字串（`"list"`、`"spawn"`、`"kill"` 等）統一成 `api::method::LIST`、`api::method::SPAWN` 等常數。消除拼字錯誤的可能，協議變更時可一次 grep 到所有呼叫點。

### `spawn_one` 抽取
spawn 邏輯（建目錄 → 產生 agent → 啟動 TUI socket 執行緒）原本在 `SPAWN` 和 `CREATE_TEAM` handler 裡各寫一次，現在統一成 `spawn_one()` 函式。

### `remove_agent_pane` 共用
`:kill` 指令原本有 20 行 inline 的 pane 移除邏輯，現在 `:kill` 和 `InstanceDeleted` 事件都用同一個 `remove_agent_pane()`，會 loop 移除所有包含該 agent 的 pane（處理同一 agent 同時出現在 team tab 和個別 tab 的情況）。

### 非阻塞的 task 注入
Task 注入原本在 MCP handler 執行緒上 `sleep(3s)` 阻塞回應，現在改成 spawn 背景執行緒。

### `add_instances_to_yaml` 批次寫入
Team 建立時，所有 instance 在一次 lock + write 週期內寫入 fleet.yaml，而不是 N 次獨立的檔案操作。

## 需要修復的問題

### 🟡 P1: Team 路徑的 instructions/mcp_config 在 spawn 後才寫入

單一 instance 路徑（`spawn_single_instance`）是先呼叫 `instructions::generate()` 和 `mcp_config::configure()`，再呼叫 API spawn。但 team 路徑是反過來的：

```rust
// mcp/handlers.rs — team 路徑
Ok(resp) if resp["ok"].as_bool() == Some(true) => {
    for inst_name in &spawned {
        let wd = home.join("workspace").join(inst_name);
        crate::instructions::generate(&wd, backend);  // ← spawn 之後才寫
        crate::mcp_config::configure(&wd, backend);
    }
```

Agent 開始執行時 instructions 和 MCP config 檔案可能還不存在。這是一個競態條件。

**建議：** 移到 `spawn_one` 裡，或在 API 呼叫前執行。

### 🟡 P2: `create_instance` 在 team 模式下仍要求 `name`

`create_team` MCP 工具被完全移除（工具數 35 → 34）。功能改由 `create_instance` 的 `team` + `count` 參數存取。但 schema 裡 `name` 仍是 `required: ["name"]`，team 模式下卻會被忽略。

**建議：** 把 `name` 改成 optional，或在 description 中明確說明 team 模式下的行為。

### 🟡 P3: Split fallback 重複訂閱

`handle_instance_created` 裡，第一個 `attach_pane` 建立 subscriber + forwarder 執行緒。如果 `split_focused` 消耗了 pane 但失敗（回傳 `false`），forwarder 執行緒變成孤兒，第二個 `attach_pane` 又建立一組重複的。

```rust
let pane = attach_pane(name, ...)?;  // ← 第一次訂閱

let split_done = spawner
    .and_then(|spawner_name| { ... })
    .map(|tab| tab.split_focused(dir, pane))  // ← 消耗 pane
    .unwrap_or(false);

if !split_done {
    let pane = attach_pane(name, ...)?;  // ← 第二次訂閱，第一個 forwarder 變孤兒
    layout.add_tab(Tab::new(name.to_string(), pane));
}
```

**建議：** 先檢查目標 tab 是否存在，再建立 pane。

### 🟡 P4: 註解中殘留 "workspaces"（複數）

程式碼路徑已改用 `home.join("workspace")`（單數），但多處註解仍寫 "workspaces"：

- `src/mcp/handlers.rs:816` — `$AGEND_HOME/workspaces/`
- `src/mcp/handlers.rs:821` — `$AGEND_HOME/workspaces/`
- `src/cli.rs:24` — `$AGEND_HOME/workspaces/general`

**建議：** 全域替換成 "workspace"。

### 🟢 P5: `LayoutHint::from_str` 遮蔽標準 trait

```rust
impl LayoutHint {
    pub(crate) fn from_str(s: &str) -> Self { ... }
}
```

遮蔽 `std::str::FromStr::from_str`。目前因為是 `pub(crate)` 且未多態使用所以沒問題，但會讓人困惑。

**建議：** 低優先，方便時改名為 `parse_hint` 或實作 `FromStr`。

## 總結

| 優先級 | 問題 | 檔案 |
|--------|------|------|
| 🟡 P1 | instructions 競態條件 | `mcp/handlers.rs` |
| 🟡 P2 | team 模式 `name` 仍 required | `mcp/tools.rs` |
| 🟡 P3 | split fallback 重複訂閱 | `app.rs` |
| 🟡 P4 | 註解 workspaces → workspace | `mcp/handlers.rs`, `cli.rs` |
| 🟢 P5 | `from_str` 命名 | `app.rs` |

整體是紮實的工作。API method 常數化和 `spawn_one` 抽取都是好的重構。主要顧慮是 team 模式下 instruction 生成的競態條件。
