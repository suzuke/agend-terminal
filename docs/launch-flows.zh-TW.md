[English](launch-flows.md)

# Launch Flows — 啟動流程與 daemon 生命週期

本文件整理啟動 `agend-terminal` 的每一種方式、各種路徑所隱含的 daemon
生命週期，以及冷啟動（沒有 daemon 在運行）與熱啟動（已有 daemon 在運行）
之間的差異。

內容對應 `fe528c1`（#879v3 revert 之後的狀態）。

## TL;DR 對照表

| 命令 | 冷啟動（無 daemon） | 熱啟動（daemon 運行中） | Shell 行為 |
|---|---|---|---|
| `agend-terminal start` | 將 daemon 以 detached 方式 spawn 到背景；parent 結束 | 中止：`another agend-terminal daemon is already running (lock held)` | 非阻塞 |
| `agend-terminal start --foreground` | 在前景運行 daemon；阻塞 shell | 中止（同上） | 阻塞；Ctrl+C 會殺掉 daemon |
| `agend-terminal start --agents NAME:CMD ...` | 隱含 `--foreground`；略過 `fleet.yaml`；只 spawn 列出的 agent | 中止 | 阻塞 |
| `agend-terminal app` | **Owned 模式**：TUI 拉起一個 in-process daemon | **Attached 模式**：TUI 以現有 daemon 的 client 身分運行 | 阻塞；Ctrl+B d 行為不同（見下文） |
| `agend-terminal tray` | menu bar app 閒置；「Start daemon」選單項目 shell out 執行 `agend-terminal start --foreground` | 選單項目反映 daemon 狀態；不 spawn | 常駐；不會佔用 shell |

輔助子命令（不是完整啟動，但與生命週期相關）：

- `attach <name>` — 在目前的 shell 中連接到既有 agent 的 PTY。Ctrl+B d
  會 detach 回到 shell。
- `connect <name> --backend X` — 向運行中的 daemon 註冊一個新 agent。
- `stop` — 乾淨地關閉 daemon。
- `kill <name>` — 停止單一 agent。
- `list`（別名 `status`、`ls`）— 列出運行中的 agent。

## Daemon discovery — Owned vs Attached

`start` 和 `app` 都會走 `src/bootstrap/mod.rs` 中同一個 `bootstrap::prepare()`
接縫。它會回傳以下其中之一：

- `BootstrapOutcome::Attached(_)` — 已有一個活著的 daemon 擁有 run dir；
  呼叫端以 client 身分接上。
- `BootstrapOutcome::Owned(_)` — 沒有活著的 daemon；呼叫端在自己 process
  的生命週期內就「是」那個 daemon。

決策分成 4 個步驟：

1. `try_attach()` — 掃描 `~/.agend-terminal/run/*`，探測 `api.port`（TCP
   connect，200ms timeout）。如果探測成功，回傳 Attached。
2. 取得 daemon 的獨佔鎖（`acquire_daemon_lock`）。這會擋掉其他 starter
   彼此競爭。
3. 再跑一次 `try_attach()`（TOCTOU 防護——在步驟 1 和步驟 2 之間可能又有
   另一個 daemon 起來了）。
4. 仍然沒有活著的 daemon → 建立 run dir、寫入 `.daemon` identity、
   發出 `api.cookie`、載入 `fleet.yaml`，然後回傳 Owned。

`start` 和 `app` 只在「拿到 outcome 後要做什麼」這點上不同：

- `start` 模式：純粹當作 daemon 運行。沒有 TUI。
- `app` 模式：啟動 TUI；如果是 Owned，還會運行 in-process API server。
  如果是 Attached，則運行 `noop_guard`（不啟動 API server），並把
  daemon 當作權威來源。

## 讓 operator 中招的不對稱性（issue #879）

`app` 模式的行為會因為你是冷啟動還是 attach 而不同：

- **冷啟動（Owned）** — daemon 活在 TUI process 內部。按下 Ctrl+B d
  （或以其他方式離開 TUI）會終止整個 process，連帶殺掉 daemon 和每一個
  agent PTY。
- **熱啟動（Attached）** — daemon 是獨立的 process。Ctrl+B d 只會結束
  TUI；daemon 和 agent 持續運行。重新啟動 `app` 會重新 attach。

operator 期待 Ctrl+B d 永遠是安全的——也就是說，`app` 應該永遠走
Attached 路徑，在冷啟動時自動 spawn 一個 detached daemon，讓這種不對稱性
消失。

PR #903（#879v3）嘗試這麼做，但在踩到兩個既有的競態 bug 之後被 revert
（`fe528c1`）——這兩個 bug 原本被舊的 Owned 模式遮蓋住了（見 issue #879
——#879v4 是針對這些競態的後續修復，而不是 always-Attached 這個轉向本身）。

## Tray 分離契約（#548 Q7）

`tray`（menu bar app，由 `tray` feature 開關控制）絕不碰 daemon 內部。
它的「Start daemon」選單項目會 shell out：

```text
Command::new("agend-terminal").arg("start").arg("--foreground")
```

——等同於在另一個 shell 輸入 `agend-terminal start --foreground`。tray
process 從不持有 daemon 鎖，也從不直接和 API 對話。這種分離就是 #548 Q7
契約；不要讓 tray 繞過它。

## `--foreground` 預設值與 fork-bomb hotfix

`start` 預設是 detached service 模式。`--foreground` 是給那些想讓 daemon
保持附著在 shell 上的 operator 用的 opt-out——在除錯 daemon log，或在
systemd/launchd/Task Scheduler 下運行時很有用。

`spawn_detached`（位於 `src/bootstrap/daemon_spawn.rs`）會把目前的 binary
fork 成 `agend-terminal start --foreground ...`——傳入 `--foreground` 是
必要的，否則子程序會重新進入預設的 detach 分支，遞迴地 spawn 自己。

## 參見

- `src/bootstrap/mod.rs::prepare` — Owned/Attached 決策邏輯。
- `src/bootstrap/daemon_spawn.rs` — detached-spawn 實作。
- `src/tray/mod.rs::start_daemon_via_cli` — tray 的 CLI shell-out。
- `src/app/mod.rs::run_app` — app 的 bootstrap 消費端。
- Issue #879 — always-Attached 轉向（進行中）。