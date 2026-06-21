[English](GIT-BEHAVIOR.md)

# Git Behavior Modification — Git 行為修改

agend-terminal **不會**讓 AI agent 直接對未經處理的 `git` 操作。為了讓多個 agent 能安全地在同一個 repo 上協作，daemon 會在 agent 和你真正的 `git` binary 之間裝設一層輕薄的 shim。**啟動 daemon 之前請先讀完這一頁**——一旦啟動，下面這些修改就會對每一個被 spawn 出來的 agent 生效。

你自己的終端**不受影響**。PATH 注入只發生在 daemon spawn 出來的 agent PTY 內部。從你的 shell 執行 `which git`，解析到的仍然是你平常的 `git` binary。

## 哪些東西被修改了

- **針對 agent 程序的 PATH shim。** `$AGEND_HOME/bin/git` 有一個 symlink，指向一個小型的 Rust binary（`agend-git`）。當 daemon spawn 出 agent 的 PTY 時，這個路徑會被前置到 agent 的 `PATH`。呼叫 `git` 的 agent 最終會跑到這個 shim；shim 幾乎會把每一個命令都轉發給你真正的 `git`（透過 `AGEND_REAL_GIT` 或 `which` 解析）。
- **各 worktree 的 commit hook。** 對於 agent 管理的 worktree，daemon 會把 `core.hooksPath` 指向 `$AGEND_HOME/hooks`，並安裝一個 `prepare-commit-msg` hook，在 commit message 後面附加 `Agend-Agent`、`Agend-Branch`、`Agend-Issued-At`，以及（存在時）`Agend-Task` 等 trailer。若 trailer 已存在則會跳過（具備冪等性）。
- **針對 agent git 操作的 deny matrix。** shim 會拒絕來自未綁定或跨分支情境的某些命令：`git worktree add/remove/move`、`git checkout` 切換到不同分支等。worktree pool 由 daemon 掌管——詳見 [`docs/proposals/agend-git-shim.md`](proposals/agend-git-shim.md) 的 Phase 3 lease。
- **dispatch 時自動 bind/lease（或透過 `bind_self`）。** 當你委派一個帶有 `branch` 欄位的任務給 agent 時，daemon 會自動建立一個受管理的 worktree，用一個 `.agend-managed` 檔案標記它，並寫入一個 `binding.json` 記錄 agent → branch 的連結。
- **worktree 生命週期由 daemon 管理。** 清理是透過 `release_worktree` MCP 工具進行，而不是直接 `git worktree remove`。daemon 端每小時的 GC 掃描（`gc_tick`）接著會**自動移除**那些已 release、超過 grace period、且未被 pin 或 bind 的受管理 worktree——force-reclaim 的候選會被歸檔到可復原的 `.trash`、而非硬刪，過期的 ci-watch lock 也會一併清掉。（被移除或歸檔的 worktree 會連同它的 `target/` 一起處理;此外,此掃描現在**也**會 age-回收帶 `.agend-managed` marker、位於 `home/worktrees`、且**目前未被綁定**的 worktree 之 stale `target/`——亦即其 owner instance 已從 roster 消失,或仍在 roster 但綁到別處/未綁——但保留 worktree 本身。它**絕不**回收目前被綁定的 worktree(不論 owner 是否在跑——該 owner 隨時可能起 build;刪除時會持有 owner 的 `.binding.json.lock` 作為柵欄),也**絕不**碰 markerless 的 `workspace/<agent>/target` 或 agent 自建的 `.claude/worktrees/*/target`。）若只想預覽會被移除什麼而不實際刪除,可使用 `gc_dry_run` MCP 工具。

## 為什麼這樣做

- **多 agent 安全性。** 多個 AI agent 在同一個 repo 裡工作而沒有隔離，就會在同一個分支上發生 race。為每個 agent 建立各自的 worktree，能在 git 這一層直接讓這種情況不可能發生，而不是仰賴 agent 端自律。
- **稽核軌跡。** `Agend-Agent: <name>` trailer 能回答「這個 commit 是哪個 agent 做的？」，不必去解析聊天記錄。在審查自動化工作時很有用，出問題時更有用。
- **生命週期衛生。** 在多 agent 的設定裡，crash 的 agent、過期的 dispatch、被遺棄的分支會很快累積。daemon 的 bind/lease/release 讓清理工作有了單一的負責方。
- **安全護欄。** deny matrix 能在 shim 這一層攔下那些顯而易見的地雷（agent 不小心 checkout `main`、刪掉其他 agent 的 worktree），而不是等事後才發現。

## 風險

- **agent 看到的 `git` 跟你看到的不一樣。** PATH 注入只發生在 daemon spawn 出來的 agent PTY 內部。你自己終端的 `git` 不會被改動。但如果你拿 agent 做的事情去跟你 shell 裡的 `git log` 比對，agent 的命令是經過 shim 的，而 shim 可能已經攔截了它。如果你需要重現 agent 那一條未經處理的 `git` 行為，請設定 `AGEND_GIT_BYPASS=1`。
- **commit 多了額外的 trailer。** 嚴格解析 commit message 的工具（某些 changelog 產生器、某些 CLA bot）可能需要更新它們的 parser。標準的 `git log --format` 輸出不受影響；這些 trailer 是附加在 commit body 之後的。
- **某些命令會出乎意料地被拒絕。** 不熟悉 bind/lease 生命週期的新 agent 或 operator，在執行 `git worktree add` 或 `git checkout main` 時會看到 `agend-git: ERROR ... HINT: ...` 之類的錯誤。錯誤訊息會說明原因以及覆寫（override）的方式。這是刻意設計的，但第一次遇到的人會被嚇到。
- **需要重啟才能套用變更。** 升級後，請執行 `cargo build --release` 並重啟 daemon。shim binary 的路徑在啟動時就固定了；執行中的 agent 在重新 spawn 之前不會套用新的 shim 邏輯。

## 如何退出 / 繞過（opt out / bypass）

在你綁定的 worktree 內進行的例行操作（`status`、`diff`、`log`、`add`、`commit`、`push origin <your-branch>`、`fetch`、在同一個 repo 內 `checkout <existing-branch>`）**會乾淨地通過 shim**——不需要 bypass。先試試未經處理的 `git`；如果 shim 拒絕了，deny 訊息會說明原因。

對於那些確實需要 bypass 的操作：

```bash
# One-off command
AGEND_GIT_BYPASS=1 AGEND_GIT_BYPASS_AGENT=<name> git worktree add ...

# Per-agent persistent override
export AGEND_GIT_BYPASS_AGENT=<name>

# Time-bounded bypass (Unix epoch)
export AGEND_GIT_BYPASS_UNTIL=$(date -v +1H +%s)
```

跳過 shim 就等於跳過安全網（trailer、deny matrix、registry）。請只在明確有意的覆寫情境下使用（operator 手動清理、daemon 自己內部的 git 操作等），不要當成預設做法。

## 想了解更多

- [`docs/FLEET-DEV-PROTOCOL.md`](FLEET-DEV-PROTOCOL.md) §13 — 完整的 bypass 指引
- [`docs/proposals/agend-git-shim.md`](proposals/agend-git-shim.md) — 涵蓋 Phase 1–5 的設計文件
- PR [#446](https://github.com/suzuke/agend-terminal/pull/446)（Phase 1 trailer）· [#447](https://github.com/suzuke/agend-terminal/pull/447)（Phase 2 deny matrix）· [#449](https://github.com/suzuke/agend-terminal/pull/449)（Phase 3 lease）· [#454](https://github.com/suzuke/agend-terminal/pull/454)（Phase 4 GC dry-run）· [#455](https://github.com/suzuke/agend-terminal/pull/455)（Phase 5 hotspot）