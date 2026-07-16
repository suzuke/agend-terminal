# Historical Operator PTY-Capture Queue

> **Status: completed historical queue.** The three tracked items (#1014,
> #1054, and #1559) were closed on 2026-06-01. Do not treat the checklist below
> as current work. For the live corpus status and new gaps (including Grok), use
> [`F685-FIXTURE-CORPUS.md`](F685-FIXTURE-CORPUS.md) and
> [`CAPTURE-RECIPES.md`](../tests/fixtures/state-replay/CAPTURE-RECIPES.md).

> 給 operator 的待錄製清單 + 操作指南。
> Agent 無法 spawn 真 interactive backend(無真 PTY allocator),所有 `.raw` fixture 必須由 operator 在真實終端錄製。
> 本文件把 2026-05-31 #1546 capture 過程學到的踩雷全部收錄,讓未來任何人照做都順。

---

## ⚠ 通用前提(沒做到就錄不到)

1. **必須「非 bypass」模式**
   permission/trust 框只在 default 權限模式跳。**不要加 `--dangerously-skip-permissions`**;視窗底部若顯示 `⏵⏵ bypass permissions on` 就按 **shift+tab** 切回 default。
   (fleet agent 全跑 bypass,所以它們自己抓不到——這是要 operator 手動錄的根本原因。)

2. **allowlist 會自動放行 → 不跳框**
   `~/.claude/settings.json` 的 `permissions.allow` 若含 `Bash(*)` 等,該操作直接放行不跳。要嘛換不在 allowlist 的操作,要嘛在乾淨 dir。
   (實測:`ls` 之類唯讀命令也常被自動放行 → 要用**有副作用**的命令/工具觸發。)

3. **`script` 語法(macOS BSD)**:`script -q <輸出檔> <命令>`。

4. **結束錄影:Ctrl-C 會被 modal 攔截**
   框跳出來時 Ctrl-C 通常沒用。兩個出路:
   - 框上**按 Esc 或選一個選項**關掉 → 回正常列再 `/exit`。(框 bytes 已在錄影**中段**,可用;只是 final-frame 是關框後。)
   - 要**乾淨的 final=permission**:**另開一個終端** `pkill -f <你的raw檔名>` 砍掉 script,讓錄影停在「框還在」那刻。

5. **驗證:別用 `cat`、別 grep 完整片語**
   TUI 用 `\x1b[<欄>G` cursor 定位把文字切片 + 用 alt-screen → `cat` 跑完看不到、grep 完整片語也漏。正確驗法:
   ```bash
   wc -c <raw>                              # size 有長大
   xxd <raw> | grep -c '1b5b'               # ANSI escape 數 >0
   grep -ao <單字> <raw>                     # 單字(不是完整片語)才連續、抓得到
   ```
   權威驗證是 **vterm replay**(測試 harness 重建螢幕 grid),不是人眼。

6. **錄完放這裡 + 加 MANIFEST**
   ```bash
   cp <raw> <repo>/tests/fixtures/state-replay/<backend>-<scenario>.raw
   ```
   MANIFEST.yaml 加一條(`expected_transitions` 含目標 state、`provenance` 寫 issue-ref);格式見既有條目 + CAPTURE-RECIPES.md。

---

## 待錄清單

### 1. #1054 — Claude 'Yes, proceed' modal cursor

**目的**:驗證 ClaudeCode `Yes, proceed` 確認框的**預設游標位置/caret**(backend.rs 有一段 keystroke 待此 fixture 驗證)。

**觸發 + 錄製**:
```bash
export CAP=$(mktemp -d /tmp/cap-XXXXX); cd "$CAP"; git init
script -q "$CAP/claude-yes-proceed.raw" claude          # 非 bypass
# 觸發一個會跳 "Yes, proceed" 的確認(例:有 update 時的 update-now、或某需確認的動作)
# 讓框 render 2-3 秒、別選
# Esc/選項關掉 → /exit;或另終端 pkill -f claude-yes-proceed
```
**看**:`grep -ao "Yes, proceed\|proceed\|❯" claude-yes-proceed.raw` — 確認 `Yes, proceed` 在、`❯` selector 在哪個選項(預設游標位置)。

---

### 2. #1559 — 各 backend permission prompt(content-FP sibling of #1546)

**目的**:#1546 修了 Claude 的 permission content-FP(改 chrome-footer 錨)。codex/kiro/gemini/opencode 的 permission pattern 也用 FP-prone 裸字串(`approve`/`deny`/`Allow this action`/`suggest changes` 等),同病。要修它們需各自 fixture 知道**各 backend 的 footer/chrome**。

**通則**:每個 backend 開**非 bypass** session、觸發它的 permission 框、錄起來、看 footer 長相。
參考已知(#1546 階段)footer:
- Claude tool-perm:`Esc to cancel · Tab to amend`
- Claude trust:`Enter to confirm · Esc to cancel`
- Claude edit option:`allow all edits during this session`

**各 backend(觸發方式因 CLI 而異,擇一能跳框的)**:
```bash
# codex —— 已知 footer 含 "Press enter to confirm or esc to cancel" + 選項 "Yes, proceed"/"No, and tell Codex"
script -q "$CAP/codex-perm.raw" codex
#   叫它跑需授權的命令 → 框 → 錄 → 關

# kiro —— pattern 現用 "Allow this action" / "y/n/t"
script -q "$CAP/kiro-perm.raw" kiro
#   觸發需授權動作 → 框 → 錄

# gemini —— pattern 現用 "Permission required" / "Allow once|always"
script -q "$CAP/gemini-perm.raw" gemini

# opencode —— pattern 現用 "Allow once" / "Allow for this session" / "suggest changes"
script -q "$CAP/opencode-perm.raw" opencode
```
**看**(每個):
```bash
grep -aoE "Esc to cancel|Tab to amend|Enter to confirm|Press enter|Allow|approve|deny|y/n/t|❯|[123]\. " <raw> | sort -u
```
重點記錄**每個 backend 的 footer/chrome 字串** → 交給 dev 把該 backend pattern 改成 chrome 錨(砍裸字串)。

---

### 3. #1014 — 真實 PTY productive-marker corpus(5 backend × 2 scenario)

**目的**:state-detection 的驗證閘(#1523)。目前 corpus 只證最小訊號;要 5 backend × 2 scenario 完整矩陣。

**2 scenario**:
- `productive_marker_fire` — agent **正在產出/工作**(spinner、token counter、tool 執行中)的畫面。
- `productive_silence` — agent **idle/等待**(乾淨 prompt、無 spinner)的畫面。

**5 backend**:claude / codex / kiro / gemini / opencode。

**通則**(每 backend 各錄 2 段,或一段含兩階段):
```bash
script -q "$CAP/<backend>-productive-fire.raw" <backend>
#   給一個會讓它持續工作幾秒的請求(例:讀多檔/跑分析)→ 錄到 spinner/工作畫面 → 結束
script -q "$CAP/<backend>-productive-silence.raw" <backend>
#   開起來、不給任務、停在 idle prompt 幾秒 → 結束
```
**看**:各 `.raw` 確認有 ANSI + 對應狀態的 marker(fire 段有 spinner/token;silence 段乾淨)。
**MANIFEST**:`expected_final_state` 分別填 thinking/tool_use(fire)與 idle/ready(silence)。
詳細逐 backend recipe 見 `tests/fixtures/state-replay/CAPTURE-RECIPES.md`(R3–R12)。

---

## 錄完之後

把 `.raw` 放進 `tests/fixtures/state-replay/` + 加 MANIFEST 條目後,**通知 lead**。lead 會:
- 各 fixture 交對應 issue 的 impl(#1559 各 backend pattern / #1054 cursor / #1014 corpus 矩陣 + measurement gate)。
- 走 review + CI + merge,納入 regression。

> 備註:這份是 operator-action 佇列;通用逐 scenario recipe 在 CAPTURE-RECIPES.md。兩者搭配:本文件說「要錄什麼、為何、給誰」,CAPTURE-RECIPES 說「每個 scenario 的精確 recipe」。
