[English](FLEET-DEV-PROTOCOL.md)

> 英文版本為規範性版本；若本翻譯與英文版有任何不一致，應以英文版為準。

# 艦隊開發協定 v1.2.1（安全勘誤）

**狀態：** ACTIVE — 所有艦隊代理都必須遵循本協定。

## 協定結構與維護

本文件分為兩層：

- **規範層（§0–§13）** — 規則。包含你要在*當下正確行動*所需的一切。這裡不得出現衝刺編號、PR/議題參照、日期或事件重述。
- **附錄 A — 理由與事件紀錄** — 說明*為什麼*以及*何時*。依章節 ID 索引規則背後的實證事件、啟用歷程及動機。從事件提煉出的規則會附有 `↳ 緣由 A-§X` 指標；僅在質疑或修訂該規則時才沿著指標查閱。
- **附錄 B — 章節編號對照表** — 過往重新編號的考古紀錄。

**整併儀式（維護後設規則）。** 新規則會持續累積；舊規則很少退役。為保持規範層清晰易讀：

- 新規則 MUST 以*命令句*（要做什麼）納入規範層，任何事件敘述都應移至附錄 A — 絕不內嵌。
- 每新增約 10 條規則，或每次涉及協定的衝刺，都要進行一次整併：合併重疊規則、退役已被取代的規則（將沿革移至附錄 A），並確認規範層仍能一次讀完。
- 如果不讀附錄條目就無法理解某個規範章節，表示規則措辭不完整 — 修正措辭，不要依賴附錄。

**作業優先順序。** 在本協定內：

1. 常駐程式硬閘門與即時 MCP 輸入結構描述，界定執行中的系統能執行什麼。
2. 僅當具體、明確限定範圍的例外指明要放寬的規則並記錄授權時，該例外才凌駕於一般規則。
3. 規範性的 `MUST` / `NEVER` 規則凌駕於範例、理由、歷史註記及軟性慣例。
4. 如果範例與即時結構描述或其他規範性規則衝突，請停止並回報不一致；不得猜測、繞過或合併這兩套做法。

## §0. KISS 原則

每個 PR 都必須回答：**「這解決了什麼真實問題？」**以及**「刪除它會讓任何人受影響嗎？」**有證據顯示不存在具體失敗模式時，判定為 `KISS-VIOLATION — REJECTED`；僅當疑慮已被提出但無法證明時，才使用 `UNVERIFIED`（§3.3）。

## §1. 任務看板（唯一事實來源）

使用常駐程式的 `task` 工具，而非各代理的本機任務清單。

**主要生命週期**：`create`（`open`）→ `claim`（`claimed`）→ `in_progress` → `in_review` / `verified` → `done`。對已宣告的相依關係使用 `blocked`，僅在刻意放棄任務時使用 `cancelled`。

**規則**：
- 協調器建立所派發的任務；處理操作員直接請求的代理可以自行建立並認領任務
- 存在相依關係時，必須設定 `depends_on`
- `task({action:"done", id:"<task-id>", result:"<evidence-backed outcome>"})` 必須包含非空的 `result`

## §2. 決策面板

使用 `decision({action: "post", ...})` 固定範圍定義或事實基準的變更。
- `tags` 必須包含工作軌，以及可用且最具體的成品 ID（`task`、`issue` 或 `PR`）；PR 存在後，加入 PR 標籤
- 跨工作軌使用 `scope: fleet`；特定工作軌使用 `scope: project`
- `supersedes` 將更正連結至原始決策

## §3. 審查協定

### 3.1 實作前
協調器發布範圍決策並建立任務。

### 3.2 審查派發契約（3 部分）
1. **事實來源** — 設計文件或決策 ID
2. **範圍邊界** — 稽核 X，忽略 Y
3. **時效邊界** — 若在 {sha} 之後變更，即為過時

### 3.3 判定
`VERIFIED` / `REJECTED` / `UNVERIFIED` — **以判定詞作為報告開頭**（常駐程式的 §3.3 證據閘門會依據開頭詞元判斷）。

每份審查報告都必須包含：`scope_source`、`audit_mode`、`reviewed_head`、`commands`、`files`。

**證據區塊（#1666 Phase A — 由常駐程式強制執行）。** `VERIFIED` 或 `REJECTED` 判定 MUST 附帶 `### Evidence` 區塊來證明主張：
- `ran: <cmd> → <result>` — 實際執行的命令（例如 `cargo test` / `clippy` / `gh pr checks <PR#>` / `grep`）及其結果；和／或
- `cited: path:line — quote` — 支持發現事項的原始碼引文。

`UNVERIFIED` 重新定義為**「已聲稱但未證明」** — 無須證據的判定。當你提出無法透過執行或引用來證明的疑慮時使用（如此一來，閘門永遠不會迫使你捏造證據）。

常駐程式會在回報時對此進行 HARD 閘控：若 `VERIFIED`/`REJECTED` **沒有可辨識的證據詞元**，就會退回給審查者。標準形式是在 `### Evidence` 內加入結構化的 `ran:` / `cited:` 項目；命令詞元（`cargo`、`gh`、`clippy`、`grep`、`rg`）及 `path:line` 引文則是相容性備援。此閘門刻意設計得**寬鬆** — 它只檢查證據是否存在，不檢查語義上是否充分。依 §3.21，審查深度仍由負責人／審查者判斷。

**註解與文字敘述是主張，不是證據。** 程式碼註解、文件或 PR 本文中的每項事實陳述都是必須對照程式碼 VERIFY 的主張，本身絕非證據。可達性／範圍／「不可能發生」／「單一關卡」等主張，作者與審查者都必須根據原始碼中的實際防護條件與比對分支加以證明。`↳ 緣由 A-§3.3`

### 3.3.1 CI 驗證閘門（Sprint 61）
核准合併前，協調器／審查者 MUST 獨立驗證 CI：

```
gh pr checks <PR#>
```

**硬性規則：**
- 核准合併前，必須取得結束碼 0（所有檢查均通過）
- 不得僅依賴開發者自行回報的 CI 狀態或 ci_watch 通知
- 不得依賴部分檢查結果（例如只有 LOC 超限檢查通過）
- 若任何檢查為 `pending`，請等待並重新檢查
- 若任何檢查為 `fail`，阻止合併並回報實作者

**不穩定測試宣告需要 CI 紀錄證據。** 將 CI 失敗稱為「不穩定測試」（以便重新執行而非修正）MUST 引用發生失敗之執行紀錄中的真實失敗測試名稱：

```
gh run view <run-id> --log-failed
```

- 所引用的測試必須符合已知的不穩定測試特徵（時序／IO／順序），不能只是被貼上不穩定標籤的確定性失敗。
- NEVER 根據本機或工作樹中的 `cargo`/`nextest` 執行結果推斷不穩定性。本機綠燈 ≠ CI 綠燈（平台、時序、平行處理、環境皆不同）；本機通過並不會證明 CI 失敗是非確定性的。
- 沒有紀錄證據 → 將失敗視為 REAL 並加以修正（一概「重新執行 + 標記不穩定」會掩蓋確定性錯誤）。

`↳ 緣由 A-§3.3.1`

### 3.4 再次審查（r1+）派發
每次再次審查的派發都必須列舉上一輪的所有發現事項及其狀態：已修正／已延後／已撤回。對照關係缺漏或不完整 → 審查者重做完整的原始範圍（`full_review`），而非僅審查聲稱已修正的部分。

### 3.5 多位審查者
- 預設：單一主要審查者
- 僅在以下情況使用雙審查者：高風險共用行為、反覆遭拒迴圈、主要審查者要求、操作員強制要求
- **對抗式審查**對應至 `review_class=dual`，並額外要求至少一位審查者挑戰權限邊界、無聲失敗路徑或執行期狀態不變條件，而非只檢閱正常路徑
- 判定嚴重程度：`REJECTED > UNVERIFIED > VERIFIED` — 以最差者為準

**合併權限矩陣：**
- 協調器合併：符合任務的 `review_class`（`single` 或 `dual`），並通過 CI 與判定鏡像閘門。
- 作者／實作者自行合併：雙重 VERIFIED，並通過 CI 與判定鏡像。
- 操作員僅文件自行合併：適用明確的 §3.6 例外；該例外不會放寬 CI。

### 3.6 LOW 僅文件例外
若要由單一審查者或操作員自行合併，必須符合所有條件：
1. 僅限艦隊協定／審查者契約文件、相符的協定迴歸測試，或 `src/instructions.rs` 中的艦隊協定範本字串
2. 差異 ≤ 50 LOC
3. 除這些範本字串外，執行期行為沒有變更
4. 沒有新增或實質放寬會影響一般／高風險工作的規則

### 3.7 跨後端主張
必須具有各後端的測試證據，否則應標記為 `unverified cross-backend claim` 並建立待辦任務。

### 3.8 跨團隊授權鏈
跨團隊借調審查者或委派任務時，必須引用操作員授權鏈（例如操作員訊息 ID）。新代理在沒有明確授權時，不得假定具有跨團隊存取權。

### 3.9 外部測試資料驗證
三類 PR 需要外部測試資料：
1. **線路格式** — 正式環境擷取資料／RFC 測試資料／跨實作參照
2. **並行狀態** — 多執行緒測試框架／loom／壓力迴圈
3. **持久化重播** — 寫入 → 重新啟動 → 還原往返流程

額外要求：線路格式不變條件測試（固定形狀）；與正式環境路徑耦合（不得用輔助函式模擬）。

**透過真實進入點進行測試（整合測試）；不要在管線中途注入輸入。** 手動將輔助函式的輸入餵給測試（例如直接將 `prs` 傳給分類器）會略過 — 因而隱藏 — 正式環境中產生該輸入的探索／接線路徑。從正式環境呼叫者所使用的真實進入點（掃描器／處理常式／派發器）驅動測試，讓探索或接線缺口使測試失敗，而非遭到無聲繞過。證據：#1799 PR-3 的單元測試將 `prs` 直接注入輔助函式，掩蓋了探索受限於 pr-state 種子的問題；codex 要求透過真實掃描器進行整合測試以揭露此問題。**審查檢查清單** — 審查者 MUST 詢問：*「這項測試有沒有演練真實進入點，還是在管線中途注入？」* 在與探索／接線耦合的路徑中，管線中途注入會造成未驗證的涵蓋範圍缺口 → 要求真實進入點整合測試。

### 3.10 測試優先
功能／修正 PR 必須採用測試優先：失敗測試的提交必須早於實作提交。
- 每個修正 PR MUST 包含實證重現測試案例。審查者 MUST 驗證此測試存在且有效。
- 審查者在由常駐程式管理、分別從測試提交及實作提交具現化的具名工作樹中驗證 RED 與 GREEN；絕不可就地分離或簽出 SHA（§3.19.1）。
- 豁免項目：僅文件、純重構、僅測試、相依套件升級、EMERGENCY、純刪除、實證還原

### 3.11 延後防禦措施
- (a) 已知議題在正式環境再次發生 → 自動升級至 P0
- (b) 延後的待辦事項必須有 `due_at`（預設：2 個衝刺）
- (c) 同一根本原因延後兩次 → 強制雙審查者 + 操作員核准
- (d) 移除防禦性程式碼 → 從 4 個觀點提出反例挑戰；0 個有力反例 = 可安全刪除

### 3.12 判定外部化（原 §3.5.13）
Fleet 內部判定 MUST 透過作用中的 SCM 提供者同步至 PR（GitHub 上使用 `gh pr comment`）。套用 §3.5 的合併權限矩陣：作者／實作者自行合併需要雙重 VERIFIED；協調器合併則使用該任務的審查類別；§3.6 僅文件的操作員例外仍須明確列出。CI 綠燈 + 必要的判定同步永遠是必要條件。

**標準合併步驟：`repo({action:"merge", pr:<N>, repository:"<owner/repo>"})`**（MCP `repo` 工具 → `handle_merge_repo`，`src/mcp/handlers/ci/mod.rs`）。它發出的合併與原始 `gh` 呼叫在位元組層級**完全相同**（`gh pr merge <N> --repo R --admin --squash --delete-branch`，由 `scm::tests::pr_merge_args_match_existing_gh_call` 鎖定），但包覆了原始命令欠缺的三層安全網：
1. **安全的儲存庫解析（#1619）** — 透過 `resolve_repo_or_error` 解析目標 `owner/repo`；偵測失敗時會報錯，而不是默默對硬編碼／維護者的儲存庫執行合併。
2. **CI 失敗即關閉閘門** — 先執行 `pr checks`（透過 `ScmProvider`）；任何非 `SUCCESS`/`SKIPPED` 的檢查，或任何無法判定的結果，都會拒絕合併。僅能用 `force=true` + 非空的 `force_reason` 繞過（稽核記錄於 `fleet_events.jsonl`）。
3. **`verify_merge_landed`（#1467）** — `gh pr merge` 以 0 結束是必要條件，但並不充分（合併佇列／最終一致性可能在尚未實際合併時就以 0 結束）；它會再次 `view` PR，並回報 `merged:false, pending:true`，而非誤報成功，讓呼叫端重新查詢，而不是盲目再次合併。

它也會透過 `ScmProvider` 路由（平台無關 — 並未硬接至 `gh`）。

⚠ **閘門的範圍：** `repo action=merge` 會對 **CI 失敗即關閉**設閘，而不是對審查判定設閘。上述雙重 VERIFIED 要求仍是由協調器強制執行的 **fleet 慣例**（派發審查者 → 等候 `VERIFIED` → 接著執行 `repo action=merge`）— 此變更並不會讓審查成為合併原語的硬性前置條件。

**備援（緊急情況／MCP 無法使用）：** 原始 `gh pr merge <N>` — `--auto --squash --delete-branch` 形式（§3.12.1，伺服器端佇列；需要嚴格的分支保護）或同步的 `--admin --squash --delete-branch` 形式（佇列競爭復原／管理員繞過，依 #985/#988 的偏離先例）。優先使用 MCP 原語；僅在 MCP 路徑無法執行時才降級使用原始 `gh`。

#### 3.12.1 採用 `gh pr merge --auto`（Sprint 65，#973）— 自 2026-05-20 起 ACTIVE

> 注意（t-protocol-merge-via-repo-action）：§3.12 現在將 **`repo action=merge`** 定為標準合併步驟。本小節規範**原始 `gh` 備援**路徑 — 當你不得不降至 MCP 原語以下時，`--auto` 是偏好的原始形式（它遵守分支保護；此路徑無法使用常駐程式的 CI 失敗即關閉閘門）。

降級至原始 `gh` 備援時，偏好的呼叫方式是 `gh pr merge <N> --auto --squash --delete-branch`（需要 `gh` CLI >= 2.31.0）。`--auto` 會將合併提交移至 GitHub 的伺服器端佇列，消除 #971 閉環中觀察到的「基礎分支已修改」競爭（2026-05-20）。

**前置條件**（每個儲存庫一次，已於 #973 啟用）：
- 儲存庫層級的 `allow_auto_merge: true`（`gh api repos/<owner>/<repo> -X PATCH -F allow_auto_merge=true`）
- `main` 上的分支保護，且 `required_status_checks.strict=true` 涵蓋完整 CI 矩陣（`Check (ubuntu-latest|macos-latest|windows-latest)`、`LOC overrun`、`audit`）。若沒有 `strict=true`，在所有檢查都已回報後呼叫的 `--auto` 會立即合併 — 默默略過 §3.12 的合取閘門。使用 `strict=true` 時，GitHub 會在合併前依目前的 main 重新檢查。
- 管理員權限注意事項：啟用這些設定需要儲存庫管理員權限。受委派的維護者應透過操作員提出要求。

**行為**：`gh pr merge --auto` 會立即返回（不會因 CI 而封鎖）。當保護閘門的合取條件成立時，合併會非同步觸發。作者 MUST NOT 手動輪詢；`[pr-merged]` 事件（由常駐程式 PR 狀態彙整器 #972 + gh-poll 觀察 #986 傳遞）是閉環確認來源。

**逃生口 — 停滯的 `--auto`**：若在 CI 綠燈 + 已張貼判定同步後 30 分鐘內仍未收到 `[pr-merged]`，可能原因包括：
1. 必要檢查從未回報（CI 基礎設施問題）
2. 分支保護設定錯誤（狀態檢查內容名稱漂移）
3. 啟用 `--auto` 的代理程式發生權杖／權限問題

依序復原：
- (a) 驗證保護狀態：`gh api repos/<owner>/<repo>/branches/main/protection --jq '.required_status_checks'`
- (b) 驗證 PR 可合併：`gh pr view <N> --json mergeable,statusCheckRollup`
- (c) 重新啟用：`gh pr merge <N> --auto --squash --delete-branch`（冪等 — 已啟用時重新啟用不會執行任何操作）
- (d) 最後手段 — 手動備援：`gh pr merge <N> --squash --delete-branch`（同步；可能遇到基礎分支已修改的競爭；若發生，3 秒後重試）
- 若使用逃生口，請透過 `send({instance:"<lead>", request_kind:"update", message:"<case (a)/(b)/(c)/(d)>"})` 通知負責人。

`↳ 緣由 / 活化史 A-§3.12.1`

### 3.13 日誌層級變更（原 §3.5.14）
必須附有行內理由，否則為 `LEVEL-CHANGE-RATIONALE-ABSENT — UNVERIFIED`。

### 3.14 可觀測性 PR（原 §3.5.15）
必須包含會實際執行正式環境掛鉤路徑的 e2e 整合測試。

### 3.15 常駐程式核心緩衝規則
觸及常駐程式核心／通道／監督器／state.rs 的 PR，在派發前必須包含壓力測試 + 鎖定順序分析。「不急 ship」原則 — 基礎設施變更以正確性優先於速度。

### 3.16 Fleet／最高儀式性討論紀律
當 §3.21 選擇 FLEET 或高風險覆寫時，本節適用。在此路徑中，實作前的原始碼探查為強制要求。負責人的初始提案 MUST 在第 2 階段派發前，接受開發者 5-10 分鐘原始碼探查的質疑。探查輸出：
- 確認或反駁負責人最初估計的站點數
- 揭露負責人遺漏的額外發送站點
- 區分「接近 bug」與「針對 bug 特徵進行斷言」（議題內文的計數經常混為一談）
- 找出會改變範圍估算的既有輔助函式／相依套件

**需要三方實質共識**：在記錄共識前，審查者必須提出至少一項設計質疑，且開發者必須提出至少一項實作疑慮。缺乏實質內容、僅有證據形式的三方 ACK 屬於 `RUBBER-STAMP — REJECTED`；只有在無法證明該疑慮時才使用 `UNVERIFIED`。

**議題內文中的數量是估算，不是合約。** 當議題內文寫著「N 個站點／N 個測試需要更新」時，開發者探查會重新計數。實際範圍可能比初始估算更窄或更廣。

`↳ 緣由 A-§3.16`

### 3.17 靜態審查限制 + 需要執行期驗證
對於下列表面，靜態／結構性審查並不充分：

- **CI 工作流程 YAML**（快取層互動、執行期 PATH／環境）
- **Shell 指令碼**（變數插值、與地區設定相關的行為）
- **常駐程式重新整理／生命週期行為**（記憶體內狀態與持久化狀態的分歧）
- **跨平台二進位檔語意**（例如 rustup-init `--version` 對代理路徑上的任何二進位檔都以 0 結束）

對於這些表面，最終 `VERIFIED` 判定需要執行期證據 — 通常是該 PR 本身在多個平台上的 CI 執行。審查者可以立即開始靜態審查（§12.2），但在執行期證據存在前，必須暫不給出最終 VERIFIED。僅檢查程式碼差異並不足夠。審查者必須在判定報告中明確註明「已透過 PR-CI 執行 X 完成執行期驗證」。若該 PR 本身的 CI 未實際執行受影響的路徑，請求提供實證重現步驟。

**可泛化的不變量**：結束代碼 0 並不是工具檢查的強式身分合約。輸出格式才是。`<tool> --version | grep -qE "^<tool> [0-9]"` 是正確的內容驗證慣用法。

### 3.18 審查者稽核衝突解決
當審查者的主張與開發者的主張矛盾時（例如審查者稱「仍有過時措辭」，而開發者稱「措辭已更新」），負責人 MUST 在接受任一方之前進行**獨立驗證**：
- 在確切的 reviewed_head SHA 上執行 `git show <SHA>:<file>`
- 對有爭議的行執行 `git diff <prev>..<reviewed_head>`
- 獨立執行相關測試或 grep 命令

負責人以實證回覆雙方。審查者／開發者應自行更正，而不是上報操作員。

### 3.19 審查者工作區紀律
審查者 MUST 在不變更標準來源儲存庫的情況下檢查 PR。透過提供者進行唯讀檢查不需要簽出；任何完整樹狀結構檢查 MUST 使用審查者自己綁定常駐程式的工作樹。具體而言：

- **絕不 `cd` 進標準來源儲存庫**以檢查 PR。標準來源是操作員的工作樹；審查者活動不得在其中留下分離式 HEAD 或過時參照。
- **Never 在標準來源中建立參照**（`git checkout -b tmp_pr_review`、`git checkout <sha>`、`git fetch origin pr/N/head:pr_head` 等）。這些操作會留下 `pr*_head`／`tmp*`／`review/*` 分支，污染 `git branch --list` 並使後續操作員命令混淆。
- **使用 `gh pr diff <N>` 或 `gh pr view <N> --json files`**，在不簽出的情況下讀取 PR 內容。若需要完整樹狀結構檢查，`repo({action:"checkout", repository_path:"<canonical>", branch:"<new-review-branch>", from_ref:"<full-PR-head>", expected_head:"<full-PR-head>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review"})` 會佈建由常駐程式管理、具確切一次性審查來源資訊的具名工作樹；`release_worktree({instance:"<self>"})` 可在不觸碰標準來源的情況下歸還該工作樹。
- **若審查後觀察到標準來源狀態髒污**（分離式 HEAD、過時的 `tmp*`／`pr*_head` 分支），請暫停審查並回報操作阻礙。不要將無關的工作區衛生問題轉化為 PR 判定。操作員清理會先以試執行方式執行 `repo({action:"cleanup_merged_branches", base:"main"})`，再附上稽核理由套用選定的候選項目 ID。

強制執行：當 cwd=canonical 時，L2 `agend-git` 墊片會拒絕代理程式呼叫者執行 `checkout -b` 與 `checkout <sha>`（PR-B）。L3 清掃器會清除殘留物，並在常駐程式啟動時自動將處於分離狀態的標準來源 HEAD 切回 main（PR-C）。

### 3.19.1 代理程式 Git 反模式

§3.19 說明審查者不得做什麼。本節說明兩種失敗模式與正確的復原路徑。適用於每個代理程式，不只審查者。`↳ 緣由 A-§3.19.1`

**反模式 1 — 使用 `AGEND_GIT_BYPASS=1` 逃避墊片拒絕。**

當作用中的 git 防護拒絕代理程式操作時，該拒絕是協定訊號，不是暫時性錯誤。禁止設定 `AGEND_GIT_BYPASS=1` 後重新執行相同命令。

- **WRONG**：墊片拒絕 → 設定 `AGEND_GIT_BYPASS=1` → 重試。繞過操作在 git 層級會成功，卻略過該拒絕原本要強制執行的協定閘門；無論該閘門保護的是什麼（標準來源衛生、租約不變量、審查者工作區邊界），現在都會被默默違反。
- **RIGHT**：中止操作。傳送 `send({instance:"<lead>", request_kind:"query", message:"<denied command + reason>"})` 並詢問正確的路由方式。

理由：

- 為了向下相容而保留的 `AGEND_GIT_BYPASS=1` 輸入，是供**常駐程式內部輔助程式**（`canonical_hygiene`、`branch_sweep`、`conflict_notify`）使用；這些程式會從以標準來源為根的路徑讀取工作樹狀態，否則會自行拒絕。它不是代理程式的逃生口。
- 繞過通常會在原有問題之上顯露隱藏狀態。
- 「詢問，不要繞過」是通用的復原方式：拒絕表示常駐程式掌握路由答案，而詢問的成本很低。

**反模式 2 — 使用 `git checkout <sha>` 具現化 PR 審查。**

即使在代理程式自己綁定常駐程式的工作樹中，`git checkout <sha>` 仍是 PR 審查的錯誤原語：

- 會留下分離式 HEAD 殘留物 — 正是 #852（標準來源衛生）與 #858（墊片拒絕矩陣）要防止的污染類型。
- 與常駐程式對工作樹的分支租約衝突，產生後續看似無關的「分支已被租用」錯誤。
- 即使從非標準來源的 cwd 執行，也會繞過 §3.19 由墊片強制執行的工作區邊界，因為墊片的租約／生命週期不變量假設 HEAD 以分支為根。

依檢查深度選擇正確路徑：

- **完整樹狀結構**（重播 `cargo test`、執行期驗證、多檔案檢查）：`repo({action:"checkout", repository_path:"<canonical>", branch:"<new-review-branch>", from_ref:"<full-PR-head>", expected_head:"<full-PR-head>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review"})`。常駐程式要求該分支經證明在本機與 `origin` 上皆為全新，會在初始簽署綁定中記錄確切的佈建 head，並在審查任務進入終止狀態後允許受防護的清理。`release_worktree({instance:"<self>"})` 會乾淨歸還且不留殘留物；髒污／分歧／模糊狀態會以失敗即關閉方式保留。
- **唯讀**（差異檢查、檔案清單）：`gh pr diff <N>` 或 `gh pr view <N> --json files`。完全不變更工作樹。

若 `repo({action:"checkout", ...})` 失敗（租約已被持有、分支未知、工作樹配額耗盡）→ **詢問，不要繞過**。以 `request_kind:"query"` 訊息將失敗模式傳送給負責人；經授權的復原可使用 `release_worktree({instance:"<target>", force:true, branch:"<branch>"})` 或替代佈建方式。在 `repo` 失敗後退回使用 `git checkout <sha>`，會重現本節禁止的確切污染類型。

**與 §3.19 的關係。** §3.19 說明*審查者不得在標準來源中做什麼*。§3.19.1 說明*每個代理程式在協定閘門觸發時必須做什麼* — 中止並詢問，而不是繞過後重試。

### 3.19.2 審查者基礎工作區分支紀律

§3.19 涵蓋標準來源儲存庫。本節涵蓋審查者代理程式自己的基礎工作區目錄（例如 `$AGEND_HOME/workspace/fixup-reviewer/`）。

**審查者 MUST NOT** 在代理程式的基礎工作區目錄中就地 `git checkout` 實作分支。基礎工作區由常駐程式綁定至特定分支（通常是 `main` 或長期存續的審查維護分支）；就地簽出實作分支會以過時分支狀態污染基礎工作區，並滲入未來工作階段。

使用以下方式之一：
- **(a) 專用審查工作樹**：解析目標 PR 的完整 head SHA，接著以 `repo({action:"checkout", repository_path:"<canonical>", branch:"review/<N>-r0", from_ref:"<full-PR-head>", expected_head:"<full-PR-head>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review"})` 佈建由常駐程式管理的具名工作樹。審查分支在本機與遠端都必須是全新的。完成時使用 `release_worktree({instance:"<self>"})` 釋放。
- **(b) 僅使用 GH 審查**（差異專用檢查的偏好方式）：`gh pr diff <N>` + `gh pr view <N> --json files,reviews,statusCheckRollup`。無本機簽出，也不需要清理。

**NEVER** 在代理程式的基礎工作區目錄中就地 `git checkout` 實作分支。

`↳ 緣由 A-§3.19.2`

**與 §3.19 的關係。** §3.19 禁止在 CANONICAL 中簽出。§3.19.2 禁止在代理程式的 BASE WORKSPACE 中就地簽出。兩者在不同邊界防止過時分支污染。

### 3.19.3 原始檔查找 — 不得掃描整個磁碟

禁止使用全磁碟 `find / -name …` 或 `find ~ -name …` 來定位原始檔（例如 `vendor/agentic-git/` 下作用中的防護原始碼）。若在整個 fleet 中同時執行，會使全機負載飆升（#2386：16 核心機器上的負載達 108）。

從**固定點**尋找來源，而不是從檔案系統根目錄：
- 在你綁定的工作樹／儲存庫內：`git ls-files | rg <name>` 或 `rg --files | rg <name>`（限定於索引範圍，速度快）。
- 需要儲存庫根目錄：`git rev-parse --show-toplevel` — 絕不使用 `find /` 尋找標記檔案。
- 不知道路徑：讀取 `binding_state({instance:"<self>"})` 取得你的工作樹路徑，或透過 `request_kind:"query"` 詢問負責人。絕不掃描整個磁碟。

不要在共享成品中硬編碼特定機器的絕對路徑（此協定跨機器使用）；請透過 `git` 解析。

`↳ 緣由 A-§3.19.3`

### 3.20 競態條件 PR 紀律

競態類 PR 帶有隱藏的時序相依性，能通過 CI + 審查者 VERIFIED，卻在生產環境中失效。以下經驗適用於每一個 spawn / 非同步協調 / 多程序啟動 PR（「競態類」）；紀律框架與 §3.19.1 相同。`↳ 緣由 A-§3.20`

**SOP 1 — r0 前競態條件問題。**

在競態類 PR 上派送 r0 前，lead 與 dev MUST 以書面回答：*"這項變更是否存在競態條件，而且我能否撰寫不依賴時序、可確定性重現它的測試？"* 答案應放入探查報告（若先前沒有探查，則放入派送訊息）。

競態類包括——但不限於——`tokio::spawn` / `thread::spawn` 位置、多程序啟動順序、`Drop`-對-`enqueue` 生命週期、跨模組的鎖定順序、訊號處理器對主迴圈協調、daemon 對 bridge 握手閘門。如果答案是「無法進行確定性測試」，請在實作前停止，並記錄豁免決定，其中包含嘗試過的確定性設計、替代實證、operator 授權，以及強制執行的 SOP 2 冒煙測試。沒有該豁免，§3.10 和 SOP 1 將阻擋合併。

**SOP 2 — 合併後 operator 冒煙健全性檢查（不是合併閘門）。**

競態類 PR 在 SOP 1（確定性 RED→GREEN 測試）與 SOP 3（審查者 RED 協定）均已滿足後即可合併，除非上述明確的無法進行確定性測試豁免，以其記錄的替代實證取代兩者。SOP 2 是**合併後健全性檢查**，不是一般的合併前閘門；在豁免情況下，它會成為合併後立即強制執行的項目。

**合併後冒煙程序**：

- Operator（或代表 operator 的 lead）在**全新、隔離的 `$AGEND_HOME`** 上重現競態情境——例如 `/tmp/smoke` 或 `$TMPDIR/agend-smoke-$$`。**NEVER 使用 operator 日常使用的 `$AGEND_HOME`**（通常是 `~/.agend`，或舊版備援位置）；冒煙執行 MUST 是封閉且可拋棄的，讓迴歸無法洩漏至 operator 狀態。
- PR 內文 MAY 包含建議的冒煙腳本，列舉修正所針對的競態情境（例如「冷啟動 daemon + 監看收件匣是否在 5s 內出現 `bridge_connected`」）。這是選用項目，不是合併核准的必要條件。
- 如果合併後冒煙測試發現迴歸：由 operator 驅動還原（`git revert <merge-sha>`）——依 §3.11(a) 延後防禦，競態迴歸會自動升級為 P0。

**閘門層級（實際的合併閘門）**：

- **SOP 1**（單元／整合層級的確定性 RED→GREEN 測試）——結構性閘門。只要有適當的模擬或 DI，多數競態類行為 CAN 進行確定性測試；標準是「是否有一項測試，在修正前失敗、修正後通過，且連續執行三次皆是如此」。
- **SOP 3**（審查者在測試表面執行 RED 協定）——稽核閘門。審查者必須獨立觀察 RED→GREEN 轉換。
- SOP 2 合併後冒煙測試是補充性的實證涵蓋，不是閘門。

「無法進行確定性測試」很少見——通常可使用 `tokio::test` + 暫停時間、以 channel 為基礎的同步，或透過 trait 注入時鐘來建立確定性設計。豁免必須定義 SOP 3 要改為執行什麼（例如有界壓力測試工具加上生產入口追蹤），而且絕不能宣稱觀察到實際並未發生的 RED→GREEN。

**SOP 3 — 競態類 PR 的審查者 RED 協定。**

對於競態類 PR，審查者 MUST 執行 RED→GREEN 協定（不是略讀）：

1. 透過 `repo({action:"checkout", from_ref:"<full-pre-fix-SHA>", expected_head:"<full-pre-fix-SHA>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review", ...})`，將修正前的 commit 具現化為新分支上的 daemon 管理具名 worktree（例如 `review/<N>-red`）；絕不在原地 checkout 該 SHA。
2. 確認 RED：新測試無法編譯、執行階段失敗，或以預期的錯誤特徵失敗。
3. 釋放 RED worktree，接著在另一個 daemon 管理的具名 worktree／分支中檢查修正（例如 `review/<N>-green`）。
4. 連續執行三次，確認 GREEN 且沒有不穩定性。

裁決內文 MUST 明確說明兩個不可變 SHA、具名 worktree、命令、RED 特徵，以及 GREEN 3/3 結果。

在競態類 PR 上跳過此協定的審查者，會連同依 §3.3 提供的證據一起收到 `RUBBER-STAMP — REJECTED`。該 PR 會退回 dev，要求明確執行審查者 RED 協定後再重新派送。

**與 §3.19.1 的關係。** §3.19.1 說明*協定閘門觸發時，每個 agent 必須做什麼*。§3.20 說明在競態類 PR 上，*lead、dev 和 reviewer 必須在閘門可能觸發之前做什麼*——這是經認可的紀律補充，不會取代任何既有規則。在 r0 派送時對競態類進行分類，比 #881 實際觀察到的先發布再還原循環成本更低。

### 3.21 比例相稱的儀式——依任務風險適當調整流程

讓 fleet 儀式配合任務風險的實際所在。由 **lead 判斷**決定，而不是 daemon 分類器——沒人遵循的評量準則只是合規劇場（虛假信心、責任轉移）。#1656 將審查分級作為純判斷推出，而且確實抓到了真實缺陷。透過 `decision({action: "post", ...})` 記錄每次派送的儀式選擇——決策日誌就是分類器（零新增程式碼）。`↳ 緣由 #1656/#1659/#1660 dialectic`

**三個獨立軸向——分別決定，絕不壓縮成單一「瑣碎／非瑣碎」旗標。** 一項任務可以是單 agent + 輕度審查 + 必須探查（例如 #1658）。

- **A. Fleet 或單一。** FLEET 當且僅當*「做錯 = 代價高昂」*，且*「只有試圖破壞它的對抗者能抓到瑕疵——你能寫出的測試抓不到」*（#1654 權限繞過、#1635 規避形式、#1629 同層 deadlock）。否則為 SINGLE——小型／失敗時採安全預設／已驗證模式／作者可透過測試或實證執行自行驗證（#1625、#1657 的 diff）。Lead 的 5 秒問題：*「如果這在細微之處出錯——會有多糟，而且我自己的測試能抓到，還是只有攻擊者能抓到？」*
- **B. 探查或跳過。** 預設 = 探查（其價值在前提檢查，不是規模閘控）。只有在全部五項均成立時，才可跳過 → 直接進入 impl：1 單一具名修正位置（沒有「調查／哪裡」動詞）2 根本原因是結構性的而非行為性的——是你能看見的事實（重複／錯字／缺少分支），而不是「因為 <runtime behavior>」3 修正從你已閱讀的 consumer 即可不證自明 4 沒有任何由測試／lint 強制的建構被移動（#1642 無聲取消強制陷阱）5 沒有前提反轉風險——任何「已經存在／不存在」的假設，都已先閱讀程式碼並完成驗證（#1658 陷阱：假設閘門不存在，但其實有一個）。助記詞 **位置 · 結構 · 不證自明 · 固定不動 · 已驗證**。跳過是可逆的：如果 impl 顯示前提比原先認為更不穩固（位置向外擴散、無關測試損壞、grep 顯示更多呼叫位置），立即中止並改做探查。跳過檢查清單由原本會撰寫 impl 的同一個 agent 套用 → 防範帶有確認偏誤的「對，顯而易見」。
- **C. 審查層級（#1656）。** normal/single → dual → adversarial，依影響範圍決定。請參閱 §3.5。

**高風險覆寫（安全底線）。** 無論表面規模如何，以下任一項都會強制三個軸向採用最高強度儀式（fleet + 探查 + adversarial 審查）：權限／安全性表面 · 無聲失敗機制（錯誤的 key/glob、無作用的 config 欄位、核准／提示路由）· 無法以實證驗證的整合宣稱（「可在已安裝版本／schema／tool 上運作」）· invariant／forcing-function 變更 · 取決於 runtime 狀態而非 diff 大小的影響範圍。放行一個偽裝成低風險的高風險變更（已發布的權限繞過／無聲 deadlock），其代價遠超過在許多真正低風險變更上省下的儀式成本。

**兩項跨領域原則。**
- **讓儀式類型配合風險位置。** 當風險位於*診斷／RCA*（而非 diff）時，承重檢查是**實證驗證**（處置／對照執行），不是更多審查者——#1657 的風險是 schema key（由 A/B 執行抓到），不是 dual-review 仔細檢查的 1 行 diff。
- **非對稱偏向——對任何軸向不確定時，升級。** 誤判為需要儀式只花幾分鐘；誤判為不需要則會發布權限繞過／無聲 deadlock。預設：有疑慮時，採用更多儀式。

`↳ 緣由: 2026-06-02 4-agent dialectic (dev/dev-2/codex/reviewer-2), /tmp/ceremony-spike-*.md. Resolves #1659 + #1660 as policy (no code).`

### 3.22 探查優先規劃閘門

當 §3.21-B 選擇探查（前提風險），**或**工作包含 operator 決策分歧（只有 operator/lead 可以決定的選擇）時，探查與 impl MUST 是**分開的派送**——絕不合併為一項任務。

- **探查僅限分析**（不含生產程式碼）。它會交付一份**決策清單**：每項前提檢查都要附上程式碼證據，說明是已確認或已推翻，並針對每個 operator 決策分歧提出具體選項 + 建議。
- **只有在分歧解決後，才可派送 Impl**，且 `depends_on` 包含探查任務 ID。決策 ID 應放在實作任務的範圍來源／描述中，而不是任務相依性清單裡。Impl 範圍應從清單導出，而不是預先假設。
- **禁止批次核准。** 不得將探查 + impl 預先核准為一個單位：在探查解決前提與分歧之前，impl 的實際範圍仍未知，因此預先核准 impl 等同核准未知項目。

**僅作強化**（lead 判斷，如 §3.21）——在派送時執行，不是 daemon 硬閘門。可機械化的候選項目是 chokepoint=dispatch / signal="does this impl have a resolved decision-manifest?"，但依 KISS，在真正有理由建立閘門之前，這仍維持為慣例（限制容易誤踩的陷阱，而不是有能力的操作）。

`↳ 緣由 A-§3.22`

## §4. Daemon 強制閘門

### 4.1 推送時語意閘門（Sprint 44）
Daemon 驗證 dev 的推送宣稱是否符合實際 diff。可辨識的文法：
- `"no other changes"` / `"byte-equal verified"` / `"scope follows dispatch spec X"` / `"only formatting"` / `"deps unchanged"`

未知文法 → 硬性拒絕。不允許直接通過。

### 4.2 審查者 SHA 過期閘門（Sprint 44）
Daemon 在裁決時比較 `reviewed_head` 與 PR HEAD。不相符 → 拒絕裁決。Reviewer 必須執行 `git fetch` 並重新審查。失敗時關閉（fetch 失敗 = 拒絕）。

### 4.3 幻覺 fn 檢查（Sprint 44）
當推送宣稱引用函式名稱時，daemon 會透過 syn-lite + rg 備援驗證其存在。找不到 → 拒絕推送。

### 4.4 保留名稱警告（Sprint 46）
具有路由語意的 instance 名稱（`general`、`lead`、`dev`、`reviewer`）會在建立時發出警告。不是硬性拒絕。

### 4.5 跨團隊 ACK 吸收例外（Sprint 61，#612）
對於一次性 Codex 後端，當接收者不是 orchestrator，且訊息不是接收者已取出之 blocker 的相關回應時，同團隊的 `update` 和 `report` 訊息會持久保存到收件匣，而不喚醒接收者。跨團隊訊息、傳給 orchestrator 的訊息，以及相關的 blocker 回應，仍會注入 PTY。ACK 吸收會抑制不必要的喚醒；絕不會丟棄訊息。

## §5. 非同步管線

Impl 推送 PR 後立即開始下一項任務。Reviewer 發出裁決後立即接手下一項審查。dev-lead 維護待處理清單；在獲授權合併前，任務所需的 `review_class` 必須由 VERIFIED 裁決滿足，且 CI 必須為綠燈（透過 `gh pr checks` 獨立驗證）。

**關鍵規則**：
- Impl 推送必須包含範圍聲明（遵循規格／偏離是因為）
- Orchestrator 派送前驗證：在轉交 reviewer 前，對照實際產出物交叉檢查 dev 的宣稱
- dev-lead 可使用一次性的 `schedule({action: "create", ...})` 作為 30 分鐘確認備援；它不得變成重複輪詢迴圈
- **分支派送前的審查類別**：建立每一項會產生 PR 的分支任務時，使用 `task({action: "create", ..., review_class: "single" | "dual"})`。既有任務的 `metadata.review_class` 具有權威性；只在之後的 `send` 加入 `review_class` 無法修復未指定的任務，而且 daemon 會以失敗關閉。
- **派送後驗證（Sprint 62）**：使用 `send({instance: "<receiver>", request_kind: "task", task_id: "<task-id>", branch: "<branch>", message: "<brief>"})` 派送。成功的任務派送回應可確認已排入佇列／路由，但不會公開穩定的訊息層級收據。`delivery_mode` 是選用的路由中繼資料：一般 send 或備援路徑可能會公開它，而主要任務派送包裝器可能省略它。將其存在視為路由中繼資料，其缺席則視為正常。不會回傳訊息 ID，而且路由成功不代表接收者已閱讀、理解或確認任務。如果接收者在約 5 min 內沒有回覆，請結合 `list_instances({instance: "<receiver>"})` 的存活狀態、`pane_snapshot({instance: "<receiver>"})` 的可見活動、`binding_state({instance: "<receiver>"})` 的分支狀態，以及最終報告。這些訊號沒有任何一個能單獨證明任務已被理解。
  - 如果訊號顯示沒有進展，請先診斷 lease 或派送路徑，再重新派送。接收者或其獲授權的團隊 orchestrator 可以使用 `release_worktree({instance: "<receiver>"})`；強制釋放還需要已知分支：`release_worktree({instance: "<receiver>", force: true, branch: "<branch>"})`。
- **Pane 宣稱不等於交付**：agent 在自己的 pane 中撰寫回應，並不是 `send`。每個回覆／裁決／報告都必須透過 MCP `send` tool 觸發。接收者看不到 pane 內容。請透過 §6 channel 紀律驗證。
- **PR 合併後閉環報告**：每個 PR `request_kind: "report"` MUST 包含「經驗教訓」章節，說明流程優點、範圍變動、意外發現。擷取流程成熟度訊號，以供協定演進使用。
- 接管需要獨立驗證 4 項條件（heartbeat 過期 ≥1h、last_input 停滯、idle 狀態、零活動）
- Worktree 釋放與分支刪除是不同的狀態轉換：乾淨且已推送／移交的 worktree 可在合併前釋放，讓 agent 能接手另一項任務；daemon 會保留尚未合併的分支與清理意圖。只有在 §10 的保存證明完成後，才能刪除分支。待處理／已排入佇列的合併不是刪除證明。
- 合併後：orchestrator 在向上游回報任務完成前，驗證 main CI 為綠燈。Main CI 失敗 = 立即 P0（還原或 hotfix）。
- Orchestrator 對自行協調的分支負責 `ci({action: "watch", repository: "<owner/repo>", branch: "<branch>", task_id: "<task-id>"})`
- Agent 卡住逾時：請參閱 §9 逾時階梯

## §6. 通訊

所有 agent 間訊息傳遞都使用 `send`：

| `request_kind` | 用途 | 預期回覆？ |
|---|---|---|
| `task` | 委派 | 是 |
| `report` | 結果／裁決 | 視情況而定 |
| `update` | 僅供參考 | 否 |
| `query` | 問題 | 是 |

**路由**：`instance`（單一）或 `instances` / `team` / `tags`（廣播）

**派送里程碑更新**——對於會產生 PR 的實作工作，無須被要求，即應在下列每個里程碑向派送者傳送 `request_kind: "update"`：

1. **r0 就緒**——PR 已開啟（或工作產出物已移交），並附上逐字不變的連結／heads。
2. **CI 全綠**——該 PR 執行的每個 CI 閘門皆已回報成功。`[ci-pass]` watch 廣播不能取代此更新——請透過你自己的更新確認，讓派送者的閉環器無論其 channel 狀態如何都會觸發。
3. **已收到審查者裁決**——VERIFIED / REJECTED / UNVERIFIED，並附上審查者身分與關鍵發現摘要。

重新審查循環（r1、r2、……）會重複相同的三個里程碑。派送者依賴這些項目來完成閉環；缺少任何一項都會迫使他們輪詢，而那是反模式（請參閱 §7）。

對於不產生 PR 的分析、探查、審查或操作任務，回報要求的產出物／結果，並將 PR 特定里程碑標記為不適用；不要虛構 PR 生命週期。

- 純確認 → 不要回覆（ACK 吸收由 §4 自動處理）
- 回應 channel 必須與來源 channel 相符
- **回應 channel 紀律**：`[user:NAME via telegram]` → `reply`；`[from:AGENT_NAME]` → `send`；無前綴（operator 直接在 TUI 輸入）→ 直接文字。不要假設直接文字會普遍鏡像轉送。
- **收件匣與 PTY 交付（Sprint 62）**：訊息會持久排入佇列；符合資格的訊息也可能注入作用中的 PTY。空的待處理收件匣取出結果不是交付證明，因為訊息可能已被取出或注入。將派送結果、後續任務／報告狀態、`list_instances({instance: "<receiver>"})` 與 `pane_snapshot({instance: "<receiver>"})` 作為互補訊號；吸收例外請參閱 §4.5。
- **Daemon 自動注入標記 `[AGEND-AUTO]`（#1769）**：daemon 會透過直接向 PTY 注入按鍵（例如 `continue`）來恢復卡住的 agent，除此之外，看起來與 operator 輸入完全相同——曾有一個裸露的注入 `continue` 被 orchestrator 誤認為 operator 命令，並因此派送任務。這類推動訊號現在會帶有 `[AGEND-AUTO kind=...]` 前綴（與 `[AGEND-MSG]` 同類）。**規則：**將 `[AGEND-AUTO]` 行視為低優先級 RESUME 訊號——繼續進行中的工作——且**絕不**將其視為 operator 命令，或用作派送任務／做出決策的依據。收件匣／operator 轉接訊息會保留自己的 `[AGEND-MSG]`/`[from:]` 標頭，不受影響。

## §7. CI

使用 `ci({action: "watch", repository: "<owner/repo>", branch: "<branch>", task_id: "<task-id>"})` 進行持續監控，不要手動輪詢。例外：合併閘門的最終驗證依 §3.3.1 要求，需執行一次性的 `gh pr checks <PR#>`。乾淨且已推送／交接的工作樹可提早釋放；刪除分支仍需 §10 的保存證明。

**禁止手動進行協調器輪詢**。協調器（負責人、一般協調器、
迴圈中的操作員）MUST NOT 透過
`gh pr view`、`gh run list`、重複執行 `cargo test` 或同等方式手動輪詢 PR / CI 狀態。
請依賴：

1. 受派者的 `request_kind: "update"` 里程碑（§6）— r0 就緒、CI
   全綠、審查者裁決。
2. `ci({action: "watch", ...})` 扇出 — `[ci-pass]` / `[ci-fail]` /
   `[ci-watch-stalled]` 會自動送達。

重複的輪詢迴圈會掩蓋失效的派工通訊，並不必要地耗用快取／
速率限制額度。如果某個里程碑在合理時間範圍後仍未出現，
正確做法是傳訊息給受派者詢問原因，
而不是開始輪詢迴圈。合併閘門或合併後精確 HEAD 驗證所要求的明確一次性檢查
則允許執行。輪詢也表示派工
簡報本身沒有列出預期的里程碑 — 應修正
派工，而不是症狀。

**PR 開啟語意（Sprint 54）**。實作者 MUST 預設將功能 PR 以
**可供審查**狀態開啟。`--draft` 旗標僅保留給
恰好以下三種情境：

1. **冒煙／驗證 PR**，且不會被合併（例如 CI
   通知路徑測試）。標題前綴為 `[smoke]` / `chore: smoke`。
2. **明確的進行中工作**，實作者需要在中途推送，
   且尚未要求審查。在通知負責人／審查者前，先移至可供審查狀態。
3. **外部 PR 修補**，負責人在上游 PR 合併前增補社群
   貢獻。

草稿 PR 會隱藏於 GitHub 的預設 UI 篩選器之外，因此操作員與
審查者若不明確檢查就會錯過。預設設為可供審查，可讓
審查管線保持可見。

**呈現設定警告（Sprint 54 P0-4）**。當無法取得 GitHub 權杖
（環境變數未設定，且 `gh` 不可用／未驗證）時，CI 相關 MCP 回應
可能包含頂層 `setup_warning` 字串。守護程序在此狀態下會以
未驗證方式輪詢，並迅速耗盡 60 req/hr 上限。
代理 MUST 在工作階段中首次出現 `setup_warning` 時，將其逐字呈現給使用者
— 這是操作員可採取行動的指引，不是
日誌行。建議措辭：「CI 監看回應：<setup_warning>」。
同一工作階段內後續出現的內容可去重。

**健康狀態介面（Sprint 54 P0-5）**。`ci({action: "watch", ...})` 回應
與 `ci({action: "status"})` 彙總器都帶有 `rate_limit_active`、
`rate_limit_until` 及 `next_poll_eta`，讓代理無須讀取監看檔案，
即可判斷 CI 輪詢是否健康。當輪詢因速率限制
時段而停滯時，守護程序也會扇出兩種收件匣事件：
連續錯過 3 次輪詢後送出 `ci-watch-stalled`（每個停滯時段
恰好一次），之後首次成功輪詢時送出 `ci-watch-resumed`。
依 P0-1 扇出契約，兩種事件都會傳送給每位訂閱者
— 不採最後寫入者勝出。應立即呈現停滯事件；恢復事件
用於確認已復原，可靜默確認。

### 7.1 CI 工具身分與快取衛生（Sprint 62）

**透過輸出形態而非結束代碼檢查工具身分。** 當 CI 步驟驗證二進位檔的身分時（例如 `cargo`、`rustc`、`rustfmt`）：

```yaml
# WRONG — rustup-init binary at proxy path also exits 0 for --version
cargo --version

# RIGHT — content-validating grep ensures shape matches
cargo --version | grep -qE "^cargo [0-9]"
```

當快取將過時的 `rustup-init` 二進位檔還原到 Proxy 路徑時，它們可能偽裝成 `cargo` / `rustc` / `rustfmt`。僅有結束代碼 0 並不能證明身分。

`↳ 緣由 A-§7.1`

**快取污染需要預防或經驗證的清理。** 如果復原介面比預防更困難，僅偵測並不足夠。KISS：優先選擇「不要快取受污染的目錄」（`Swatinem/rust-cache@v2 with cache-bin: false`），而非「偵測過時狀態並 rm + reset」。復原程式碼本身會成為維護負擔 + 新的失敗介面。

### 7.2 跨平台測試慣用法

在 2026-05-13/14 工作階段中多次觀察到跨平台測試失敗。強制慣用法：

- **時間算術**：絕不對不受信任的持續時間使用未檢查的 `Instant::now() - Duration` 或 `Instant + Duration`；當結果超出可表示範圍時，兩者都可能 panic。使用 `checked_sub` / `checked_add`，或為測試注入 `now: Instant`。
- **Regex 熱路徑**：絕不在熱迴圈中每次呼叫 `Regex::new`。使用 `LazyLock<Vec<Regex>>`（或 `OnceLock`）。效能比：約 100× 加速，可避免累積的 `min_hold` 額度造成 Windows 執行器測試逾時。
- **PTY EOF 語意**：絕不假設 cmd.exe/bash/ConPTY 的 EOF 行為一致。如果 EOF 語意差異是該錯誤而非 SUT，Shell 後端測試需要 `#[cfg_attr(windows, ignore = "tracking #N")]`。
- **路徑變形**：從來源路徑建構工作樹路徑時，同時清理 `/`（Unix 路徑）與 `\` + `:`（Windows 磁碟機代號）。

### 7.3 卡死執行的復原

當 CI 工作流程執行**卡死**時 — 某個工作維持 `in_progress` 超過一般平台完成時間的 2×，且 `gh run cancel <run-id>` 回傳成功，但工作狀態未轉換 — 將一個**空提交**推送至 PR 分支，以觸發新的工作流程執行。這是獲准的復原技術，不是因應措施。

```
repo({action: "checkout", repository_path: "<canonical>", branch: "<PR-branch>", bind: true, task_id: "<task-id>"})
cd <bound-worktree>
git commit --allow-empty -m "ci: nudge wedged runner (PR #N wedged Nhr)"
git push origin <PR-branch>
```

新的 CI 執行會在新 HEAD 上觸發；舊的卡死執行因此變得無關緊要（最終會在 6 小時後被 GH-Actions 判定逾時，而不影響合併）。由於 HEAD 已變更，先前的裁決已過時。重新蓋章前，使用相等的樹狀結構 OID（`git rev-parse <old-head>^{tree}` 與 `git rev-parse <new-head>^{tree}`）或空的 `git diff <old-head>..<new-head>` 證明內容一致；接著針對新的 `reviewed_head` 傳送新裁決。合併閘門以新的 CI 結果為準。

**套用時機** — 以下三個條件必須全部成立：

- CI 工作處於 in_progress 超過一般平台完成時間的 2×（例如 macOS 工作通常在約 10–15 min 完成；>30 min 即屬卡死範圍）。
- `gh run cancel <run-id>` 回報成功，但卡死工作的狀態在約 2 minutes 內沒有變更。
- 其他平台的工作已完成（證明問題是平台特定，而非工作流程／協調器迴歸）。

**這不是什麼**：

- **不是強制推送。** 空提交會透過快轉推進 HEAD；分支歷史得以保留。壓縮合併會將推動提交 + 實際工作摺疊為 main 上單一的 PR 提交，因此推動不會在合併後的歷史中留下痕跡。
- **不是合法測試失敗的因應措施。** 測試失敗代表真實錯誤。推動僅處理確實卡死且沒有進展的執行器 — 幾分鐘前仍可運作的相同設定，會在新的執行器上再次通過。
- **不是用於「測試很慢」。** 緩慢但持續進展的 CI 是另一個問題（快取未命中、固定裝置成本）。等待正常完成；如果緩慢是系統性問題，另行建立議題。

`↳ 緣由 A-§7.3`

此復原技術呼應 [§3.19.1](#3191-agent-git-anti-patterns) 對協定閘門復原的框架：拒絕／卡死是一個訊號，而非暫時性錯誤。記錄獲准的回應方式，讓未來的操作員不會改用強制推送或 `gh run rerun --failed`（後者會在相同 SHA 上重新執行相同的卡死平台，且常在同一執行器集區資源上再次卡死）。

## §8. 進度可見性

工作狀態變更會傳送至 Telegram。執行個體生命週期事件（非 fleet.yaml 來源）會連同 `origin` 欄位廣播。`create_instance` 預設使用隔離的工作區（`$AGEND_HOME/workspace/<name>`）。

## §9. 等待與逾時

- 使用 `set_waiting_on` 宣告阻礙因素（閒置 120s 後自動清除）
- 使用 `schedule({action: "create", ...})` 進行查看（跨後端）

**逾時階梯**（單一事實來源）：

| 派工後經過時間 | 動作 |
|---|---|
| < 20 min | 正常。`list_instances({instance: "<agent>"})` — 新鮮的心跳表示程序仍在執行，不代表工作已完成。 |
| 20 min，心跳新鮮 | 代理正在工作。延長等待。 |
| 20 min，心跳過時（>120s） | 透過 `send` 傳送直接問題。 |
| 25 min，傳送訊息後沒有回應 | 檢查工作、窗格、綁定與髒狀態。只有在持久交接狀態為最新，且不會遺失任何未提交工作時，才允許重新啟動同一代理。 |
| ≥ 1 h | 重新指派／接管需要全部四項獨立條件：心跳過時、最後輸入凍結、閒置／錯誤狀態，以及零工作活動。 |

**後端修正條件**：
- kiro-cli：多等待 1-2h（內容壓縮會自我修復）；升級至操作員處理，而非 `interrupt`
- 其他後端（claude/codex/opencode/agy/grok）：原樣使用上述階梯

### 監督器通知
守護程序偵測到代理進入錯誤狀態（UsageLimit/RateLimit/Hang/Crashed/AuthError/PermissionPrompt）→ 通知協調器。每個代理有 60s 防抖。

### 9.1 內容已滿的自我重新啟動

當代理（尤其是負責人／協調器）偵測到自身內容接近滿載（約 80-85%；窗格頁尾顯示 `N% context used`）時，它會重新啟動**自身** — 守護程序會執行終止 + 重新產生；不需要第二個代理來觸發。

- **`mode="fresh"`，絕不使用 `resume`** — `resume` 會重新載入先前的內容。只有 `restart_instance({instance: "<self>", mode: "fresh", reason: "context-full self-restart"})` 會以乾淨狀態啟動。
- **程序**：
  1. 在重新啟動**之前**，將所有即時狀態落地至持久儲存區 — 將 `SESSION-HANDOFF.md` 更新至目前狀態（交接進入點、進行中的 PR、合併程序、成員狀態、待處理派工、決策），發布任何未處理的 `decision`，確保工作位於 `task` 看板上。任何內容都不得依賴記憶體中的上下文。
  2. 確保綁定的工作樹乾淨，或進行中的變更已提交。守護程序可能拒絕重新啟動髒工作樹；未經操作員授權，不得強制執行。
  3. 選擇空檔 — 絕不在合併途中或不可逆動作的步驟中途執行。
  4. 呼叫全新重新啟動。守護程序會在重新產生後發出一次 `[AGEND-RESUME]` 啟動觸發；不要建立多餘的排程喚醒。
  5. 收到 `[AGEND-RESUME]` 時，從權威工作看板與 `list_instances` 重建狀態、清空收件匣，接著將 `SESSION-HANDOFF.md` 作為可容忍過時的提示，並繼續待處理工作。
- 重新啟動呼叫可能不會回傳，因為呼叫程序會被取代。同儕可以執行存活檢查，但不需要由其觸發重新啟動。

## §10. Git 工作流程

- 絕不可直接 commit 到 main；一律使用 worktree + branch
- 使用具有描述性的慣例 prefix，例如 `feat/`、`fix/`、`docs/`、`refactor/`、`test/`、`review/` 或 `chore/`
- 切換 task 時，release 乾淨且已 push／handoff 的 worktree；只有在下方 preservation proof 成立後才能刪除其 branch
- **Worktree lifecycle 由 daemon 擁有。** 對 dispatched work 而言，`send({instance: "<assignee>", request_kind: "task", task_id: "<task-id>", branch: "<branch>", message: "<brief>"})` 會把 assignee（不是 dispatcher）綁定到 daemon-managed worktree。以 `binding_state({instance: "<self>"})` 找到該 worktree，`cd` 進去，再於其中使用一般 git。不得執行 raw `git worktree add`、不得切換 canonical repo，也不得用 bypass 逃避 shim deny。
- **Provision/re-bind**：fresh task 應優先使用 `repo({action: "checkout", repository_path: "<canonical>", branch: "<branch>", from_ref: "<base>", bind: true, task_id: "<task-id>"})`。只有在重新綁定 recovered worktree、從 fleet metadata 解析 source repo，或 release 後重新取得同一 branch 時，才使用 `bind_self({repository_path: "<canonical>", branch: "<branch>", task_id: "<task-id>"})`。Protected branch 會被拒絕，cross-agent conflict 必須透過 owner/lead 解決。Binding 必須搭配 `release_worktree({instance: "<self>"})`。
- 一般 bound push 會參與 daemon lifecycle/CI integration。若經 operator 授權的 exceptional push 繞過該 integration，必須明確啟用 `ci({action: "watch", repository: "<owner/repo>", branch: "<branch>", task_id: "<task-id>"})`；見 §13。

### release_worktree branch-cleanup 範圍

Release daemon-managed worktree 與刪除其 local branch 是不同的動作：

1. Commit 已 push 或已 durable handoff 的乾淨 worktree，可以在 merge 前 release。Daemon 會保留 unmerged branch，並記錄 cleanup intent。
2. Local branch deletion 必須符合下列任一條件：branch 是 main 的 ancestor；provider 證明 matching head 的 PR 已 merge；或 structural squash proof 通過，且同時符合 24-hour age floor。
3. 只有 remote tracking ref 消失，絕不構成 deletion proof：local-only commit 可能仍需保存。
4. Protected ref（`main`/`master`）絕不會被更動。

Automatic lifecycle cleanup 只適用於具有已驗證 `.agend-managed` marker 的 daemon-managed worktree。User/operator 建立的 worktree，以及任何無法驗證的 marker，都會保留。

### release_worktree parameter 形式

使用 `release_worktree({instance: "<self>"})`。Forced recovery 還需要已知 branch：`release_worktree({instance: "<self>", force: true, branch: "<branch>"})`。缺少 required `instance` 會 hard-reject；額外的 unknown key 可能只會 warning 後忽略，因此絕不可把它們視為 cleanup。以 `binding_state({instance: "<self>"})` 回傳 `bound: false` 驗證成功。

### 10.6 Dispatch Binding Ownership

帶有 `branch` 的 task send 會自動綁定 **assignee**，不會綁定或移動 dispatcher。因此 dispatcher 不得把 release 自己的 worktree 當成一般 pre-dispatch hygiene。

如果 assignee 已綁定另一個 branch，必須在 dispatch 前處理該 binding：要求 assignee commit／handoff 並呼叫 `release_worktree({instance: "<assignee>"})`，或由 authorized orchestrator 使用帶 exact branch 的 forced form。絕不可推測性地 release 另一個 agent 的 worktree；active dirty binding 可能包含尚未回報的工作。

### 10.7 Worktree Branch 上的空 `init` Commit

Backend CLI（包括 Claude Code、Codex 與 Kiro CLI）可能在 bound worktree 中建立空的
`init` session-checkpoint commit。Scratch-test leak 過去也曾是來源，
但 repository test 現在會防護 mutating scratch-repo
git command；`t <t@t>` committer 並不是永恆或唯一的 RCA。不得只從 subject 或 committer
推斷 producer。

現行 git interception 與 pre-push cleanup 已移至 vendored
`agentic-git`（`cleanup_init_pile_pre_push`）。In-tree `agend-git` binary
只處理 kill family，不再處理 git，因此其舊 line reference 與
behavior 不再是 operational source of truth。在一般 guarded push 中，
只有在 guard 證明 subject、body 與 file diff 都安全後，才會移除符合條件的
`init` / `initial` commit。Daemon 也提供
`repo({action:"cleanup_init_commits", instance:"<agent>"})`，供明確提出
cleanup request。

**Agent guidance：** 絕不可用 reset、rebase、amend 或
force-push 手動清理這些 commit。正常 push，讓 guard 執行其 bounded cleanup。若
unexpected commit 非空、有 meaningful body，或通過一般
cleanup 後仍存在，請保留它，並向 lead 回報 exact branch/SHA；不得只因
`init` 一詞就把它分類為 harmless。Reviewer 仍需在 daemon-managed worktree 中驗證 immutable RED
與 GREEN ref（§3.10/§3.20）。

### 10.8 Backend TUI Render Duplication（#1464）

Backend pane 偶爾會連續顯示**同一行 rendering 兩次**，
即使 source content 只有一份。這是 **backend-renderer
artifact，不是 agend bug**——與 #1401
residual-text investigation 屬於相同 root-cause class：inner backend 的 TUI 會做 partial redraw /
reflow，因而重新 emit（或沒有清除）某一行。

agend 自己的 layer 已證明 faithful，問題**不是**源於此處：
- `VTerm::process` 是 **pure alacritty**（`processor.advance`）——沒有任何 custom
  grid/scroll/line manipulation；grid 會精確反映 backend emit 的 byte。
- `render_to_buffer` 是 **monotonic 1:1** grid→buffer copy（每個 viewport row
  精確對應一個 grid line）——它不可能複製一行。

此問題只是**外觀問題**，且會在下一次 full repaint（resize，或任何
觸發 `terminal.clear()` 的 event）自行修復。**不得在 agend 的
`vterm` / `render` layer 尋找 fix——兩者都是乾淨的。** 若未來要做 mitigation，應放在
inject-timing / forced-redraw layer，而不是 render path。

### 10.9 GitHub CLI Authorship Signature（soft convention）

所有 fleet instance 共用 operator 的 GitHub account，因此由 `gh` 建立的 issue、PR 與 comment 都沒有 instance/model attribution——這與 git commit 不同；git commit 會透過 `prepare-commit-msg` hook 自動加上 `Agend-Agent` trailer（§10.7）。系統**沒有** `gh` shim；這只是 soft convention，不是 enforced interception。

使用會帶 body 的 `gh` action（`gh issue create`、`gh pr create`、`gh pr comment`、`gh pr review`）時，若 attribution 很重要——例如 multi-agent thread、cross-team handoff，或 operator 日後可能需要追查到特定 instance 的任何內容——就在 body 加上一行 signature：

```
---
*<instance-name>* · <backend/model>
```

Instance 使用 `$AGEND_INSTANCE_NAME`，並填入已解析的 backend/model。若只是 authorship 顯而易見的 trivial passthrough comment，可以略過。這是 soft requirement——省略不會造成 gate failure。

↳ 緣由 A-§10.9

## §11. Tool 快速參考

| 需求 | 使用 | 不要使用 |
|---|---|---|
| 追蹤工作 | `task({action: "create" / "claim" / "update" / "done", ...})` | local task list |
| 記錄 decision | `decision({action: "post", ...})` | 只有 Markdown 的 decision |
| 指派工作 | 先 create task，再用 `send({request_kind: "task", task_id: "...", ...})` | 只做其中一項 |
| 回報結果 | `send({request_kind: "report", parent_id: "...", correlation_id: "...", ...})` | pane text |
| CI monitoring | `ci({action: "watch", repository: "...", branch: "...", task_id: "..."})` | manual polling loop |
| CI merge gate | `gh pr checks <PR#>` | 信任 dev 自行回報的結果 |
| Waiting state | `set_waiting_on({condition: "..."})` | prose |
| Instance health | `list_instances({instance: "..."})` | 猜測 |
| 清除 blocked health | `health({action: "clear", instance: "..."})` | stale local note |
| Schedule | `schedule({action: "create", ...})` | backend-specific tool |
| Timeout | §9 staircase，接著 `restart_instance({instance: "...", mode: "fresh", ...})` | 立即 destructive restart |

**Daemon 無法連線時的行為。** Agent-facing MCP bridge 是 daemon proxy；
不得以 local/offline fallback 為基礎規劃任何 tool workflow。Daemon
connection 無法使用時，tool call 會回傳可採取行動的 connection error，
而不是證明 mutation 或 delivery 已發生。請顯示該 error、
恢復 daemon/socket，再以相同 correlation identifier 重試原始 operation。
Test 或 recovery code 使用的 internal handler fallback，並不是 agent-facing availability contract。

### 11.1 Daemon Refresh 後的 State Persistence（Sprint 62）

Daemon binary refresh（recompile + restart，或透過 `mcp_registry_watcher` hot-reload）會使數個 in-memory state store 失效。**每次 `mcp_registry_watcher` notification 觸發後**，都應重新驗證下列 state：

- **CI watch state**——已由 #786 修正，但 #786 前建立的 watch 可能遺失
- **Instance registry 與 team metadata sync**——已由 #785 修正（better-error 會顯示 desync）；team membership 會比 instance restart 存活更久，可能指向已清除的 instance
- **Team 上的 source_repo**——歷史上 refresh 時會被 `teams.json` migration 清除（原為 #781 root cause）；#781 後已持久化，但若 behavior 不如預期，請用 `grep source_repo fleet.yaml` 驗證
- **Active binding**——in-memory `bind_in_flight` flag 可能遺失；用 `binding_state({instance: "<agent>"})` 檢查。若已證明 binding dangling，使用一般 `release_worktree({instance: "<agent>"})`；guarded force recovery 還需要 `force: true` 加上 exact `branch`。

**Operator workflow**：`mcp_registry_watcher` notification = restart-needed signal。執行 `agend-terminal stop && cargo build --release && agend-terminal start` 以載入新 binary。後續 agent dispatch 將受益於 fresh code。

**Agent workflow**：不得假設 state 能在 daemon refresh 後存活。收到任何 refresh notification 後，透過 `team({action: "list"})`、`ci({action: "status"})` 與 `binding_state({instance: "<self>"})` 重新驗證。

## §12. 工作流程效率

### 12.1 Pipeline Dispatch
Push PR 後立即開始下一個 task。Depth ≤ 2。必須從 main 建 branch（不可 stack 在 pending PR 上）。

### 12.2 Reviewer 不等待 CI
PR push 後立即開始 review。`reviewed_head` 是 snapshot；後續 commit 會重設 verdict。

### 12.3 Task Close
`in_progress` → `verified`（reviewer）→ merge（依 §3.3.1，CI green）→ post-merge main CI green → `done`。

**Post-merge verification**：Squash-merge 後，擷取 immutable merge SHA，並由 target team orchestrator/operator 登記 exact-head protected-branch watch：

```
ci({action: "watch", repository: "<owner/repo>", branch: "main", head_sha: "<full-merge-sha>", task_id: "<task-id>", next_after_ci: "<orchestrator>"})
```

只有 matching exact-head success 才能關閉 task；較新的 unrelated main run 不是此次 merge 的 evidence。Protected exact-head watch 目前只支援 GitHub；若使用其他 provider，必須取得綁定 merge SHA 的 provider-native evidence，否則將 close gate 回報為 UNVERIFIED。若該 exact SHA 失敗，立即調查並修正（必要時 revert）。

### 12.4 Worktree 強制要求
Impl/reviewer 必須在 worktree 中工作，絕不能使用 canonical working tree。一般帶 branch、已啟用 binding 的 task dispatch（非空 `branch`，且沒有 `bind:false`）會自動綁定 assignee；branchless 與 `bind:false` dispatch 不會。用 `binding_state({instance: "<self>"})` 確認，`cd` 到回報的 worktree，再使用一般 git。不得用 raw git 自行 provision；應透過 `repo({action: "checkout", repository_path: "<canonical>", branch: "<branch>", from_ref: "<base>", bind: true, task_id: "<task-id>"})` 明確 provision，或使用 §10 中 recovery-oriented `bind_self` form。Agent 絕不可把 shim deny 當成 bypass 許可。完整規則與 exception：§12.4 與 §13。

### 12.5 Spawn Site Rationale
每個 spawn 都必須有 `// fire-and-forget: <reason>`，或保存 JoinHandle。Test-only exempt。

### 12.6 Multi-PR Wave 依序 Merge
同一 wave（相同 dispatch/task_id）中有多個 PR 時：
1. 依序 merge：A → 在新 main 上 rebase B → 重新驗證 CI → merge B → ...
2. 絕不可 parallel merge——後續 PR 的 base 已 stale
3. 每次 merge 後，其餘 PR 都必須 rebase 並重新執行 CI，才能 merge。Rebase 會改變 `reviewed_head`，所以先前 verdict 已 stale：必須 re-review 新 head；若 tree byte-identical，也可以發布新的 re-stamp，並引用相等的 tree OID／empty diff。

此限制會在 dispatch message text 中傳達（沒有 daemon-enforced param——已移除的 `send.sequencing` passthrough 沒有 consumer）。Recipient **必須**一次 merge 一個，並在每次 merge 間驗證 CI。

### 12.7 Linked-Issue Close Convention

會解決 tracked issue 的 PR，**PR body** 中必須包含 closing keyword（`Closes #N` / `Fixes #N` / `Resolves #N`），讓 platform 在 merge 至 default branch 時自動關閉 issue。

- Bare `#N` reference **不會** auto-close，而且含義不明——mention/cross-reference 並不等於 fix（例如 cluster-sibling issue 仍可能 open）。只有 PR 確實解決 issue 時，才使用 keyword。
- **Daemon 不會 auto-close。** Daemon-side `Closes #N` parser 會與 native platform behavior 重複；在採用此 convention 前也無作用，而且 `gh issue close` 只支援 GitHub（與 multi-platform `ScmProvider` 方向衝突）。
- **只作 reinforcement。** 這是 convention，不是 gate；lead 會在 merge 時手動關閉任何漏網項目。

`↳ 緣由 A-§12.7`

## §13. `AGEND_GIT_BYPASS=1` 使用方式

**TL;DR：** agent 在 daemon-managed worktree 內使用一般 git，且絕不 bypass shim denial。Bypass 只保留給 daemon internal，以及經 operator 明確授權的 repair/bootstrapping exception。

### 13.1 不應使用 bypass 的情況

在 bound worktree 內，所有日常 git op 都能乾淨地通過 shim。直接執行：

```bash
git status / diff / log / show
git add / commit / fetch
git push origin <your-branch>     # any branch except main
```

不得預先加上 `AGEND_GIT_BYPASS=1`。若 shim deny 某個 action，請停止，依照 daemon-managed remediation 處理，或詢問 lead/operator；deny 不是從 guard 下方重試的許可。

### 13.2 已授權的 bypass scope

允許的 scope 很窄：

- Daemon-internal git helper 會設定 bypass，以避免遞迴進入自己的 shim。
- Daemon-managed release/recovery route 全數用盡後，operator 可以授權一次性的 repair command。記錄 command、reason、受影響 repo/branch 與 result。
- §13.5 允許修復會阻礙自身正常 delivery 的 bug，但必須有明確 operator authorization，並提供該節定義的 PR disclosure。
- Repository-owned test wrapper 可以在內部設定 bypass，供自身 nested git probe 可能遞迴的 tool 使用（例如 configured `nextest` wrapper）。Agent 不得臨時自行加上 prefix。

Raw worktree lifecycle、切換 protected branch 與 push 至 main 都不屬於 agent bypass scope。使用 `repo`、`bind_self`、`release_worktree` 與 PR/merge workflow。

### 13.3 Bypass 為何代價高昂

跳過 shim 就會跳過 safety net：

- **跳過 Phase 1 trailer**——commit 缺少 `Agend-Agent: <name>` provenance，破壞 audit trail
- **跳過 Deny matrix**——risky op（force-push 至 protected ref 等）會在沒有 guard 下執行
- **Git registry 可能 drift**——在 daemon pool 外執行 `git worktree add` 會留下未追蹤 entry；後續 lease 可能衝突
- **跳過 Phase 5 hotspot warning**——flagged file 的 concurrent edit 不會在 dispatch path 顯示

其中任何一項都可能使 review 失效，或讓 operator state 無法復原；請把 bypass 視為 audited exception，而不是便利功能。

### 13.4 預設工作流程

1. 直接執行 `git <command>`。
2. 若 shim deny，閱讀 deny message——它會指出具體原因並建議 remediation。
3. 若有建議，依照 daemon-managed remediation（`repo`、`bind_self`、`release_worktree`）處理。
4. 如果唯一建議的 remediation 是 bypass，agent 應暫停並要求 lead/operator 指示。只有 operator 或明確授權的 procedure 能核准 exact one-command scope。

`AGEND_GIT_BYPASS_UNTIL=<epoch>` 用於 audited、time-bounded operator intervention；它不是 agent convenience flag。

### 13.5 Bug-Blocks-Its-Own-Fix Exception（Sprint 62）

修復 daemon binding bug（或其他因自身存在而阻礙 bypass-free workflow 的 bug）時，只有在已證明一般 daemon-managed recovery 無法 delivery 該 fix，且 operator 授權 exact scope 後，fix PR 才能使用 one-command bypass。

**此 exception 的 acceptance criteria**：
1. PR body **必須**包含 `## Bypass scope rationale` section，明確描述這個 loop：
   - 正在修正的 bug
   - 為何此 fix 會消除未來的 bypass 需求
   - One-shot scope 只限此單一 PR
2. 在 task 或 decision log 記錄 operator authorization，以及每個 bypassed command/result
3. Bypass commit 在 branch history 中仍可 review；squash-merge 可以壓縮最終 main
4. PR merge 且 daemon 更新後，所有後續工作都回到 zero-bypass workflow
5. Worktree manipulation 與 protected-branch mutation 仍屬禁止；此 exception 不能默示授權它們

`↳ 緣由 A-§13.5`

---

## Appendix A——理由與 Incident Log

Normative rule 背後的 *why* 與 *when*。Incident narrative、activation history 與 empirical motivation 已從 rule text 移至此處；normative layer 會透過 `↳ 緣由 A-§X` 引用。除非你要質疑或修訂某項 rule，否則不一定要閱讀本 appendix。

### A-§3.3——Evidence 位於 Claim 之外
有些 review 曾接受 comment 或 PR prose 作為 reachability 與 scope 的 proof，之後檢查 source 才發現 dead path、bypass call site 或 missing event。此 rule 要求使用 executable behavior 或引用 source 作為 evidence，而不是重述 author 的 claim。

### A-§3.3.1——CI Verification Gate
Sprint 61 incident——ci_watch 在只完成部分工作時發出錯誤 [ci-pass]，導致 failing code 被 merge。

**Flake-evidence rule：**「rerun + label flake」的制式反應曾一再掩蓋 deterministic failure——某次 CI Coverage run 變紅時，多數其實是 REAL failure，卻被錯標為 flake，結果只是不斷 rerun，而沒有 fix。反覆出現的陷阱，是從 local/worktree pass 推論「它是 flaky」：local green ≠ CI green（platform / timing / parallelism / env 不同），因此 local pass 不能證明 CI failure 是 non-deterministic。要求提供 `gh run view <id> --log-failed` 中實際的 failing-test name，能迫使 claim 指出真正且已知的 flake signature，才有理由 rerun；若沒有該 evidence，預設就是「真正 failure，應修正」。

### A-§3.12.1——採用 `gh pr merge --auto`
**Activation status**：#986 gh-poll integration 交付後（PR #990，merge commit 4242c24），自 2026-05-20 起為 ACTIVE。在此日期前，canonical form 是舊有 synchronous `gh pr merge <N> --squash --delete-branch`，因為 `--auto` 的 async return 會丟失 synchronous merge confirmation。#972 PR-state aggregator + #986 gh-poll integration 都 live 後，`[pr-merged]` event 現在會在 GitHub 真實觀察到 merge 後觸發，恢復 async-flow visibility，並讓這個 default switch 得以啟用。

**Async confirmation pipeline（#972 + #986）**：`--auto` 會立即 return，因此不再有 synchronous「PR merged at SHA」terminal feedback。Daemon PR-state aggregator（#972，merged be23875）+ gh-poll integration（#986，merged 4242c24）會在觀察到 GitHub-side merge 後，共同向 PR author 的 inbox 發出 `[pr-merged]` event。Author 會等待該 event，而不是 polling。

**Activation history**：§3.12.1 在 #973（此 rule 的 home PR）引入，但在 #972 + #986 都交付前保持 INACTIVE。Activation switch 於 2026-05-20 以 docs-only follow-up 落地（把 canonical form 從舊有 `gh pr merge ... --squash --delete-branch` 改為 `gh pr merge ... --auto --squash --delete-branch`）。

### A-§3.16——Phase 1 Discussion Discipline
理由：lead 根據 code structure 做出的推論，持續漏掉 scope hole（2026-05-14 retrospective 中有 8/12 PR 如此）。

### A-§3.19.1——Agent Git Anti-Pattern
這兩種 failure mode 都在 #863 reviewer incident 中實際出現。Bypass 通常會在原本問題之上暴露 hidden state：在 #863 incident 中，繞過 checkout deny 產生了一個 target branch 上根本不存在的 phantom `.gitignore` conflict，讓 reviewer 卡在虛假的 merge conflict。

### A-§3.19.2——Reviewer Base Workspace Branch Discipline
2026-05-20 incident——fixup-reviewer base dir 被發現卡在 `fix/900-spawn-env-propagation`，留下 2026-05-18 in-place checkout 產生的 492 個 deletion marker，且從未 revert。Recovery 造成 session backend state 損失（`.codex/.claude/.gemini/.kiro/.opencode/AGENTS.md` 被 `git clean -fd` 移除，因為這些 dir 不在 fix/900 時代的 `.gitignore` 中）。Reflog 顯示原本的 Sprint discipline 正確使用每個 PR 一個 `review/NNN-r0` worktree（2026-05-16 entry），之後才 drift 到 in-place checkout。

### A-§3.19.3——Source-File Lookup：禁止 Full-Disk Scan
#2386（2026-06-23，operator-confirmed）：某個 `de2eb8` code-review workflow 的 agent/subagent，各自臨時執行 full-disk `find / -name agend-git.rs` 以尋找 git-shim source。當 fleet 並行執行這些 scan 時，16-core machine 的 load 飆到 **108**——這是一種 one-shot blowup，只要 agent 不知道 path 就使用 `find /`，便會重現。這不是 `agend` scan（daemon 絕不這樣做）；修正方式是 preventive guidance（§3.19.3），不是 code gate：在 bound worktree 內使用 index-scoped lookup（`git ls-files` / `rg`），既正確又低成本。

### A-§3.20——Race-Condition PR Discipline
Empirical motivation：#881（「app mode never owns the daemon」）在 2026-05-17 以 CI green + reviewer VERIFIED 交付，第一次在 slow filesystem flush 的 cold-start 上就出現 spawn-and-attach race。Operator 在 470c251 revert；#882 reopen fix 以 0fd89e8 交付，包含 probe_api gate + `--foreground` mode + actionable error path。

SOP 2 為何是 post-merge，而不是 gate：pre-merge smoke gate 會造成 chicken-and-egg problem。Operator 日常使用的 binary 來自 main；若要 smoke 尚未 merge 的 race-class PR，必須 (a) 手動從 branch build，並 (b) 把 `$AGEND_HOME` 指向不同於日常 setup 的位置。於是 gate 會阻止 merge，直到 smoke 確認——但不經 operator side-work、打破 merge flow，就無法執行 smoke。PR #908（2026-05-18 的 #896 fix）正是卡在此 loop；當時 operator directive 是：「smoke gate會造成 chicken-and-egg的問題，要拿掉」。

SOP 3 的門檻——fixup-reviewer 對 #882 的 verdict 原文：「Checked out pre-fix revert base 470c251 in this worktree. Verified target helper/tests are absent there by source grep.」

### A-§3.22——Spike-First Planning Gate
此 rule 源自 2026-06-18 governance batch（operator D1–D5）。反覆出現的 failure mode：合併 spike+impl 的 dispatch，會先承諾一個之後被 spike 推翻的 impl scope——例如 #2325 的「copy key is broken」framing，與 D1 的「parse `Closes #N`」approach，都在 spike 真正閱讀 code／檢查 native platform behavior 後反轉；因此，任何預先核准的 impl 都會建置錯誤項目。分開 dispatch 並用 decision-manifest gate impl，能讓 premise check 成為真正必要條件，而不是裝飾，也能避免 batch approval 核准 unknown scope。

### A-§7.1——CI Tool Identity 與 Cache Hygiene
此 pattern 在 2026-05-14 PR #772 v1 → v3 演進中被捕捉；v1 的 `cargo --version` exit check 漏掉 pollution；v2 detection-recover 失敗；v3 的 `cache-bin: false` prevention 交付成功。

### A-§7.3——Wedged-Run Recovery
PR #863（#852 residual PR-A）在 2026-05-16 遇到 `windows-latest` wedge。Job 從 15:19 UTC 開始維持 `in_progress` 超過 9 小時；`gh run cancel` 已接受，但 job 從未 transition。16:14 UTC dispatch empty-commit nudge → fresh CI 觸發 → 約 10 分鐘內 green → merge 繼續。此操作經 operator 授權，總 recovery 約 5 分鐘；另一方案則是等待 GH-Actions timeout 6 小時，再手動 re-trigger。

### A-§10.9——GitHub CLI Authorship Signature
#2109 提議新增 `agend-gh` shim（類似 `agend-git`），自動把 instance+model 加入 gh body。Operator 在 2026-06-14 否決此 shim：`agend-git` shim 是 behavioral-correctness 的必要條件（worktree redirect，#821/#1463）；沒有它 daemon 就會失效。相較之下，gh authorship 只是 observability cosmetics——為此新增第二個 PATH-hijack shim binary 屬 over-engineering。因此降級為 §10.9 soft convention；#2109 以 note 關閉。

### A-§12.7——Linked-Issue Close Convention
Operator decision 2026-06-18（governance D1）。D1 spike 發現：(1) 最近的 fleet PR 使用 bare `#N`（0/12），它永遠不會 auto-close，而且含義不明（reference ≠ fix——例如 #2158 曾被 merged PR 以 bare form 引用，但它保持 open 是合理的）；(2) daemon-side `Closes #N` parser 會與 GitHub/GitLab native auto-close 重複，而且在 PR 採用此 keyword 前沒有作用；(3) `gh issue close` 只支援 GitHub，與 multi-platform `ScmProvider` 方向衝突。因此 fix 是 convention（使用 keyword → native close），不是 daemon code。第一次真實使用：#2325 / PR #2328 透過 `Closes` keyword 自動關閉 #2325；漏網項目的 fallback 是 lead 在 merge 時手動關閉（例如 #2327）。

### A-§13.5——Bug-Blocks-Its-Own-Fix Exception
Reference：PR #779（Sprint 61）Option 1 + Option 3 daemon binding fix 依此 exception 交付。PR #781 + #800 遵循標準 ZERO BYPASS workflow。

---

## Appendix B——Section Number Map（舊 → 新）

| 舊（v1 full） | 新（condensed） |
|---|---|
| §3.5.5 | §3.6 |
| §3.5.9 | §3.7 |
| §3.5.10 | §3.9 |
| §3.5.11 | §3.10 |
| §3.5.12 | §3.11 |
| §3.5.13 | §3.12 |
| §3.5.14 | §3.13 |
| §3.5.15 | §3.14 |
| §3.6 | §5 |
| §10.1-10.5 | §12.1-12.5 |
