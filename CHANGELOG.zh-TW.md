[English](CHANGELOG.md)

# 更新日誌

本文件記錄本專案所有重要變更。
格式基於 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)；專案遵循 [SemVer](https://semver.org/spec/v2.0.0.html)。

## [0.9.0] — 2026-06-19

自 0.8.0 起 228 個 commit。以下依主題整理重點(非完整 commit 清單)。

### Added

- **多專案任務板(#2117 P1–P3)** — 任務板現在具備專案意識。`BoardRouter` + per-board 索引把每個 `task` 指令路由到正確的專案板(#2122);dispatch 自動標記目標專案並 per-board sweep(#2125);變更路徑由 per-board ACL 把關、fleet 解析 fail-closed,且 create 時拒絕跨專案 `parent_id`(#2134、#2136);branch lease 改以 `(source_repo, branch)` 為鍵,跨 repo 同名 branch 不再衝突(#2137)。`depends_on` 可跨板解析(#2230),`task health` 聚合所有板(#2229)。單→多專案轉換不回溯重歸舊任務 —— 已記為接受的語意(#2322)。
- **非同步決策板(#2305)** — agent/operator 可貼出帶建議選項的待答問題並非同步收集答案:後端(#2308)+ TUI 內 `Ctrl+B D` 互動作答 overlay(#2309)。
- **選取即複製 + 跨平台複製(TUI)** — 選取即複製,雙模式 + 雙控制面(#2325、#2328);`Ctrl+Shift+C` 複製選取,Win/Linux 對等(#2295);選取高亮生命週期打磨 —— 複製完成後清除(#2294)、左鍵點擊取消(#2296)、拖放放開時保留(僅由複製鍵觸發複製)(#2302)。
- **每實例自訂 skills(#2321)** — `fleet.yaml` 的 `skills_path` 可設定 per-instance skill 目錄;skills symlink 表補上 `agy` 後端(#2326)。
- **origin-aware 進度回報(#2247 —— 預設 OFF、exfil-gated)** — agent 自我策展的 report 模式(#2283)與 raw transcript-tail mirror 模式(#2293),搭配可行動的 `[AGEND-PROGRESS]` daemon nudge marker(#2284)。
- **專案範圍的 DCO sign-off(#2298)** — 當 repo 帶有 DCO workflow 時,`prepare-commit-msg` hook 自動加上 `Signed-off-by`。
- **可讀的任務板欄位(#2306、#2307)** — 九種任務狀態在 TUI overlay 重整為五個可讀欄位。
- **從卡住的 agent 回收工作(#2127)** — 從不可恢復的 usage-limit agent 回收任務板任務(#2133),並把其待處理的 inbox dispatch 重新路由回 dispatcher(#2142)。

### Changed

- **自我 respawn restart 預設開啟(#1814 Stage 4、#2094)** — `AGEND_RESTART_HANDOFF` 由 OFF→ON:daemon 在退出前生成並健康把關自己的後繼者,launchd KeepAlive 契約收斂為 restart-on-failure(#2093)。
- **更快更順的 restart** — app-mode shutdown teardown 平行化(~6 秒 → grace 窗,#2311);old-exit→new-launch 的 gap 與 restart timing 加上 instrument(#2310、#2275);release build 採 ThinLTO + 更多 codegen units(#2265)。
- **canonical worktree reconcile(#2234 —— flag-gated、預設 OFF)** — 把 `workspace/<agent>` reconcile 成 canonical git worktree(#2262),含 layout-aware GC + agent 歸屬(#2263、#2266、#2269)、in-place dispatch checkout(#2264)、reconcile-backups 保留 GC(#2272)、reverse-reconcile 回滾原語(#2267)。`agend-git` shim 對 cwd↔worktree 漂移發 WARN(#2254、#2278),並在 canonical-rooted repo 中 DENY agent 的 `AGEND_GIT_BYPASS` provisioning op(#2316)—— 此 deny 現在會穿透 leading `-C` 解析真正的 subcommand 與 effective cwd,使 `git -C <canonical> worktree add` 從任何 cwd 都正確 DENY(#2336),訊息也改為條件式措辭以涵蓋無 auto-bind worktree 的 no-branch dispatch(#2334)。
- **治理閘機械化** — 反覆手追的 review 規則變成 invariant 測:狀態檔必須走 `store::atomic_write`(D2,#2323),instrument/audit 路徑永不影響 control-flow 或 exit code(D3,#2324);三條 protocol 規則 + context-full 自我 restart 流程已形式化(#2329、#2157)。
- **依賴升級** — crossterm 0.29、regex、serde_json、insta(#2285–#2289)。

### Fixed

- **rate-limit / server-overload 恢復** — agent 自清 rate-limit block 後緊接的第二個 `529` 不再靜默卡死:self-clear latch 在新的 `ApiError` hook 上重置以重新 arm retry(#2318),且堆疊的 `AGEND-AUTO` retry nudge 合併保留最新(#2319)。另外:ratelimit-retry over-inject 由 agent self-clear 的 ground-truth 訊號把關(#2239);fresh-restart 注入一個恢復首回合,讓失憶的 respawn 不會空等(#2255);窄 pane / hard-wrap 的 rate-limit 行可被偵測(#2089、#2091、#2087、#2261);並辨識 OpenCode/agy 的 usage-limit 措辭(#2276、#2258、#2236)。
- **Telegram 健壯性** — 網路失敗時 polling 退避而非 panic-loop(#2200、#2224);全新安裝在空 allowlist 時 fail-fast,quickstart 自動填入 sender id(#2207、#2225);sender 顯示名稱從 allowlist 解析(#2045);關閉經由 `reqwest` error URL 的 bot-token 洩漏(#2178)。
- **CR-2026-06-14 可靠性與安全硬化** — 一輪廣泛清掃:case-insensitive `branch="Main"` protected-ref 繞過(#2172)、Telegram notify 改同步驅動而非丟到未驅動的 runtime(#2152)、char-aware pane 解析(非 ASCII 不 panic)(#2149)、lock-ordering / dedup / keepalive 修正(#2197)、worktree 資料遺失防護(#2193、#2194)、有界的 `gh` CLI subprocess(#2191),以及更多 inbox / state-capture / worktree 發現(#2181、#2187、#2190、#2201、#2203、#2205、#2212、#2221、#2223)。
- **control-plane 與 TUI 可靠性** — `restart_daemon` 在 app/owned 模式 fail-closed,而非把 control plane 弄 brick(#2103);reused worktree 在 lease 重取時 force-sync 到 HEAD(#2226);daemon git-spawn 的 stdio 不再弄亂 TUI 畫面(#2073)。
- **CI-watch 信號準確度(#2335)** — ci-watch 現在只在 required status check 上 gate `[ci-fail]`,非-required check(如 Coverage)失敗不再觸發假的 `[ci-fail]` + re-nudge。
- **stray-worktree 衛生(#2158、#2337)** — 每小時一次、emit-once 的 sweep 把 GC 無法回收的 stray daemon-managed worktree 以 fleet health「Workspace violations」count 呈現。

## [0.8.0] — 2026-06-12

### Changed

- **MSRV:宣告的 `rust-version` 更正為 1.88(#1994)** — 先前的宣告(1.87)是錯的:鎖定的依賴集合最高需要 rustc 1.95(`sysinfo 0.39.3`),因此 `cargo install agend-terminal --locked` 對所有信任此宣告、停在 1.87–1.94 的人都會壞掉。`sysinfo` 釘選為 `0.38`(MSRV 1.88,無程式碼變更 —— 由 #1987 的 release gate 在首次執行時抓到,該 gate 現在會強制此下限)。Builder 需要 rustc ≥ 1.88。

### Fixed

- **Rework lease 衝突:reviewer-binding 洩漏 + detached-HEAD 重用(#2010 item 2)** — reviewer 駁回一個 PR 後,把 rework 重新派發到同一分支會碰到 lease 衝突,需要手動 `release_worktree`。兩個成因:(2a) `REJECTED`/`UNVERIFIED` 判定從未排入 auto-release intent(該閘只比對 `VERIFIED`),而且即使排入了,open-PR invariant 仍會把 reviewer 的 binding 綁在 PR-terminal —— auto-release 閘現在放寬到所有 terminal 判定,並且僅在 verdict-sender 自己的 binding 上、當其 review task 已 terminal 且 worktree 乾淨時繞過 open-PR 檢查(implementer 的 binding 結構上不受影響);(2b) 重用 agent 既有、其 HEAD 已 detached 的 worktree(`git branch --show-current` 回傳 `Some("")`)永遠回 lease 衝突 —— 現在乾淨時會重新接上請求的分支並重用該 worktree,而 dirty 的 detached worktree 仍會衝突(保護進行中的 review WIP)。RCA 功勞:@cheerc。

- **Reply-ledger 將未回覆路由給 agent,而非操作者的 telegram(#2042)** — Phase-1 稽核在 agent 把使用者訊息留著沒回時,會把一則維護者口吻的 WARN(`msg Some("m-...")`)送到操作者自己的通道。Phase 2 將此義務以升級階梯路由給可採取行動的一方,每一階至多一次:虧欠的 agent 被 nudge(附上 message id + reply-tool 指示;送出失敗會用重試措辭),其 lead 在第二次漏接時被通知,操作者只在萬不得已時才以人類措辭聽到。WARN 仍留在日誌中。同一邏輯訊息的重複投遞(操作者重送、通道重投重播)現在會歸併為一個義務:回覆任何一份副本即可全部結清,而對已回答訊息的重投不會開啟新義務 —— 關閉了實際命中的那些假 no-reply WARN。

- **fleet.yaml `model` 現在在執行期 respawn 時也會套用,不再僅限 daemon 開機(#2038)** — `restart_instance` / `replace_instance` / `start_instance`(以及 deploy/team spawn)respawn 時沒帶 `--model`,因此寫好的 `model:` 會悄悄沿用 backend 預設值,直到下次完整 daemon 重啟。與 #900 env 修復同屬 config-honesty 類別:SPAWN handler 現在會從 fleet.yaml 重新解析 `model`(對於無參數的 replace flow 還有 `args`),caller 傳入的 `--model` 仍優先。Crash-respawn 本來就保留開機時解析的 argv,不受影響。

- **`from_ref` 在多 remote / fork checkout 下解析到正確的 remote(#2010 item 1)** — `repo checkout` 的 `from_ref` 把 fetch 與 remote-tracking ref 檢查的 remote 硬寫成 `origin`,因此像 `fork/main` 這種 fork-tracking ref 會對著錯誤的 repository fetch 與驗證(origin-only 的設定下潛伏)。remote 現在改以對實際 `git remote` 清單做最長前綴比對來解析 —— 含 `/` 的分支名稱也能正確處理 —— 沒有任何匹配時回退到 origin。RCA 功勞:@cheerc。

- **被開啟的 dialog 吞掉的注入派發現在會自我修復(#2044)** — 操作者開啟的選單(例如 `/model`)可能吞掉一次注入派發:鍵擊跑進了 dialog、prompt 從未送出、wake 就這麼悄悄遺失。新的 inject-delivery watchdog 以 dialog 無關的方式偵測這次漏接(成功落地的 inject 會觸發 `UserPromptSubmit` hook;被吞掉的則不會觸發任何 hook),並在 30 秒後恰好重投一次,接著警告並放棄 —— 絕不會形成重試風暴。它自我侷限在會發出 hook 的 backend(今天是 claude),所以不會發出該信號的 backend 永遠不會被誤判重投。

- **MCP 工具人體工學(#2037)** — `task list` 接受 `status` / `assignee` / `tag` 作為 filter 別名(對應文件記載的 `filter_*` 參數);`schedule list` 把每列的 `run_history` 修剪為最新 3 筆(附 `runs_total` 計數;`full_history=true` 可選擇還原);`task` 在任何 `id` 為規範名稱處都接受 `task_id`,`decision post` 接受 `text` 作為 `content`,並以教你別名用法的錯誤訊息提示;另外從 `send` 的 busy 選單移除了從未實作的 `queue=true` 選項。

- **已合併 PR 的派發任務自動關閉;`create_instance` 在 team 中保留明確名稱(#2037)** — 連結到已合併分支的派發任務現在可從任何 active 狀態自動關閉(原本僅限 `Verified`),終結每天「PR 已合併但任務仍開著」的殭屍;結構化的分支連結會關閉 active 工作,而鬆散的 title/description token 比對仍維持僅 `Verified` 才關閉,因此永遠無法關掉進行中的工作。另外,`create_instance` 同時給定 `name` 與 `team` 時,現在會以那個確切名稱 spawn,而非悄悄改名為 `<team>-N`。

- **Recovery telegram 通知限縮為已實際處理的阻塞(#2033)** — recovery 子系統在沒有採取任何動作的 recovery pass 上也會呼叫操作者的通道;此通知現在僅在確實處理了某個阻塞時才觸發(降噪,#2008 類別)。

- **Pane 內容不再浮在一條空白帶之上(#2046)** — 當一個 pane 的 VTerm/PTY grid 比其螢幕上的內容區短時,backend 的 footer / status line 會渲染在距離 pane 底部數列以上、下方留著空白列。Render 現在會在最後一哩步驟把 pane VTerm 與底層 PTY resize 到實際的內容 rect,使 `vterm rows == pane content rows`,無論走的是哪條 resize 路徑。

- **Inbox 在競爭下的 drain 可重試,而非假性為空(#2028)** — 碰到鎖競爭的 drain 會回傳一個與「沒有訊息」無法區分的空批次,因此單發呼叫者會把 inbox 當成已 drain 而丟掉 wake。競爭現在以一個獨特的 `Unavailable` 信號浮現,讓呼叫者重試。

- **daemon 重啟後的假 AwaitingOperator(#2020)** — 兩種重啟形態會把健康的 agent 強推進 `AwaitingOperator`:一個閒置、respawn 後恢復 session 的 opencode pane 不會渲染任何 `Ask anything` 佔位字(Idle pattern 永遠匹配不到 → `Starting` 滯留 → startup-stall fallback 觸發,實況 3 次),而一個忙碌、respawn 後立即注入工作的 agent 永遠不會渲染乾淨的 ready-prompt(在靜默窗內同樣的 fallback)。修復:opencode profile 在持久的 statusline 提示(`ctrl+p commands`)上新增一個最低優先序的 Idle pattern(working/error pattern 仍以 first-match 勝出 —— 已對完整 opencode replay-fixture 套件驗證),並且當 agent 自本次 spawn 以來已渲染過 backend 的 productive marker 時,否決 startup-stall 那一臂(真正的登入提示永遠不會渲染工具 chrome,所以 fallback 的本職得以保留;回顯的注入文字不算數)。

- **全新的 Telegram 設定在啟動時就能解析(#2005)** — quickstart 的 fleet.yaml 範本把舊的 `bot_token_env: AGEND_BOT_TOKEN` 釘住,而 `.env` 卻寫在規範的 `AGEND_TELEGRAM_BOT_TOKEN` 之下,且 credentials fallback 又重試同一個舊名稱 —— 因此全新安裝的 Telegram 通道在 daemon 啟動時解析失敗(舊安裝被殘留的舊 `.env` key 遮蔽)。範本現在釘住規範名稱,fallback 也對稱(設定的名稱 → 規範/舊兩者中的另一個),於是 fleet/.env 漂移的全部四種組合都能解析;仍使用舊名稱時會警告。

- **健康長時間工作上的 dispatch-idle 假警報(#2022, #2032)** — 「派發已沉默」的 watchdog 不再對緩慢但有進展的 agent 觸發。deadline 自動延長被收斂為單一「long-running — confirm expected」升級,而非每隔幾分鐘嘮叨;預設 idle 窗口從 10 → 30 分鐘以貼近真實任務長度;而且升級是分層的 —— 先通知 dispatcher,只有當派發在下一個窗口後仍未解決時,才中斷 agent 本身。

- **CI-watch 假風暴套組(#2001, #2013)** — CI-watch 現在每個 workflow 取最新一次嘗試的判定(rerun 不再讓陳舊的失敗鎖死),unwatch 會寫一個 tombstone,讓 PR-state 聚合器無法立即重新武裝同一個 watch,而 handoff track 以 head-aware 方式失效,使 force-push 後不再對死掉的 commit 重複 nudge。

- **Task board 撐過一行損毀的 event-log(#1992)** — `task-events.jsonl` 中單獨一行畸形不再讓整個 board replay 中止;壞掉的那行會被跳過並隔離,而一行向前不相容(較新 schema)的內容仍會 fail closed,讓舊 daemon 永遠不會對著一個它無法完整讀取的 board 動作。

- **損毀的內部 store 會被備份,而非悄悄覆寫(#2017)** — 當一個版本化的 store 檔案解析失敗時,daemon 會把它移到 `.corrupt` 備份,並在每次開機浮現一次該事件,而不是把它蓋掉。

- **Git shim 在非 fleet repo 中不再假裝成功(#2030)** — 由綁定的 agent 在 fleet 不管理的 repo 中執行的 `git branch <name>`(以及其他會命名 ref 的 `branch`/`tag` 形式)現在會直通到真正的 git,而非被悄悄重導向到 agent 的 worktree —— 後者先前會回 exit 0 卻什麼都沒建立,或從錯誤的 repo 回一個假的 `already exists`。

- **延後的通知在無頭 daemon 下不再擱淺(#1978)** — daemon 側的 per-tick flush 即使沒有 TUI 連接,也會投遞排隊的操作者通知。(功勞:@yujunchao)

- **背景服務繼承 `PATH`(#1984)** — `agend-terminal service install` 現在會把 `PATH` 傳播進 launchd/systemd 的服務環境,讓由服務啟動的 daemon 能找到 `git`、`gh` 與各 backend CLI。(功勞:@cheerc)

- **速率限制重試 nudge 需要螢幕上的信號(#1999)** — abort 之後的 ServerRateLimit 重試 nudge 現在僅在速率限制橫幅確實在螢幕上時才觸發,關閉了 agent 已恢復後的一次假重新 nudge。(功勞:@cheerc)

### Added

- **Context 滿載安全網(#2007, Plan A)** — daemon 監看每個 agent 的 context 用量(statusline pattern;今天僅限 claude —— 其他 backend 沒有被動信號,永不注入),在 85% 時注入一次 `[AGEND-AUTO kind=context-handoff]` nudge,告訴 agent 寫 `SESSION-HANDOFF.md` + 標註其任務。有噪音預算:每個 episode 一次注入(compact/restart 時以 hysteresis 重新武裝,絕不定時重複),在 92% 時若無 handoff 檔出現則一次可選的操作者升級,靜默自動解決,閒置的 agent 在 event log 中標記而非注入。閾值:`AGEND_CONTEXT_HANDOFF_PCT` / `AGEND_CONTEXT_HANDOFF_ESCALATE_PCT`。重啟仍由人/lead 驅動(Plan B 延後)。

- **`quickstart --unattended`(別名 `--yes`)** — 給 CI / 腳本化安裝用的非互動式設定:絕不讀 stdin(缺少輸入 = 清楚的錯誤 + 非零退出,而非卡住),絕不等待網路。Backend = PATH 上第一個偵測到的;Telegram 可選地來自 `AGEND_TELEGRAM_BOT_TOKEN` / `AGEND_TELEGRAM_GROUP_ID` 環境變數(token 未經驗證即儲存;daemon 在啟動時驗證),否則略過。冪等:既有的 fleet.yaml 永不覆寫;既有 `.env` 的 token 僅在有明確環境變數時才被替換。

- **fleet.yaml `schema_version` + 相容性政策(#1989)** — `fleet.yaml` 接受可選的 `schema_version:` 欄位(省略 = `1`;既有檔案不變,daemon 永不注入它)。宣告版本比 daemon 支援的更新的檔案會帶警告載入,而非毫無痕跡地被誤讀。`docs/COMPATIBILITY.md` 宣告 on-disk 介面層級 ——(a)穩定公開(fleet.yaml、service 範本、instruction block、MCP 設定)、(b)內部持久化狀態、(c)可重生/暫時 —— 以及對 (a)/(b) 的「僅可加性」變更規則。

- **Release pipeline 加固** — `release.yml` 新增一個 pre-release 的 `gate` job(version==tag、changelog 區段存在、MSRV 1.88 `cargo check`、`cargo-semver-checks` 對前一個 tag 做 soft-fail 報告),所有 artifact job 都依賴它;以及一個 `publish` job,在 GitHub Release 成功後自動發佈到 crates.io(先 `--dry-run`;`CRATES_IO_TOKEN` secret 未設定時優雅略過;對 `-rc.N` 預發佈 tag 絕不執行)。Release 流程記載於 `docs/RELEASING.md`。

- **內部 store 的 schema 版本化(#2000)** — `runtime-config.json`、`decisions.json` 以及每個 agent 的 `binding.json` 新增可選的 `schema_version`(省略 = `1`;既有檔案不變),因此宣告版本比 daemon 支援的更新的檔案會帶警告載入,而非被悄悄誤讀 —— 把 #1989 的相容性政策延伸到 (b) 層級的內部持久化狀態。

- **CONTRIBUTING Review Process + PR 相容性自我檢查(#2024)** — `CONTRIBUTING.md` 記載 search-first / RCA-first 的審查流程、VERIFIED-with-Evidence 標準、「註解與散文是主張,不是證據」規則、敏感區域的雙重審查,以及陳舊 PR 的接力(作者身分保留);PR 範本新增一項對照 `docs/COMPATIBILITY.md` on-disk-format 層級的自我檢查。

- **給外部操作者的事故 RUNBOOK(#2023)** — `docs/RUNBOOK.md` 新增以症狀為導向的恢復配方(daemon 健康、卡住的派發、lease 衝突、通道/通知問題),讓沒有 codebase 脈絡的操作者也能診斷並從已知的失敗形態中恢復。

### 移除
- **Gemini CLI backend 退役**（[#1580](https://github.com/suzuke/agend-terminal/issues/1580),完成 [#8](https://github.com/suzuke/agend-terminal/issues/8))。`gemini-cli` 於 2026-06-18 停止服務(免費/Pro/Ultra);其官方後繼者 Antigravity CLI(`agy`)自 [#1547](https://github.com/suzuke/agend-terminal/issues/1547) 起即為支援的 backend。`Backend::Gemini` 變體、其 preset/偵測 patterns、以及 8 個 gemini state-replay fixtures 皆已移除。**操作者注意:** `fleet.yaml` 內指定的 `gemini` / `gemini-cli` 不再解析為受管 backend,改以泛用 `Raw` backend 啟動;請改用 `agy`。移除最後一個 legacy backend 也讓 legacy 偵測骨幹(`compile_for`、`config_for_legacy`、`legacy_initial_state`)得以刪除——每個 backend 現在皆透過其同址的 `BackendProfile` 路由(#8 完成)。

### Changed

- **operator-mode 授權檔改為 fail-closed(#1576)** — daemon 啟動時僅在 `operator-mode.json` 存在且帶有效 HMAC 簽章時才信任它;檔案缺失、未簽章或遭竄改,會把 #1339 授權閘鎖進限制性的 **Away**,以阻止遭注入的 agent 偷寫 `{"mode":"active"}` 來停用該閘。此鎖定是**通道無關的** —— 在授權閘層強制執行,而非綁定任一 adapter —— 因此會壓制**所有** operator channel 的 operator 互動(目前的 Telegram;以 `--features discord` 編譯時的 Discord;未來的 Slack/Matrix adapter 亦同):從通道送進來的 operator 權威指令不會被當成 operator 處理,且 agent→operator 的通知會被拒絕/排入佇列,直到啟用模式為止。(唯讀與 agent 間的 fleet 協作永不受此閘限制。)**遷移:任何從 #1576 之前(2026-06-02 前)的版本升級上來的安裝,都沒有已簽章的檔,會以 Away 啟動 —— 不論你使用哪一種通道,升級後都請執行一次 `agend-terminal mode active`。** 之後該已簽章檔會跨重啟持續有效。注意:當磁碟上的檔不存在時,`mode get` 仍可能回報記憶體中的 last-known-good 模式,因此請確認磁碟上檔案是否真的存在,而非只看回報的模式。

- **retention sweep 解耦;移除 `AGEND_CTRLC_SENTINEL`(#1812 env-cleanup)** — decisions retention sweep 改讀自己的 opt-in 旗標 **`AGEND_RETENTION_DECISIONS_CUTOVER=1`**,與 pending-dispatch kill-switch `AGEND_RETENTION_CUTOVER` 分離(後者的另一消費者以相反極性讀取,導致「pending 關 + decisions 開」無法達成)。**遷移:** 舊的 `AGEND_RETENTION_CUTOVER=1` 暫時仍會啟用 decisions sweep(棄用緩衝期)—— 請改用新旗標。另外移除內部 Windows 除錯輔助 `AGEND_CTRLC_SENTINEL`(Ctrl+C 時寫 sentinel 檔):無 operator 用途、無自動化消費者。`AGEND_POINTER_ONLY_INJECT` 經檢視後**保留**(是 inbox 注入的實際功能旗標)。

- **重啟監督偵測:改用正向 `AGEND_SUPERVISED` sentinel,移除 `XPC_SERVICE_NAME`(#1812)** — `is_restart_supervised()`(`restart_daemon` 的 #851 fail-closed 防呆)不再信任 `XPC_SERVICE_NAME`。macOS 會把該變數注入 GUI 登入工作階段中的*每一個* process(包含在 Terminal.app 裡裸跑 `agend-terminal start`),導致 macOS 上此防呆永遠回 true,`restart_daemon` 可能 `exit(42)` 後沒有任何程序重啟 daemon。現在改以 `agend-terminal service install` 寫入 launchd plist(`EnvironmentVariables`)與 systemd unit(`Environment=`)的 `AGEND_SUPERVISED=1` 明確 sentinel 判斷;`AGEND_WRAPPED` 與 systemd 的 `INVOCATION_ID` 仍接受。**macOS/Linux 升級遷移:升級後請重跑 `agend-terminal service install`,再重啟 daemon 一次** —— 舊版安裝的 service 設定檔早於此 sentinel,在重新產生前 `restart_daemon` 會 fail-closed 並回傳可操作的錯誤訊息(這是安全方向 —— 拒絕而非把 daemon 卡死)。Windows Task Scheduler 無法攜帶此 sentinel(task XML 沒有環境變數元素),維持裸啟動 fail-closed。

## [0.7.0] — 2026-05-28

自 `0.6.1` 以來超過 200 個 commit，橫跨 Sprint 55–69（2026 年 5 月 7 日至 5 月 28 日）。三大主題：
**(1) Task board 可靠性** — 根因分析並預防 ghost-owner 問題（`teams::delete` 級聯刪除、啟動時孤兒掃描、`force` 旗標）+ 新的清理工具 + 營運者可見的健康快照；**(2) Bridge ↔ daemon 冪等重試** — 消除暫時性傳輸失敗下有副作用的 MCP 呼叫重複執行問題（UUID request_id + DedupCache + Condvar 阻塞等待）；**(3) MCP handler 重構 #694 完成** — 30+ 個工具分支從行內 `match` 遷移至 dispatch table，為工具註冊表熱重載 (#776) 鋪路。另有 Hung 偵測影子模式基礎設施（F9 Stage 1–3）、速率限制恢復自動提示，以及約 50 個較小的 bug 修復/加固 PR。

### 新增

- **`notify_system` helper (#1335)** — `crate::inbox::notify_system()` 封裝常見的 daemon 通知模式。七個 daemon 模組完成遷移，每個通知點從約 8 行減少到 1 行。
- **Event bus (#1336)** — 全域事件匯流排，透過 `AGEND_EVENT_BUS=1` 啟用。`event_bus::emit_lazy(kind, || payload)` 在停用時延遲序列化；停用路徑零成本（無配置、無序列化）。
- **`with_pr_state` flock helper (#1342)** — `pr_state::with_pr_state()` 和 `with_pr_state_or_create()` 透過 `fs4` 檔案鎖序列化所有 `pr-state/*.json` 的讀取-修改-寫入操作。消除 gh-poll 覆寫 scanner 的 `ready_emitted_for_sha` 旗標導致重複 `[pr-merged]` 通知的 lost-update race。全部 6 個生產環境 `save()` 呼叫點已遷移。
- **pr-merged 時自動釋放 worktree (#1344)** — Scanner 的 `MergeState::Merged` 分支現在在發出 `[pr-merged]` 前呼叫 `auto_release_for_merged_branch()`。防止 dev 的 worktree 仍持有本地分支時 `gh pr merge --delete-branch` 失敗。
- **`/setup-telegram` skill + `skill add` URL#subdir 支援 (#1351, PR #1354)** — 導引式 Telegram 設定的 per-channel 安裝 skill。`skill add <url>#<subdir>` 可克隆 skill repo 並安裝子目錄。
- **CI 程式碼覆蓋率（cargo-llvm-cov + Codecov, #686）** — CI 新增 `coverage` job，在 Ubuntu 上執行 `cargo llvm-cov` 並上傳至 Codecov。維護者專用基礎設施。
- **Bridge ↔ daemon 冪等重試 (#842, PR #843)** — bridge 為每個 JSON 封包產生 UUID v4 `request_id`；傳輸失敗重試時重用同一 id。新的 `DedupCache`（TTL 10 分鐘、64KB/條目、64MB 上限、Condvar 阻塞等待）快取已完成的回應並阻擋進行中的重複請求。
- **`task action=health` (#830, PR #838)** — 營運者自助式看板健康快照。回傳總數 + 按狀態統計 + ghost_owners + 過期 claims + 年齡分布 + 建議陣列。
- **`task action=sweep` (#806, PR #810)** — 營運者觸發的過期任務清理。4 個類別搭配預設 dry-run + `confirm_ids` + `audit_reason` 審計。
- **`repo action=cleanup_merged_branches` (#817, PR #820)** — 營運者觸發的本地分支清理。4 個類別，`min_age_days` 可設定（預設 90）。
- **`force_release_worktree` GC 模式 (#826, PR #837)** — 對已解散的 agent 額外掃描孤兒 git-level worktree metadata 並強制清理。
- **Daemon 啟動時孤兒擁有者掃描 (#829, PR #835)** — 啟動時掃描所有 assignee 不在 fleet registry 中的任務，嚴格模式自動設為孤兒。
- **`teams::delete` 級聯刪除 (#828, PR #834)** — 解散路徑現在遍歷團隊成員並呼叫 `full_delete_instance`，級聯至 `orphan_tasks_for_owner`。修復產生 ghost-owner 的根因。
- **`task force=true` 旗標 (#808, PR #809)** — 繞過 ACL 清理歷史遺留的 ghost-owned 任務。需要 `force_reason`（記入審計日誌）。
- **`cleanup_init_commits` trailer 感知 body 檢查 (#833, PR #839)** — `KNOWN_TRAILER_KEYS` 白名單剝離後再判斷 commit body 是否為空。daemon 產生的 heartbeat 現在正確分類為空 commit 並可清理。
- **Daemon 速率限制恢復自動提示 (#841, PR #844)** — 偵測到 `ServerRateLimit`/`RateLimit`/`ApiError` 狀態閒置後，自動注入單次恢復提示。`fleet.yaml` 可 per-instance 停用。
- **`ci_watch` 衝突 PR 偵測 (#813, PR #816)** — daemon 現在查詢 PR `mergeable` 狀態；若為 `CONFLICTING`/`DIRTY`，立即發出 `[ci-conflict-detected]` 警報。
- **Dispatch 測試名稱驗證 (#812, PR #815)** — daemon 驗證 send body 中的 `cargo test ... <test_name>` 是否存在於 PR HEAD tree。
- **通知去重 race 修復 (#836, PR #840)** — 基於 `msg_id` 的抑制機制，防止接收者命中速率限制時同一通知最多觸發 3 次。
- **MCP dispatch table 重構 — #694 完成** — 30+ 個工具分支遷移至 dispatch table，啟用未來工具註冊表熱重載。
- **Hung 偵測 F9 影子模式基礎設施 (#685)** — 生產力輸出閘道、fixture 語料庫、per-backend 標記、Stage 1-3 自動恢復/重啟/升級排程。僅影子模式。
- **時區顯示設定 (#790, PR #797)** — 透過 `fleet.yaml` 設定顯示時區（預設 Asia/Taipei）。

### 變更

- **comms.rs `request_id` 傳播 (#1341)** — `comms.rs` 中的 3 個 `api::call` 呼叫點現在包含 UUIDv4 `request_id`，啟用 daemon 的 `DedupCache` 去重。
- **`dispatch_idle` flock (#1340, PR #1347)** — `mark_resolved` 和 `scan_and_emit` 現在使用 flock 序列化，防止並行解析和掃描之間的 race condition。
- **`agy` backend (#987)** — Google Antigravity CLI 作為第六個一級 backend。因 Gemini CLI 2026-06-18 停止服務而新增。
- **`doctor topics` 分類從 4 類減少為 2 類 (#994)** — 移除 `drift_fleet` 和 `stale_registry`。僅保留 `live` 和 `orphan`。
- **`agy` backend 顯示名稱 + workspace-trust 自動關閉 + `fleet_mcp_supported: false` (#995)** — TUI 顯示 `antigravity-cli`；workspace-trust 提示自動關閉；MCP 設定寫入為 no-op。
- **`agend-terminal list --json` 封包 (#938)** — JSON 輸出現在用帶有 `mode` 欄位的 discriminated envelope 包裝 agent 陣列。
- **`AGEND_LOG` 優先級 (#927 PR-A)** — 環境變數設定值現在確實優先於程式內預設值。
- **`telegram_init` 非同步背景化 (#945 Phase 1)** — 冷啟動時間從約 6.6 秒降至約 0.5 秒。
- **CI-watch `correlation_id` 格式 (#946)** — 每個 `system:ci` inbox 通知現在攜帶 `correlation_id = "{repo}@{branch}"`。
- **`dispatch_idle` watchdog fallback `correlation_id` (#947)** — 合成的 fallback 使用規範的 `disp-<unix_micros>-<seq>` 格式。
- **`ci_watch` 檔案身分加固 (#942 / #943)** — watch 檔名現在使用 sha256。舊格式檔案在啟動時遷移。
- **`ci_watch` 跨 bind/release 移交存活 (#931)** — `release_worktree` 不再銷毀 watch 檔案。
- **`agend-terminal admin cleanup-zombies` (#927 PR-B)** — 終止持有過期 run 目錄的殭屍 daemon。
- **啟動掃描環境變數 (#933)** — `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` + `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN`。
- **執行緒狀態傾印環境變數 (#941)** — `AGEND_DAEMON_THREAD_DUMP_SECS`。
- **`.ready` 啟動完成信號 (#922)** — daemon 初始化完成的單一信號策略。
- **`bootstrap-step` 儀表化 (#945 Phase 0)** — daemon log 中的每步驟計時分解。
- **狀態偵測紅色 SGR 錨點 (#919 Phase A)** — `HIGH_FP` 模式現在要求 200 bytes 內出現紅色轉義序列。
- **Telegram inbound 長度分流 (#1352, PR #1353)** — 短訊息（<200 字元）僅 PTY，長訊息 inbox+hint。消除雙路徑重複投遞。
- **Replay cache mtime→generation counter (#1355, PR #1357)** — 使用單調遞增的 generation counter + mtime 四元組修復 mtime 碰撞導致的假快取命中。
- **`task action=list` 預設過濾 (#806)** — 預設只回傳可操作狀態；`include_history=true` 可查看 `done`/`cancelled`。
- **`task action=create` 回應格式 (#807, PR #811)** — 回傳完整 task 物件而非僅 `{id, status}`。

### 修復

- **過濾 pr_state scanner 的 `.lock` 檔案 (#1349, PR #1350)** — pr_state scanner 嘗試將 `.json.lock` sidecar 檔案解析為 JSON，導致每 10 秒 tick 產生 WARN 日誌。
- **TUI 滑鼠選取捲動凍結 (#1356, PR #1358)** — 活動選取期間新輸出到達時，選取座標會漂移。修復在 MouseDown 時快照 `max_scroll()`，並補償 grid 成長以將 viewport 固定在相同內容上。
- **`WIDE_CHAR_SPACER` ratatui buffer cell 洩漏 (#819, PR #823)** — 全形字元過渡到半形字元時，過期字元跨 frame 洩漏。
- **Bridge ↔ daemon 暫時性傳輸失敗下的重複執行 (PR #804 RCA → PR #843)** — bridge 的 `is_retriable_io` 將 `TimedOut` 分類為可重試，導致慢 handler 觸發重試和重複執行。根本修復是 L1 冪等重試架構。
- **`teams::delete` 未清理成員任務 (#828)** — 遺漏的 `full_delete_instance` 呼叫是所有 ghost-owner 累積的根因。
- **`force_release_worktree` 未清理 git-level metadata (#826)** — binding 已移除時未檢查 git-level worktree。新增 GC 模式。
- **生產環境 `task action=list` 回傳 500KB+ (#806)** — 見「變更」中的預設過濾。
- **`cleanup_init_commits` 對 daemon 產生的 heartbeat 無效 (#833)** — 見「新增」中的 trailer 感知 body 檢查。
- **CI 審計 job 自 2026-05-15 起在 main 上持續失敗 (PR #831)** — `permissions: checks: write + issues: write` 修復。
- **通知在消費+重試時三次觸發 (#836)** — 見「新增」中的通知去重。
- **WAF-stage / macOS rustup-init 暫時性問題 (#772 v3, PR #800)** — `cache-bin: false` 防止過期 rustup-init 汙染。
- **PTY 寫入超時防止鏈路死鎖 (#659, PR #679)** — `PTY_WRITE_TIMEOUT = 5s`。
- **`kill_process_tree` PID 0 防護 (#681, PR #687)** — 拒絕向 PID 0 發送信號。
- **`flock` 保護 ci-watch RMW race (#692, PR #731)** — 防止兩個 daemon tick 互相覆寫。

### 移除

- **`requires_daemon_state` 欄位 (#672 / #674)** — 已移除無用欄位。

### 建置/依賴

- **`sysinfo` 0.32 → 0.39** — API 遷移。
- **`rustls-webpki` 升級** — 安全公告修復。
- **`twilight-model` 0.16 → 0.17.1**（Discord channel 依賴）。
- **`libc`、`uuid`、`which`** — 例行 dependabot 升級。

### 測試基礎設施

- **`admin::cleanup_zombies::poll_until_dead` (#934)** — 確定性輪詢原語，取代基於 sleep 的等待。
- **`api::handlers::instance::await_sentinel_nonempty` (#949)** — 重新命名以釐清合約：等待檔案有內容，而非僅存在。