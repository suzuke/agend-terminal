[English](GIT-BEHAVIOR.md)

# Git Behavior Modification — Git 行為修改

agend-terminal **不會**讓 AI agent 直接對未經處理的 `git` 操作。為了讓多個 agent 能安全地在同一個 repo 上協作，daemon 會在 agent 和你真正的 `git` binary 之間裝設一層輕薄的 shim。**啟動 daemon 之前請先讀完這一頁**——一旦啟動，下面這些修改就會對每一個被 spawn 出來的 agent 生效。

你自己的終端**不受影響**。PATH 注入只發生在 daemon spawn 出來的 agent PTY 內部。從你的 shell 執行 `which git`，解析到的仍然是你平常的 `git` binary。

## 哪些東西被修改了

- **針對 agent 程序的 PATH shim。** `$AGEND_HOME/bin/git` 的 symlink 會指向 daemon 選定的 git guard。啟用 `use_agentic_git_shim` 時，具備 git 能力的 target 是 vendored `agentic-git` binary；repo 內的 `agend-git` 現在只負責 kill-family guard 與 fail-closed fallback，不再處理 git 命令。daemon spawn agent PTY 時會把 shim 目錄前置到 `PATH`，所以 agent 的 git 操作會先經過檢查，guard 才會呼叫真正的 `git`。
- **各 worktree 的 commit hook。** 對於 agent 管理的 worktree，daemon 會把 `core.hooksPath` 指向 `$AGEND_HOME/hooks`，並安裝一個 `prepare-commit-msg` hook，在 commit message 後面附加 `Agend-Agent`、`Agend-Branch`、`Agend-Issued-At`，以及（存在時）`Agend-Task` 等 trailer。若 trailer 已存在則會跳過（具備冪等性）。
- **針對 agent git 操作的 deny matrix。** shim 會拒絕來自未綁定或跨分支情境的某些命令：`git worktree add/remove/move`、`git checkout` 切換到不同分支等。worktree pool 由 daemon 掌管。目前行為以 live guard 與 [`FLEET-DEV-PROTOCOL.zh-TW.md`](FLEET-DEV-PROTOCOL.zh-TW.md) §10、§12.4、§13 為準；原始 proposal 仍可依[歷史紀錄還原規則](README.zh-TW.md#歷史紀錄)取得。
- **dispatch 時自動 bind/lease（或透過 `bind_self`）。** 當你委派一個帶有 `branch` 欄位的任務給 agent 時，daemon 會自動建立一個受管理的 worktree，用一個 `.agend-managed` 檔案標記它，並寫入一個 `binding.json` 記錄 agent → branch 的連結。
- **worktree 生命週期由 daemon 管理。** 清理是透過 `release_worktree` MCP 工具進行，而不是直接 `git worktree remove`。daemon 端每小時的 GC 掃描（`gc_tick`）接著會**自動移除**那些已 release、超過 grace period、且未被 pin 或 bind 的受管理 worktree——force-reclaim 的候選會被歸檔到可復原的 `.trash`、而非硬刪，過期的 ci-watch lock 也會一併清掉。（被移除或歸檔的 worktree 會連同它的 `target/` 一起處理;此外,此掃描現在**也**會 age-回收帶 `.agend-managed` marker、位於 `home/worktrees`、且**目前未被綁定**的 worktree 之 stale `target/`——亦即其 owner instance 已從 roster 消失,或仍在 roster 但綁到別處/未綁——但保留 worktree 本身。它**絕不**回收目前被綁定的 worktree(不論 owner 是否在跑——該 owner 隨時可能起 build;刪除時會持有 owner 的 `.binding.json.lock` 作為柵欄),也**絕不**碰 markerless 的 `workspace/<agent>/target` 或 agent 自建的 `.claude/worktrees/*/target`。）若只想預覽會被移除什麼而不實際刪除,可使用 `agend-terminal admin gc-dry-run`（#2548：從 `gc_dry_run` MCP 工具移過來）。

## 為什麼這樣做

- **多 agent 安全性。** 多個 AI agent 在同一個 repo 裡工作而沒有隔離，就會在同一個分支上發生 race。為每個 agent 建立各自的 worktree，能在 git 這一層直接讓這種情況不可能發生，而不是仰賴 agent 端自律。
- **稽核軌跡。** `Agend-Agent: <name>` trailer 能回答「這個 commit 是哪個 agent 做的？」，不必去解析聊天記錄。在審查自動化工作時很有用，出問題時更有用。
- **生命週期衛生。** 在多 agent 的設定裡，crash 的 agent、過期的 dispatch、被遺棄的分支會很快累積。daemon 的 bind/lease/release 讓清理工作有了單一的負責方。
- **安全護欄。** deny matrix 能在 shim 這一層攔下那些顯而易見的地雷（agent 不小心 checkout `main`、刪掉其他 agent 的 worktree），而不是等事後才發現。

## 風險

- **agent 看到的 `git` 跟你看到的不一樣。** PATH 注入只發生在 daemon spawn 出來的 agent PTY 內部。你自己終端的 `git` 不會被改動。若要比較受 guard 保護與 bare-git 的行為，請從不受影響的 operator 終端檢查相同 refs；不要從 agent PTY 關閉 guard。
- **commit 多了額外的 trailer。** 嚴格解析 commit message 的工具（某些 changelog 產生器、某些 CLA bot）可能需要更新它們的 parser。標準的 `git log --format` 輸出不受影響；這些 trailer 是附加在 commit body 之後的。
- **某些命令會出乎意料地被拒絕。** 不熟悉 bind/lease 生命週期的新 agent 或 operator，在嘗試 raw worktree 生命週期操作或切換 protected branch 時可能看到 guard error。錯誤訊息會說明原因與 daemon-managed remediation。請把 deny 視為 protocol signal，而不是繞到 guard 下方重試的許可。
- **需要重啟才能套用變更。** 升級後，請執行 `cargo build --release` 並重啟 daemon。shim binary 的路徑在啟動時就固定了；執行中的 agent 在重新 spawn 之前不會套用新的 shim 邏輯。

## guard 拒絕操作時

在綁定的 worktree 內進行例行操作（`status`、`diff`、`log`、`add`、`commit`、`push origin <your-branch>`、`fetch`）**會乾淨地通過 shim**。直接執行正常的 `git`；不要在前面加 bypass variable。

如果 guard 拒絕操作：

1. 停下來並閱讀 deny 提供的 remediation。
2. 使用 daemon 擁有的生命週期操作，例如 `repo` checkout、`bind_self` 或
   `release_worktree`；agent 絕不直接執行 raw `git worktree` 生命週期命令。
3. 若沒有安全路徑，詢問 lead 或 operator。bypass variable 只保留給 daemon
   內部，以及正常復原路徑用盡後、由 operator 明確授權並留下 audit 的單一修復
   命令。agent 不得自行設定。

## 想了解更多

- [`docs/FLEET-DEV-PROTOCOL.md`](FLEET-DEV-PROTOCOL.md) §13 — 完整的 bypass 指引
- [歷史紀錄還原](README.zh-TW.md#歷史紀錄) — 調查 Phase 1–5 時可還原原始 shim 設計
- PR [#446](https://github.com/suzuke/agend-terminal/pull/446)（Phase 1 trailer）· [#447](https://github.com/suzuke/agend-terminal/pull/447)（Phase 2 deny matrix）· [#449](https://github.com/suzuke/agend-terminal/pull/449)（Phase 3 lease）· [#454](https://github.com/suzuke/agend-terminal/pull/454)（Phase 4 GC dry-run）· [#455](https://github.com/suzuke/agend-terminal/pull/455)（Phase 5 hotspot）
