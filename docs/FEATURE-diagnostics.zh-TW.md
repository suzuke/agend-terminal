[English](FEATURE-diagnostics.md)

# 診斷與取證：`agend-terminal doctor / bugreport / capture`

這份文件整理所有「先觀察、再判斷、最後再修」的診斷入口。

## 使用情境

> **Target audience:** Operators — used through CLI or TUI.

當某個 agent 行為異常時，操作者可以先跑 `agend-terminal doctor`，確認問題是在 `fleet.yaml`、backend binary、helper 檔案，還是某個 agent 的連線已經死掉。重點是在改動之前先縮小故障範圍。

如果 Telegram topic 已經跟 registry 漂移，`doctor topics` 會把 live 與 orphan 的條目清楚列出來，讓操作者在動聊天室之前先做安全判斷。

當問題需要交給別人接手時，`bugreport` 會把 runtime snapshot、log 與已 redaction 的設定包成單一檔案，這樣不用再手動拼湊環境資訊。

它們共同的原則很簡單：

- 預設不改狀態。
- 需要改狀態時，會先把可見輸出印給操作員。
- 能紅就紅，能警告就警告，不會悄悄幫你修掉。

## 功能總覽

| 命令 | 目的 | 預設是否改狀態 |
|---|---|---|
| `doctor` | 全域健康檢查 | 否 |
| `doctor topics` | Telegram topic 狀態診斷 | 否；`--cleanup` 才會改 |
| `bugreport` | 匯出可附帶回報的診斷檔 | 否 |
| `capture backend` | 擷取 backend PTY 內容 | 是，寫 capture 檔 |
| `capture promote` | 把 capture 提升成 fixture | 是，寫 fixture / manifest |

這幾個入口對應到不同場景：

- `doctor`：我現在這台機器是不是健康？
- `doctor topics`：Telegram topic 有沒有孤兒或缺失？
- `bugreport`：我要把現在的狀態打包給別人看。
- `capture`：我要留下可重播的原始輸出。

## `doctor`：全域健康檢查

```bash
agend-terminal doctor
```

### 它檢查什麼

`doctor` 會依序印出以下項目：

1. `AGEND_HOME` 是否存在。
2. `.env` 是否存在。
3. `fleet.yaml` 是否存在、是否能 parse、目前有幾個 instance。
4. 每個 instance 是否能透過 daemon 的 runtime helper 找到活著的 agent。
5. thread census。
6. 所有 backend binary 是否在 PATH。
7. `$AGEND_HOME/bin` 的 helper staleness。

### 判讀方式

`doctor` 的輸出不是「有沒有一切完美」，而是「哪些層壞了」。

常見訊號：

- `✗ (not found)`：檔案或目錄缺失。
- `✗ (parse error: ...)`：設定檔已經壞掉。
- `✗ (port stale)`：agent 名稱還在，但實際 PTY / IPC 可能已死。
- `✓ (port responsive)`：daemon 看到該 agent 且 probe 成功。
- `patterns may need update`：backend 版本與我們 calibrate 的版本不一致。

### `doctor` 不做的事

它不會：

- 自動修 fleet.yaml。
- 自動更新 backend。
- 自動重啟 daemon。
- 自動修 helper staleness。

這是故意的。`doctor` 是觀察，不是治療。

## `doctor topics`：Telegram topic 診斷

```bash
agend-terminal doctor topics
agend-terminal doctor topics --cleanup
agend-terminal doctor topics --cleanup --yes
agend-terminal doctor topics --format json
```

### 主要用途

這個入口用來看 Telegram topic registry 與實際群組聊天室的狀態是否一致。

它會把每個 topic 分成兩類：

- `live`：資料庫 / registry 與聊天室都還對得上。
- `orphan`：registry 裡還有，但聊天室狀態已經不一致，或反過來需要清理。

### `--cleanup`

`--cleanup` 不是單純的格式開關，而是會真的執行修復：

1. 先印出診斷結果。
2. 再要求確認，除非加了 `--yes`。
3. 依結果對 registry 與聊天室做同步清理。

這裡的清理包含兩個面向：

- registry 更新
- chat-side delete

### 權限檢查

`doctor topics` 會先 probe bot 是否有 `can_manage_topics`。

這件事的影響是：

- 有權限 → 可以刪聊天室 topic。
- 沒權限 → 只會跳過，並印出 warn。

如果 probe 失敗，輸出會傾向保守，避免誤刪。

### JSON 與 human

`--format human`

- 多行表格。
- 適合人在 terminal 直接看。

`--format json`

- 給腳本或其他工具 pipe。
- 適合在外層做自動化 triage。

### cleanup 動作的回報格式

cleanup 後，CLI 會列出每個動作：

- `deleted topic ... — chat + registry`
- `skipped ... — bot lacks can_manage_topics`
- `skipped ... — API error: ...`

這讓你知道是「真的修掉了」還是「因為權限或 API 失敗跳過」。

## `bugreport`：一鍵匯出診斷包

```bash
agend-terminal bugreport
```

### 輸出位置

`bugreport` 會把檔案寫到：

- 目前工作目錄
- 如果目前工作目錄不可用，則退回 `AGEND_HOME`

檔名格式：

```text
bugreport-YYYYMMDD-HHMMSS.txt
```

### 包含哪些內容

`bugreport` 的內容很適合拿來附在 issue 或回報裡，因為它收了這些章節：

1. 版本資訊
2. `AGEND_HOME`
3. fleet config（已 redacted token / secret）
4. schedules.json
5. daemon status
6. 最新 snapshot
7. event log 最後 50 行
8. 已安裝 backend
9. 目前 active sockets
10. `.env`（已 redacted）

### 為什麼有些內容會 redacted

`bugreport` 會把敏感值遮掉，尤其是：

- token
- key
- secret
- password
- bearer
- authorization
- credential
- group_id

這樣可以在不外洩敏感資訊的前提下，把狀態完整附上。

### `bugreport` 與 `doctor` 的差別

- `doctor`：現場即時健康摘要。
- `bugreport`：可分享的靜態快照。

如果你要把問題交給別人看，通常先跑 `bugreport`，再附上 `doctor` 的輸出會更完整。

## `capture backend`：抓 backend 原始輸出

```bash
agend-terminal capture backend --backend claude --seconds 15
```

### 目的

這個命令是為了把 backend PTY 的原始輸出留下來，讓你可以：

- 做 fixture。
- 做 replay。
- 重現某個 shell / backend 的行為。

### 行為

`capture backend` 會：

1. 起一個對應 backend 的 agent。
2. 在指定秒數內持續讀取 PTY。
3. 把 bytes 寫到 capture 檔。
4. 結束時寫出 `.meta.json` sidecar。

### 捕獲檔位置

預設落在：

```text
$AGEND_HOME/captures/<agent>/<epoch_ms>.cap
```

對應 sidecar：

```text
$AGEND_HOME/captures/<agent>/<epoch_ms>.cap.meta.json
```

### 何時使用

適合以下情境：

- 你想重播某個 backend 的 prompt / response 序列。
- 你要建立 fixture corpus。
- 你要比較不同 backend 的實際輸出差異。

### 重要限制

- `AGEND_CAPTURE_FIXTURES` 沒開時，capture writer 是 no-op。
- 這個功能是觀測工具，不是 replay 引擎本身。
- capture 內容是 raw PTY bytes，不是美化過的摘要。

## `capture promote`：把 capture 變成可重播 fixture

```bash
agend-terminal capture promote \
  $AGEND_HOME/captures/myagent/1234567890.cap \
  sample-scenario \
  --scenario-kind silent_stuck
```

### 它會做什麼

`promote` 會把一個 `.cap` 檔提升成 canonical fixture，流程是：

1. 讀 `.cap.meta.json`。
2. 複製 `.cap` 到 `tests/fixtures/state-replay/<scenario>.raw`。
3. 在 `tests/fixtures/state-replay/MANIFEST.yaml` 追加一筆條目。
4. 可選擇做 `auto_replay` 的警告比對。

### `scenario_kind`

目前合法值是：

- `productive_marker_fire`
- `productive_silence`
- `silent_stuck`
- `hung`
- `real_capture`

這個值是 manifest schema 的一部分，不是任意文字。

### `auto_replay`

如果你加 `--auto-replay`，CLI 會把 `scenario_kind` 對應的 hung/not_hung 預期拿來比對。

注意：

- mismatch 只會 warning，不會回滾 promote。
- 這是刻意的，因為 operator review 仍是 v1 safety net。

### `expected_hung`

這個選項用來做交叉檢查：

- `silent_stuck` / `hung` 通常期待 `hung`
- `productive_*` 通常期待 `not_hung`
- `real_capture` 不做這個比較

## 檔案與資料來源對照

| 命令 | 主要讀取 | 主要寫入 |
|---|---|---|
| `doctor` | `fleet.yaml`、`$AGEND_HOME/bin`、runtime probe | 無 |
| `doctor topics` | Telegram channel / registry | 可選：registry + 聊天室刪除 |
| `bugreport` | `fleet.yaml`、`schedules.json`、snapshot、event log、`.env` | `bugreport-*.txt` |
| `capture backend` | backend PTY | `$AGEND_HOME/captures/.../*.cap` + `.meta.json` |
| `capture promote` | `.cap` + `.meta.json` | `tests/fixtures/state-replay/*.raw` + `MANIFEST.yaml` |

## 典型工作流程

### 1. 先做健康檢查

```bash
agend-terminal doctor
```

看出來是：

- file 層壞了
- backend 不在 PATH
- helper staleness
- agent port stale

### 2. 若是 Telegram topic 問題

```bash
agend-terminal doctor topics --format json
```

先看哪些是 orphan，再決定要不要 cleanup。

### 3. 若要回報 issue

```bash
agend-terminal bugreport
```

把產出的檔案貼到 issue 或附檔。

### 4. 若要新增 replay fixture

```bash
export AGEND_CAPTURE_FIXTURES=1
agend-terminal capture backend --backend claude --seconds 20
agend-terminal capture promote <cap> <name> --scenario-kind silent_stuck
```

## 常見誤區

### 把 `doctor` 當修復工具

不對。`doctor` 是觀察與列舉問題，不是自動修復。

### 忽略 `doctor topics` 的 permission probe

如果 bot 沒有 `can_manage_topics`，cleanup 會保守跳過。
不要把這當成程式沒壞；多半是權限不足。

### 把 bugreport 當完整安全備份

它是診斷包，不是 restore 檔。
你可以靠它看出狀態，但不要拿它當正式的資料備份。

### 把 capture 當 fixture 的最終版本

`capture backend` 只負責抓原始資料。
要讓 replay harness 看得到，還要 `capture promote`。

## 對應原始碼

- `src/cli.rs`：`run_doctor`、`run_doctor_topics`、`capture_backend`
- `src/main.rs`：CLI subcommand routing
- `src/bugreport.rs`：bugreport 內容與 redaction
- `src/capture.rs`：capture sink、rotation、promote
- `src/bootstrap/doctor_topics.rs`：topic classification 與 cleanup
- `src/bootstrap/doctor.rs`：fleet health validation

## 實務建議

1. 遇到 agent staleness，先跑 `doctor`，不要直接刪檔。
2. Telegram topic 問題先看 `doctor topics`，再做 cleanup。
3. 需要貼給別人看的狀態，優先用 `bugreport`。
4. 做 capture 時，記得確認 `AGEND_CAPTURE_FIXTURES=1`。
5. promote 前先看 `scenario_kind`，不要用錯類型造成 manifest 語義錯位。