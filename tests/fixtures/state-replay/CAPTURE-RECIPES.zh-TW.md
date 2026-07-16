[English](CAPTURE-RECIPES.md)

# PTY Fixture 擷取手冊

供 operator 端批次擷取 PTY fixture 使用的結構化操作方法。Agent 無法 spawn 新的互動式 backend（沒有真正的 PTY allocator），因此所有 fixture recording 都必須由 operator 在 live terminal session 中完成。每個 `.raw` 檔案都會記錄 backend 發出的精確 byte stream，包括 ANSI escape、cursor positioning 與 SGR color code。這些 byte 是 state detection、dismiss-pattern matching 與 composite-signature framework（#996）的 ground truth。

`cli_version` 很重要：pattern 會偵測不同 CLI version 之間可能改變的 literal string。一律記錄 version 與日期，才能區分 regression 與 upstream UI change。

> **目前 CORPUS（於 `main@1d83b423`、2026-07-16 重新驗證）：**
> 以 `MANIFEST.yaml` 為準，目前含有 **44 個 fixture**，其中有 **8 個帶
> schema-v2 label 的 fixture**：六個 `silent_stuck`、一個
> `productive_marker_fire` 與一個 `productive_silence`。Manifest 涵蓋 Agy、
> Claude、Codex、Kiro/Kiro CLI 與 OpenCode；目前**尚無 Grok fixture**。
> Gemini 已退役，不再出現在 live manifest 中。§F685-CORPUS.3 保留較小的
> launch corpus 作為歷史 provenance，不是目前數量。

決策：`d-20260514015214320625-1`（`#685` 的 N 個 sub-task 之 5）。
Sibling chain：sub-task 1（Hung audit，PR #750）、2（F39 audit，PR #752）、
3（縮窄 Gemini regex，PR #763）、4（F9 productive-output gate，PR #766）。

維護：section ID（`§F685-CORPUS.1`–`§F685-CORPUS.7`）是穩定的 contract anchor。
套用 sub-task 1 的 M1/M2/M3 discipline（inline comment 交叉參照
`§F685-CORPUS.<n>`；本手冊以 `rg <pattern>` 提示 source reference）。

---

## §F685-CORPUS.1——用途與跨領域性質

此 corpus 是多個 `#685` deliverable 共用的**基礎設施**：

- **F9 promotion gate**——對照 `expected_hung_classification` ground truth，
  測量 `check_hang` productive-silence path 的分類。Promotion criteria 要求在
  N ≥ 300 個 not-stuck fixture 上 FP < 1%（95% confidence 的 statistical Rule
  of Three），加上 2 週穩定的 shadow telemetry。
- **F39 mitigation selection**——`docs/HUNG-STATE-TRANSITIONS.md §F39.4` 中的
  六個 hypothesis (a)/(b)/(c)/(d)/(e)/(f) 需要以 FP measurement 選出結果。
  使用相同 corpus，但走不同 harness pass（依 §F685-CORPUS.4）。
- **Recovery calibration**——live Stage-1 recovery action 預設仍是 shadow，
  promotion 前需要對 detection FP/FN 有足夠信心。

此 corpus **不**專屬於 F9 或 F39——它是兩者共同仰賴的 **measurement substrate**。
這些明確的 contract section 與 top-level integration test 讓這項跨領域角色保持可見。

## §F685-CORPUS.2——Manifest schema extension

位於 `rg "struct ReplayFixture" src/state/tests.rs` 的 `ReplayFixture` 有七個
optional field（serde default 維持對 schema-v1 fixture 的向後相容性）：

| 欄位 | 型別 | 用途 |
|---|---|---|
| `scenario_kind` | `Option<String>` | 可為 `scrollback_static`、`screen_change_same_state`、`priority_oscillation`、`productive_marker_fire`、`productive_silence`、`silent_stuck`、`productive_bursty` 之一。驅動 harness measurement dispatch。 |
| `expected_hung_classification` | `Option<String>` | F9 promotion measurement 的 ground truth。可為 `not_hung`、`hung`、`ambiguous` 之一。 |
| `expected_oscillation_count` | `Option<u32>` | F39 measurement：啟用 wall-clock injection 時，此 trace 應產生多少次 priority transition（已延後——§F685-CORPUS.6）。 |
| `productive_marker_expectations` | `Vec<{time_ms, source}>` | F9 詳細 measurement：哪些 marker 在什麼時間觸發。沒有 expectation 的 fixture 預設為空。 |
| `capture_kind` | `Option<String>` | Measurement provenance，例如 `real`、`synthetic` 或 `synthetic_from_real_template`。依 §F685-CORPUS.4 驅動 source-separated reporting。 |
| `provenance` | `Option<String>` | 人類可讀的來源：PR number、operator-session note，或 `synthetic from <template>`。Audit trail。 |
| `schema_version` | `u32`（預設 `1`） | Future-compatibility marker。**Phase 1 不做 runtime enforcement**——僅供資訊使用；未來 schema 變更時提高版本並加入 migration。 |

**向後相容性：** schema-v1 fixture 會透過 serde default 原樣解析。
`state::tests::replay_manifest_regression` test 會固定此路徑。

## §F685-CORPUS.3——初始 corpus（歷史 launch snapshot）

> 本節的數量與 backend name 描述 Phase 1 launch plan。保留它們是為了解釋原始
> measurement design。目前 coverage 請以上方 current-corpus banner 與
> `MANIFEST.yaml` 為準。

Phase 1 文件列出三個 synthetic schema-v2 fixture，加上當時的 schema-v1 baseline：

| Fixture | Backend | Scenario | Classification | Capture |
|---|---|---|---|---|
| `f685-f9-positive-savedfile.raw` | claude-code | `productive_marker_fire` | `not_hung` | `synthetic` |
| `f685-f9-negative-saved-prose.raw` | claude-code | `productive_silence` | `not_hung` | `synthetic` |
| `f685-silent-stuck-stub.raw` | gemini | `silent_stuck` | `hung` | `synthetic_from_real_template`（歷史上的 planned stub；不在目前 manifest 中） |

Launch 時，13 個 legacy schema-v1 fixture（每個 backend × {thinking, tooluse,
+偶爾的 perm/update} 各一個）在 `replay_manifest_regression` 下不修改 manifest
即可解析。

Launch coverage 的優先項目是 **Gemini + Kiro**（issue `#659` 明確將它們列為
known-stuck backend）。Gemini 後來退役，由 Agy 取代。目前 corpus 已加入 Agy
coverage；Grok 則仍是沒有 labelled 或 schema-v1 fixture 的 active backend。

此初始集合在統計上**不足以**通過 FP < 1% / FN < 10% gate。Corpus growth 依
§F685-CORPUS.6 委派給 operator 與後續 sub-task。

## §F685-CORPUS.4——Measurement 方法

### 共用 corpus 上的兩條 pipeline

1. **F9 productive-signal pipeline**（active）。
   - 讓每個 schema-v2 fixture 通過 `VTerm` + `infer_productivity` replay。
   - 將產生的 `ProductivitySignal` 與 `scenario_kind` expectation 比較：
     - `productive_marker_fire` → 預期 `Productive { source: Marker(_) }` 或
       `Productive { source: Heartbeat }`
     - `productive_silence` → 預期 `NoSignal`
     - `silent_stuck` → 預期 `NoSignal`
   - Smoke test：`rg "corpus_measurement_smoke_f9_marker_signals" src/state/tests.rs`。
2. **F39 oscillation pipeline**（已延後；見 §F685-CORPUS.6）。
   - 讓每個 schema-v2 fixture 通過 `StateTracker::feed` replay，並在 chunk
     之間注入 wall clock。
   - 將觀察到的 transition count 與 `expected_oscillation_count` 比較。
   - 這需要 harness extension，依 manifest 的 per-chunk timing metadata 回溯
     `since`。它不在 Phase 1 範圍內。

### 以 transition 為單位（不是每個 tick）

單次 false-Hung transition × 100 ticks 只算**一個** FP event。Classifier 只會在
進入 transition 時回傳 `true`（sub-task 1 invariant 5a/5b）；harness aggregation
也採相同方式。

### Report 依來源分開

Report 分成三行：

```
F9 measurement (N at report time):
  Real:        X/Y  (high signal value — actual operator sessions)
  Synthetic:   X/Y  (specific scenario coverage — crafted to exercise paths)
  Combined:    X/Y  (aggregate)
```

Synthetic FAIL 可立即採取行動（marker 或 pattern 有誤）。Real PASS 提供實證信心。
**Real FAIL 是最有價值的 data point**——它會揭露與 production 相關的 FP 或 FN。

Integration test `rg "corpus_count_report" tests/fixture_corpus_measurement.rs`
透過 `eprintln!` 發出此 report（以 `cargo test -- --nocapture` 顯示），並對
`scenario_kind` shape 套用寬鬆 gate（每個 core kind 至少一個）。N 增加時會逐步
收緊 strictness。

### 統計最低數量（委派給 corpus growth）

- 95% confidence 下 **FP < 1%**（Rule of Three）：N ≥ 300 個 not-stuck fixture
- 合理 confidence 下 **FN < 10%**：N ≥ 30 個 known-stuck fixture

Phase 1 交付 N = 3 個 schema-v2 fixture 加上 harness；目前 manifest 有 N = 8 個
labelled fixture。**Harness 會依目前 N 回報 rate**；promotion criteria（F9 commit
message 與 F39 audit 中）要求 `N ≥ minimum AND rate < threshold`。這刻意將 issue
的 `FP < 1%` 從「在單一 PR 達標」重新框定為「透過 corpus 隨時間成長而達標」。

### Shadow 與 active F9 measurement

- **Shadow mode**（預設，未設定 `AGEND_PRODUCTIVE_GATE`）：F9 telemetry 會觸發，
  但不改變 classification。可在不影響 production 的情況下估算 FP rate。
- **Active mode**（`AGEND_PRODUCTIVE_GATE=1`）：F9 會實際分類。
  **Promotion-criteria measurement 必須使用此模式。** Test code 使用
  `tests/common/env_gate.rs` 的 `with_f9_gate(true, || { ... })`（以及
  `src/health.rs::tests` 中的 unit-test mirror）。

## §F685-CORPUS.5——擷取工作流程

使用下方 operator-side 操作方法；不需要新的 CLI 工具。Synthetic fixture 可使用
`printf '%b' ...` 寫入手刻 byte sequence（範例可查看 F685 fixture 的 git log）。

通用 recording loop 如下：

```sh
script -q /tmp/<backend>-session.raw <cli-command>
# 互動：觸發 thinking、等待完成、離開
# 將檔案複製到 tests/fixtures/state-replay/，並新增 manifest entry
```

若供 F9/F39 measurement 使用，除了普通 capture metadata 外，也要加入 schema-v2
measurement field：

```yaml
- file: my-new-capture.raw
  backend: kiro-cli
  cli_version: "X.Y.Z"
  recorded_on: "YYYY-MM-DD"
  scenario: "human-readable summary"
  expected_transitions: [starting, ...]
  expected_final_state: ...
  scenario_kind: silent_stuck  # measurement 必填
  expected_hung_classification: hung  # measurement 必填
  capture_kind: real  # 或 synthetic / synthetic_from_real_template
  provenance: "#NNN operator session 2026-..."
  schema_version: 2
```

## 設定檢查清單

開始 capture session 前，請準備環境：

- [ ] 建立 throwaway 工作目錄：
  ```bash
  export CAPTURE_DIR=$(mktemp -d /tmp/agend-fixture-capture-XXXXX)
  cd "$CAPTURE_DIR"
  git init  # 有些 backend 需要 git repo
  ```

- [ ] 驗證 target backend binary 位於 PATH：
  ```bash
  which claude && claude --version
  which codex && codex --version
  which kiro && kiro --version
  which opencode && opencode --version
  which agy && agy --version
  which grok && grok --version
  ```

- [ ] 若要擷取 clean-state，先備份 backend config：
  ```bash
  # 僅在 trust-prompt / first-run scenario 需要
  mv ~/.claude ~/.claude.bak 2>/dev/null
  mv ~/.codex ~/.codex.bak 2>/dev/null
  ```

- [ ] 確認 `script` 語法（macOS BSD 與 GNU 不同）：
  ```bash
  # macOS BSD（預設）：
  script -q /tmp/test-capture.raw claude --version
  # GNU（Linux）：
  script -q -c "claude --version" /tmp/test-capture.raw
  ```
  下方所有操作方法都使用 **macOS BSD** 語法。在 Linux 上請改為
  `script -q -c "<command>" <output-file>`。

- [ ] 準備 MANIFEST.yaml entry template（每次 capture 都複製一份）：
  ```yaml
  - file: <backend>-<scenario>.raw
    backend: <backend-name>
    cli_version: "<version>"
    recorded_on: "<YYYY-MM-DD>"
    scenario: "<one-line description>"
    expected_transitions: [starting, ...]
    expected_final_state: <state>
    expected_final_detect: <state-or-null>
    capture_kind: real_pty
    provenance: "<issue-ref> batch capture by operator"
  ```

---

## 各 Scenario 操作方法

### Priority 1：Phase 2a 緊急項目

#### R1. `claude-yes-proceed.raw`（約 3 分鐘）

**目標**：擷取 Claude Code 詢問 permission 時出現的「Yes, proceed」confirmation
modal（例如 `--dangerously-skip-permissions` confirmation 或 update prompt）。
Phase 2a keystroke audit 需要此 fixture。

> **#1546 用途**：這是 edit/confirm permission 類別。其 footer + option chrome
>（`Esc to cancel · Tab to amend`、帶編號的 `❯ 1. Yes / … / N. No`）是 #1546
> 使用的 zero-FP detection anchor。請一併擷取 R1/R2/R2b，讓 #1546 判斷該 chrome
> 在不同 permission type（edit、trust、bash）是否**穩定**——`Tab to amend` 是 edit
> 專用，因此單一 footer anchor 不一定能涵蓋全部。

```bash
# 1. 開始擷取
script -q "$CAPTURE_DIR/claude-yes-proceed.raw" claude

# 2. 觸發：輸入會讓 "Yes, proceed" confirmation 出現的請求。
#    若剛好有 update，也可使用應把 "Yes, proceed" 顯示為選項的
#    update-now prompt。

# 3. 先不要關閉 modal——靜置 2–3 秒，讓完整 ANSI rendering 被擷取。

# 4. 按 Ctrl-C 或輸入 /exit 結束 session。
```

**驗證**：`xxd claude-yes-proceed.raw | grep -c '1b\['` 應顯示有 ANSI escape
sequence。檔案應含 literal string「Yes, proceed」。

```bash
grep -c "Yes, proceed" "$CAPTURE_DIR/claude-yes-proceed.raw"
# 預期：>= 1
```

**時間估計**：約 3 分鐘

---

### Priority 2：#996 Framework 支援

#### R2. `claude-trust-prompt.raw`（約 2 分鐘）

**目標**：擷取 Claude Code 第一次在 untrusted directory 啟動時出現的 trust-folder
modal。Composite-signature discriminator calibration 需要此 fixture。

> **#1546 用途**：trust-folder modal 的 footer/option chrome 可能與 edit permission
>（R1）**不同**——它的措辭是「Do you trust the files…」，不是「Tab to amend」。
> #1546 需要用它判斷一個 footer anchor 是否能涵蓋所有 permission type，或 edit /
> trust / bash 各自需要自己的 chrome match。

```bash
# 1. 確保 clean state（此目錄先前未被信任）
rm -rf /tmp/agend-untrusted-test && mkdir /tmp/agend-untrusted-test
cd /tmp/agend-untrusted-test && git init

# 2. 開始擷取
script -q "$CAPTURE_DIR/claude-trust-prompt.raw" claude

# 3. 等待 trust modal 出現（"Do you trust the files in
#    this folder?" 或 "Yes, I trust the files in this folder"）。
#    讓它完整 render（2–3 秒）。

# 4. 按 Ctrl-C 離開，不要關閉 modal（我們要的是 modal byte）。
```

**驗證**：檔案應含「trust」與 ANSI box-drawing character。

```bash
grep -c "trust" "$CAPTURE_DIR/claude-trust-prompt.raw"
```

**時間估計**：約 2 分鐘

---

#### R2b. `claude-bash-perm.raw`（#1546——bash-command permission，約 2 分鐘）

> 編號為 `R2b`，因為它屬於 permission group（R1 edit、R2 trust）；下方 `R3`–`R10`
> 是 productive-marker capture。

**目標**：擷取 Claude Code 在執行 **shell command** 前顯示的 permission dialog。
#1546 需要知道 bash-permission footer/option chrome 是否與 edit-permission footer
（`Esc to cancel · Tab to amend`）相同，或有所差異——`Tab to amend` 是 edit 專用，
因此 bash 可能顯示不同 footer。這會決定 #1546 detection 能否使用單一 footer anchor，
或每種 permission type 都需要自己的 chrome match。

> ⚠ **不要使用 `--dangerously-skip-permissions`。** Bypass mode 會完全略過
> permission prompt——這正是 fleet agent（以 bypass 執行）看不到它，也無法自行
> capture 此 fixture 的原因。請在**不使用 bypass** 的情況下啟動 Claude。

```bash
# 1. Throwaway repo（保持 capture 乾淨）
rm -rf /tmp/agend-bash-perm-test && mkdir /tmp/agend-bash-perm-test
cd /tmp/agend-bash-perm-test && git init

# 2. 開始擷取——不使用 bypass 的 claude
script -q "$CAPTURE_DIR/claude-bash-perm.raw" claude

# 3. 要求它執行 shell command，例如輸入：run: ls -la
#    Bash-permission dialog 出現後，讓它完整 render（2–3 秒）。
#    不要選擇任何 option。

# 4. 按 Ctrl-C 離開，不要回答（我們要的是 dialog byte）。
```

**驗證**：確認 ANSI escape 存在，並 dump footer/option 措辭，讓 #1546 能比較不同
permission type 的 chrome：

```bash
xxd "$CAPTURE_DIR/claude-bash-perm.raw" | grep -c '1b\['
grep -aoE "Esc to cancel[^|]*|Tab to amend|enter to confirm|Allow|Do you want" \
  "$CAPTURE_DIR/claude-bash-perm.raw" | sort -u
```

**MANIFEST entry**：

```yaml
- file: claude-bash-perm.raw
  backend: claude-code
  cli_version: "<version>"
  recorded_on: "<YYYY-MM-DD>"
  scenario: "bash-command permission dialog (run shell command, dialog not answered)"
  expected_transitions: [starting, permission]
  expected_final_state: permission
  expected_final_detect: permission
  capture_kind: real_pty
  provenance: "#1546 operator capture"
```

**時間估計**：約 2 分鐘

---

### Priority 3：Productive Marker 擷取（#1014 / S2）

每個 backend 都需要兩種 scenario：

- **productive_marker_fire**：tool result 回傳，且畫面上出現 visible marker
  （例如 file-write confirmation、command output）。
- **productive_silence**：active session 長時間暫停，沒有 visible progress marker
  （hung detection 的「silent stuck」情況）。

#### R3. Claude productive_marker_fire（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/claude-productive-marker.raw" claude

# 1. 請 Claude 建立一個小檔案：
#    "Create a file called hello.txt with the content 'hello world'"
# 2. 等待 tool use 完成，且 file-write confirmation 出現在畫面上。
# 3. 等待 response 完全結束。
# 4. 輸入 /exit
```

#### R4. Claude productive_silence（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/claude-productive-silence.raw" claude

# 1. 詢問需要長時間思考的複雜問題：
#    "Explain the mathematical proof of Fermat's Last Theorem in detail"
# 2. Claude thinking/streaming（spinner 可見、沒有 tool-use marker）時等待 30–60 秒。
# 3. 在 response 中途按 Ctrl-C，擷取 "stuck" state。
```

#### R5. Codex productive_marker_fire（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/codex-productive-marker.raw" codex

# 1. 請 Codex 建立檔案。
# 2. 等待 apply-patch confirmation 與 completion。
# 3. 輸入 /exit
```

#### R6. Codex productive_silence（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/codex-productive-silence.raw" codex

# 1. 詢問複雜問題。
# 2. Thinking/streaming 期間等待 30–60 秒。
# 3. 在 response 中途按 Ctrl-C。
```

#### R7. Agy productive_marker_fire（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/agy-productive-marker.raw" agy

# 1. 請 Agy 建立檔案。
# 2. 等待 tool completion marker。
# 3. 離開 session。
```

#### R8. Agy productive_silence（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/agy-productive-silence.raw" agy

# 1. 詢問複雜問題。
# 2. Generation 期間等待 30–60 秒。
# 3. 在 response 中途 interrupt。
```

#### R9. Kiro productive_marker_fire（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/kiro-productive-marker.raw" kiro

# 1. 請 Kiro 建立檔案。
# 2. 等待 file-write tool completion marker。
# 3. 輸入 /exit
```

#### R10. Kiro productive_silence（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/kiro-productive-silence.raw" kiro

# 1. 詢問複雜問題。
# 2. Thinking 期間等待 30–60 秒。
# 3. 在 response 中途按 Ctrl-C。
```

#### R11. OpenCode productive_marker_fire（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/opencode-productive-marker.raw" opencode

# 1. 請 OpenCode 建立檔案。
# 2. 等待 tool completion。
# 3. 輸入 /exit
```

#### R12. OpenCode productive_silence（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/opencode-productive-silence.raw" opencode

# 1. 詢問複雜問題。
# 2. Thinking 期間等待 30–60 秒。
# 3. 在 response 中途按 Ctrl-C。
```

#### R13. Grok productive_marker_fire（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/grok-productive-marker.raw" grok

# 1. 請 Grok 建立檔案或執行 visible tool action。
# 2. 等待 completion marker。
# 3. 離開 session。
```

#### R14. Grok productive_silence（約 5 分鐘）

```bash
script -q "$CAPTURE_DIR/grok-productive-silence.raw" grok

# 1. 詢問複雜問題。
# 2. Generation 期間等待 30–60 秒。
# 3. 在 response 中途 interrupt。
```

---

## 擷取後工作流程

### 1. 清理敏感資訊

Commit 前，檢查每個 `.raw` 檔案是否含有 sensitive content：

```bash
# 檢查 API key、token、個人 path
for f in "$CAPTURE_DIR"/*.raw; do
  echo "=== $(basename $f) ==="
  strings "$f" | grep -iE 'api.key|token|secret|password|/Users/[^/]+' | head -5
done
```

若發現 sensitive content，請在 clean environment 中重新 capture，或使用 `sed`
取代特定字串（維持 byte offset 非常重要——只取代不會影響 ANSI escape positioning
的內容）。

### 2. 複製到 fixture 目錄

```bash
cp "$CAPTURE_DIR"/*.raw tests/fixtures/state-replay/
```

### 3. 加入 MANIFEST.yaml entry

每個新 fixture 都要使用 setup checklist 的 template，在
`tests/fixtures/state-replay/MANIFEST.yaml` 加入 entry。需填寫的 field：

- `file`：filename（例如 `claude-yes-proceed.raw`）
- `backend`：backend identifier（`claude-code`、`codex`、`kiro-cli`、`opencode`、`agy`、`grok`）
- `cli_version`：`<backend> --version` 的確切 version string
- `recorded_on`：今天的日期，格式為 YYYY-MM-DD
- `scenario`：一句話描述擷取內容
- `expected_transitions`：起初保留為 `[starting]`；replay test 後再填寫
- `capture_kind`：`real_pty`
- `provenance`：參照 batch capture session 與 issue number

### 4. 以 replay harness 驗證

```bash
cargo test --test state_replay -- --nocapture
```

若 fixture 不符合 expected transition，請更新 MANIFEST entry——fixture 才是
ground truth，不是 expectation。

### 5. PR 形式

- Branch：`fixtures/<batch-description>`
- 檔案：只含 `.raw` fixture + MANIFEST.yaml update（不變更 src）
- Title：`fixtures: batch capture for #<issue> (<backend list>)`
- Cross-ref：連結此 batch 會解除阻擋的 issue

---

## 時間估計

| 操作方法 | 時間 |
|----------|------|
| R1. claude-yes-proceed | 約 3 分鐘 |
| R2. claude-trust-prompt | 約 2 分鐘 |
| R3-R14. productive marker（6 個 backend × 2） | 約 60 分鐘 |
| 擷取後清理 + MANIFEST | 約 15 分鐘 |
| **Session 總計** | **約 80 分鐘** |

提示：依 backend 批次執行 productive capture，將 context switching 降至最低。
先完成所有 Claude capture，再做所有 Codex capture，依此類推。

---

## §F685-CORPUS.6——Corpus growth protocol 與 open question

### Growth protocol

Corpus 會在數週內以 **incident-driven** 方式成長：

1. Operator 在 production 遇到 stuck-in-thinking 或 false-Hung incident。
2. 透過 §F685-CORPUS.5 workflow capture（或重現）PTY trace。
3. Operator（或後續 sub-task）加入帶有 measurement label 的 manifest entry。
4. Harness 在下一輪 CI cycle 重新執行；aggregate FP/FN report 隨之更新。
5. N 達到統計最低數量，**且** rate 低於 threshold 時，promotion gate（F9
   default-active flip 或 F39 mitigation choice）解除阻擋。

每個新 fixture 都是 `#685` 的**後續 sub-task**（除非一併包含 code 或 harness
change，否則不是獨立 PR）。

### Open question

- **Time-injection harness extension：** F39 Scenario C measurement 要求在 byte
  chunk 間推進 wall clock（位於 `rg "min_hold" src/state/mod.rs` 的 priority
  `min_hold` gate 使用 `Instant::now()`）。Replay loop 以 microsecond 執行；即使是
  30 秒的 real trace 也會瞬間 replay，因此 `since.elapsed()` 永遠不會跨過
  `min_hold`。需要：`.raw` companion 中的 per-chunk timestamp metadata，以及依
  chunk 回溯 `since` 的 harness。這不在 Phase 1 範圍內；F39 mitigation selection
  受它阻擋。
- **Real Scenario C capture：** 尚未取得。Operator 遇到 oscillation 時應以 script
  capture session 並貢獻 real fixture。在此之前，可接受 synthetic-from-real-template
  trace（以 operator incident report 為基礎、忠於 timeline 的 byte sequence）。
- **Per-backend marker calibration：** Deliverable #4（sub-task 6，決策
  `d-20260514022917793418-0`）交付 backend-specific marker cache，後來由 Gemini
  改名為 Agy。Grok 目前使用 generic cache；current listing 見
  `docs/HUNG-STATE-TRANSITIONS.zh-TW.md §F9.2`。Codex 與 OpenCode marker 在透過此
  growth protocol 取得 real PTY capture 前仍是 **synthetic-only**；擷取後其 fixture
  會加入同一 harness loop。
- **重新檢視 Cargo feature gate：** Phase 1 交付 always-on harness（fixture 沒有
  measurement label 時為零成本）。若 corpus 成長到約 100 個 fixture 以上，且
  aggregate replay time 接近 CI budget，請重新考慮以
  `cargo test --features f9-measure` gate harness。

## §F685-CORPUS.7——交叉參照與範圍界線

- S2 memo capture protocol：`/tmp/dialectic-996-s2-signatures-dev.md` 2.1–2.4 節
- MANIFEST.yaml recording protocol：`tests/fixtures/state-replay/MANIFEST.yaml` 的 header comment
- Fixture corpus measurement：`tests/fixture_corpus_measurement.rs`
- 既有 real-PTY fixture：`codex-update.raw`（2026-04-20）、`kiro-tooluse.raw`（2026-04-20）、`agy-thinking.raw`（2026-05-20）
- `docs/HUNG-STATE-TRANSITIONS.md §F39.5` 指向此處的 fixture-corpus capture criteria。
- `docs/HUNG-STATE-TRANSITIONS.zh-TW.md §F9.5` 指向此處的 promotion-measurement 方法。
- `src/state/tests.rs::corpus_measurement_smoke_f9_marker_signals`——F9 marker measurement 的 unit-test smoke harness。
- `tests/common/env_gate.rs::with_f9_gate`——F9 env-var serialisation 的 integration-test helper。它在 `src/health.rs::tests::with_f9_gate` 中的 mirror 必須維持 lockstep。

### 不在範圍內

- F39 mitigation (a)/(b)/(c)/(d)/(e)/(f) selection——需要先擴充 corpus 與
  time-injection harness。
- F9 promotion flip——需要先擴充 corpus 並做 active-mode measurement。
- Per-backend tuning（deliverable #4）——獨立的 sub-task。
- 超出目前 Stage-1-only dispatcher 的 recovery automation——需要新的 scope
  decision 與新證據；已移除的 Stage 2/3 並非 live plan。
- `schema_version` enforcement 的 schema migration code——Phase 1 只有 metadata。
- `cargo test --features f9-measure` gating——延後到 N 約為 100 時再處理。
