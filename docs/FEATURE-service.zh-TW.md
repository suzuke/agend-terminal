[English](FEATURE-service.md)

# 服務管理：`agend-terminal service`

這份文件說明 `service` 子命令如何把 daemon 交給作業系統管理，
以及三個平台各自的落點與限制。

## 使用情境

> **Target audience:** Operators — used through CLI or TUI.

操作者希望機器登入後 daemon 自動啟動，而不是每次都手動跑 `agend-terminal start`。`service install` 會把 daemon 交給作業系統管理，讓平台負責開機登入時的啟動與 crash 後重拉。

在 binary 升級之後，操作者希望 service manager 指向新的可執行檔路徑。重新執行 `service install` 就會重新產生 artifact，帶入最新的 binary path 與 `AGEND_HOME`。

當機器要退役，或不再需要這個 daemon 時，`service uninstall` 可以乾淨地解除註冊，讓平台回到可預期的狀態。

## 這個功能解決什麼問題

`agend-terminal` 本身可以在前景、背景、TUI 或 daemon 模式運作，
但若希望它在登入後自動啟動、在 crash 後重新拉起，
就不能只靠手動執行。
`service` 提供的是「交給 OS 管理生命周期」這個入口。

你可以把它理解成兩件事：

1. 把目前這個 `agend-terminal` binary 的絕對路徑寫進平台的 service manager。
2. 讓平台在使用者登入時自動啟動，並在 daemon 掛掉後嘗試重新拉起。

重要前提：

- 這是 **user-level** 設定，不需要 root / admin。
- 它不會讓 daemon 自我監督；真正的 supervisor 是 OS 自己。
- install / uninstall 都是 idempotent，重跑不會破壞既有狀態。

## 支援平台

| 平台 | 服務管理器 | 輸出位置 | 註冊方式 |
|---|---|---|---|
| macOS | `launchd` | `~/Library/LaunchAgents/com.agend-terminal.daemon.plist` | `launchctl load -w` |
| Linux | `systemd --user` | `~/.config/systemd/user/agend-terminal-daemon.service` | `systemctl --user enable --now` |
| Windows | Task Scheduler | `\AgendTerminalDaemon` | `schtasks /Create /XML` |

這三個平台都屬於使用者層級。
如果你在沒有對應平台工具的環境中執行，`install` 會回報平台不支援。

## 三個子命令

```bash
agend-terminal service install
agend-terminal service uninstall
agend-terminal service status
```

### `install`

`install` 會做以下事情：

1. 解析目前執行中的 `agend-terminal` binary 絕對路徑。
2. 依平台套用對應 template，並把路徑與 `AGEND_HOME` 填入。
3. 寫入平台要求的 artifact。
4. 呼叫平台 service manager 完成註冊。

如果你重跑 `install`，它會重新產生 template，這對 binary 更新或 `AGEND_HOME` 變動很有用。

### `uninstall`

`uninstall` 會嘗試：

1. 解除註冊。
2. 刪除對應的 service artifact。

如果原本就沒有安裝，這會是 no-op 成功。

### `status`

`status` 只回答三種結果：

- `running`
- `stopped`
- `not_installed`

注意：`status` 是在查平台 service manager，而不是直接看 daemon 內部狀態。

## macOS 行為

macOS 使用 `launchd` 的 user agent。

### 位置

- plist：`~/Library/LaunchAgents/com.agend-terminal.daemon.plist`
- label：`com.agend-terminal.daemon`

### install

macOS 的流程是：

1. 先把 template render 成 plist。
2. 寫入 LaunchAgents 目錄。
3. 先 `launchctl unload -w`，再 `launchctl load -w`。

這個順序讓重新安裝保持 idempotent。

### status

判斷邏輯是：

- plist 不存在 → `NotInstalled`
- `launchctl list <label>` 成功，且輸出包含 PID → `Running`
- plist 存在但未載入或未執行 → `Stopped`

### 注意事項

- `launchctl` 失敗時，status 不會誤判成 `running`。
- 如果 plist 已存在，但 daemon 沒有被 launchd 管理，仍會顯示 `stopped`。
- `StandardOutPath` / `StandardErrorPath` 已固定為 `/dev/null`；真正的日誌輸出走 daemon tracing。

## Linux 行為

Linux 使用 `systemd --user`。

### 位置

- unit：`~/.config/systemd/user/agend-terminal-daemon.service`
- 若有設定 `XDG_CONFIG_HOME`，會先使用它。

### install

Linux 的流程是：

1. render unit template。
2. 寫入 `~/.config/systemd/user/`。
3. 執行 `systemctl --user daemon-reload`。
4. 執行 `systemctl --user enable --now agend-terminal-daemon.service`。

這樣可以同時做到開機後登入自啟，以及立即啟動。

### status

判斷邏輯是：

- unit 檔不存在 → `NotInstalled`
- `systemctl --user is-active` 成功 → `Running`
- unit 存在但未 active → `Stopped`

### 注意事項

- 在某些 CI 或無 systemd session bus 的環境中，`enable --now` 可能失敗，但 unit 檔案仍然已經寫入。
- 這種情況下，install 會保留檔案並把 activation 問題當成 warning。
- 因為是 user-level unit，不需要 sudo。

## Windows 行為

Windows 使用 Task Scheduler。

### 位置

- task 名稱：`\AgendTerminalDaemon`
- XML cache：`$AGEND_HOME/service/scheduler.task.xml`

### install

Windows 的流程比較特別：

1. render XML template。
2. 以 UTF-16 LE + BOM 寫入 XML。
3. 執行 `schtasks /Create /XML <path> /F`。

XML 會先套用 `xml_escape`，避免 `&`、`<`、`>`、`"`、`'` 破壞結構。

### status

判斷邏輯是：

- XML cache 不存在 → `NotInstalled`
- `schtasks /Query /TN \AgendTerminalDaemon /FO LIST` 成功且內容含 `Running` → `Running`
- XML 存在但查詢不是 active → `Stopped`

### 注意事項

- `schtasks` 失敗時，install 仍可能保留 XML，方便你檢查 render 後的內容。
- task scheduler 的命名是固定的，不會依 instance 改名。
- Windows 也不需要 admin，只要使用者自身可建立排程即可。

## idempotency 規則

| 操作 | 已存在時 | 不存在時 |
|---|---|---|
| `install` | 重新產生 artifact 並重註冊 | 正常安裝 |
| `uninstall` | 刪除 artifact 並解除註冊 | no-op 成功 |
| `status` | 回報 `running` / `stopped` | 回報 `not_installed` |

這裡的 idempotency 是針對「操作結果可重跑」，不是說 platform command 一定完全安靜。
例如 `launchctl unload`、`systemctl daemon-reload`、`schtasks /Create` 可能會輸出 warning，但功能上仍保持可重跑。

## 與 daemon lifecycle 的關係

`service` 只負責把 daemon 放到 OS supervisor 底下。
真正的 daemon 邏輯仍然在 `start` / `app` / `daemon` 路徑中。

換句話說：

- `service install`：告訴 OS「請幫我啟動這個 binary」
- `agend-terminal start`：真正跑 daemon
- `service status`：看 OS 這邊是不是還握著 service 註冊

這也代表如果你更新 binary，通常要重新 install 一次，
讓 service manager 持有最新的絕對路徑與 `AGEND_HOME`。

## 常見操作流程

### 第一次安裝

```bash
agend-terminal service install
agend-terminal service status
```

你通常會先確認：

- artifact 已寫入
- service manager 已接受註冊
- status 顯示 `running` 或 `stopped`，但不是 `not_installed`

### 更新 binary 後重裝

```bash
cargo build --release
agend-terminal service install
```

這會重新 render template，帶入新的 `current_exe()` 路徑。

### 解除安裝

```bash
agend-terminal service uninstall
agend-terminal service status
```

如果你想確認真的清掉了，`status` 應該回 `not_installed`。

## 失敗排查

### `this platform is not supported`

表示目前編譯目標沒有對應的 service manager 實作。
確認你是在 macOS / Linux / Windows 其中之一。

### `status` 回 `stopped`

表示平台認得這個 service artifact，但它現在沒在跑。
建議檢查：

- binary 是否還存在
- `AGEND_HOME` 是否正確
- service manager log 是否有啟動失敗
- daemon 啟動後是否立刻 panic

### Windows XML 可以寫，但 task 沒出現

常見原因：

- `schtasks` 不可用
- 目前使用者沒有建立排程的權限
- XML 中有未 escape 的字元

### Linux unit 寫入成功但沒有啟動

常見原因：

- 沒有 systemd user session bus
- `systemctl --user` 無法在該環境運作
- unit 內容指向了不存在的 binary

### macOS plist 存在但沒有載入

常見原因：

- `launchctl load -w` 失敗
- plist 路徑被移動
- binary 已更新，但 plist 還保留舊路徑（重跑 install 即可）

## 跟其他設定的關係

`service` 只管 OS supervisor。
它不負責：

- fleet.yaml 的 agent 配置
- runtime-config.json 的 runtime threshold
- MCP JSON 的 backend 端設定
- bugreport / capture 的診斷輸出

這些是不同層次的配置。
如果你在改完 fleet 或 runtime config 後遇到 daemon 行為異常，
先看 `doctor`，再看 `service status`，最後再重裝 service。

## 對應原始碼

- `src/main.rs`：`Commands::Service` 與 CLI 文案
- `src/service/mod.rs`：跨平台共用 helper
- `src/service/macos.rs`：launchd 實作
- `src/service/linux.rs`：systemd user 實作
- `src/service/windows.rs`：Task Scheduler 實作
- `src/daemon/restart.rs`：supervisor 偵測與重啟語意

## 實務建議

1. 安裝後立刻跑一次 `status`。
2. 更新 binary 後重新 `install`，不要假設舊 artifact 會自動更新。
3. 若你在 CI 或臨時環境測試，保留 `install` 產生的檔案比強求 service 真正啟動更有用。
4. 若 `status` 非 `not_installed` 但 daemon 還是沒反應，先看 platform manager log，不要先懷疑 CLI。
5. 這個功能是「外部 supervisor 入口」，不是 daemon 的自愈機制。