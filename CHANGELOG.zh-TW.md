[English](CHANGELOG.md)

# 更新日誌

本文件記錄本專案所有重要變更。
格式基於 [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)；專案遵循 [SemVer](https://semver.org/spec/v2.0.0.html)。

## [Unreleased]

## [0.10.0] — 2026-07-07

自 0.9.0 起 311 個 commit。以下依主題整理重點(非完整 commit 清單)。

### Added

- **Discord 成為一等公民 operator 頻道(#2562、#2586–#2592)** — Discord 加入 Telegram,成為即時的 operator 頻道。先前的工作已交付 REST / gateway 解析 / binding 骨架,但沒有任何連線真正打開;現在 gateway WebSocket + `gateway_event_to_channel_event` 轉譯(#2564)已接上真實的 daemon/app 啟動流程,使 `fleet.yaml` 裡的 `ChannelConfig::Discord` 建構出即時頻道而非 no-op(#2567),搭配把 `poll_event()` 導入 agent inbox 的 inbound dispatcher(#2587)、write-side per-instance binding(#2588),以及一個由 gateway death-status flag 把關的有界重啟 supervisor(#2590、#2592)。背後由完整的 inbound/outbound REST + reconnect-backoff 測試矩陣支撐,現已納入主 CI(#2625、#2628、#2629、#2649)。
- **Shadow Observer — 不依賴 hook 的活動推斷(#2413、#1523)** — 全新的 out-of-band pipeline 在不依賴 tool-call hook 下推斷 agent 活動:以 lsof 為基礎的 API-activity probe(#2426)、帶 Evidence + quantification 的本地 hook-plane reducer(#2433),以及泛化的 `{Hook|Stream}` reducer 來源,涵蓋 codex rollout-tail(#2437)、opencode SSE(#2440)、kiro session-tail(#2447)與 agy lifecycle-hooks plane(#2448)。已畢業為 kill-switch 背後的預設 ON(#2449)、併入 confidence gate 把關的 pane badge(#2456),並延伸以驅動 operated-state(#2457)。`#1523` turn-completion sentinel 的 shadow 量測現在也涵蓋 `claude` backend(#2366,僅量測、預設 OFF)。
- **多頻道 binding + topic 補強(#991、#2642)** — `bind_topic` MCP action 把 topic 追加到先前被延後(deferred)的 instance 上(#2598);deployment / team-mode 樣板端到端傳遞 `topic_binding`(#2600);`list_instances` 公開 `topic_binding_mode`(#2606);`FleetConfig::configured_channels()` 提供 canonical 的多頻道視圖(#2643),Telegram 與 Discord 都透過它一起註冊(#2646)。
- **臨時 one-shot worker(#1967 Phase 1)** — headless、不進 roster 的 PTY worker,生成、跑完一個 turn 就 reap:帶 day-1 cost guards 的 tracking-store 骨架(#2401)、搭配 admission-before-spawn 的真實 headless spawn(#2402)、group-kill reap(#2405),以及一個 one-shot driver(inject → idle-debounce turn-end → capture → oracle),先在 opencode 上線(#2407)再延伸到 claude(#2408)。
- **per-role MCP capability registry(#2300、#2344、#2367)** — 新增 per-role MCP capability registry(#2344),加上 `fleet.yaml` 中型別化、由 operator 宣告的 `role_kind`(七種變體)驅動它(#2367),裁切 agent 對外公告的工具面。唯讀角色(reviewer/planner/explorer)現在拿到 report/read 子集;exhaustive match 強制每個新角色都要做出明確決定。opt-in、預設全工具。
- **三態 inbox 投遞(#2345、#2299)** — 訊息投遞改為 `unread → delivering → processed`,搭配顯式 `inbox ack` 與 reclaim-TTL:收件者 turn 在 drain 之後死掉的訊息會被重新投遞(at-least-once),不再靜默遺失。
- **Fugu / Codex model-provider 支援(#2484–#2492、#2505)** — 一鍵 Fugu quick-spawn(#2487),搭配 model-provider descriptor(#2488)與自動偵測(#2486);Fugu 以 Codex「Sakana」backend 呈現(#2490);fleet model-tier policy(#2484);以及 provision 真正的 Codex profile 而非隔離的 `CODEX_HOME`(#2505)。
- **Fleet 決策與觀察 plane(#2524)** — 進入 `in_progress` 前要求 pre-work alignment 的 plan-ack gate(#2529)、status line 上的被動 decision badge + per-pane marker(#2530)、decision-board timeout + 預設處理(#2531),以及涵蓋臨時 worker 的 observation-plane 覆蓋(#2532、#2535)。
- **Ralph-loop self-continuation + discharge ledger(#2524、#2622)** — 帶硬性 `max_iterations` 上限的 self-continuation loop(#2543);以及接上 inbox-reclaim 與 `poll_reminder` 兩個 chokepoint 的 channel-reply / notification discharge ledger(#2541、#2644),讓真正被吸收(absorb)的訊息能抑制 re-nudge(#2545),前端為 `inbox action=discharge` 與可指定對象的 `reply message_id`(#2647、#2648)。
- **Off-thread 渲染 pipeline(Option X — `AGEND_OFFTHREAD_PARSE`,預設 OFF)** — per-pane 解析現在可以跑在主執行緒之外(#2404),並補上正確性後續:pane 捲動(#2411)、基於 snapshot 的複製選取(#2414)、滑鼠轉發(#2417),以及 zoom/resize 路由(#2419、#2420)。
- **TUI:command palette、圖片貼上、pane 淡化** — `Ctrl+B :` command-palette 自動完成,涵蓋指令探索(#2381)與參數值(#2387);`Ctrl+B i` 把剪貼簿圖片——包含透過真實檔案內容讀取而來的 Finder 檔案複製——貼入 agent 輸入(#2435、#2443、#2446);非聚焦的 pane 預設以 RGB 混色朝背景淡化,方便追蹤焦點(#2444)。
- **Creator-scoped `delete_instance` ACL(#2552)** — instance 的建立者現在可以刪除自己 spawn 的實例(先前僅限 orchestrator),當目標有 in-flight 工作時,由 `force` + `force_reason` valve 把關。

### Changed

- **MCP tool-surface 整併(#2548)** — 多波縮減 MCP surface:移除死工具(#2554);`replace_instance` 退役,併入 `restart_instance mode=fresh`(#2556);`set_display_name` / `set_description` 併入 `set_metadata`(#2557);五個工具移到 CLI 並收窄設定(#2560);`mode` 折入 `list_instances`,`force_release_worktree` 併入 `release_worktree(force:true)`(#2561)——在 v0.10.0 tag 時讓公告的 surface 收斂到 29 個工具。
- **Notification-watchdog handler 整併(#2549)** — poll-reminder + inbox-stuck + handoff-timeout 併成一個 handler(#2572);context-alert + context-handoff 併入 `ContextThresholdsHandler`(#2577);四個 GC tick handler 折成一個 `HourlyGcHandler`,改成 per-sweep(而非 per-handler)的 panic isolation(#2568);`ProgressBackstop` / `Mirror` 退役,handler 數量從 40 降到 37(#2571)。
- **Binding / GC / agend-git 收斂(#2550)** — 一輪廣泛整併,把 `binding.json` 路徑建構、scan-all 讀取與 `bind_self` read-back 統一到共用 helper(#2583–#2585);把 GC 折成單一 fire-on-first driver(#2599、#2605);把 `cleanup_merged_branch` 的刪除 gate 收斂到 `is_squash_gc_eligible`(#2597);並抽出帶 protected-refs 收斂的 `agend-git` classify predicate(#2580)。
- **agend-git policy engine(#2379)** — git 操作被分類為 deny / warn / info,衝突會路由到 `fleet_events`(#2462);fail-closed 的 protected-ref push deny,可用 `policy.toml` override(#2468);以及對 `$AGEND_HOME` config/audit blob 寫入的 push-shim deny(#2390)。
- **per-tick / render 熱路徑效能稽核(#2348–#2356、#2388–#2400)** — 一批零行為改變的加速:render redraw-scan 節流與 per-pane snapshot-scratch 重用(#2348、#2351、#2354)、PTY-reader pre-hash gate 在未變更的 frame 跳過 colour-mask 建構(#2349)、inbox unread-count 改用 cheap probe(#2350)、`FleetConfig` mtime-cache Arc 共享(#2356)、monitor proc-table 重用(#2355)、唯讀 MCP 工具跳過 usage-append / heartbeat RMW(#2388),以及 task-events compaction hysteresis(#2389、#2400)。
- **CI handoff 與 inbox 通知可靠性** — 降低 CI source-resolution 的 API loopback(#2495);report auto-close 改走 task project board(#2501);同一 head 上 terminal PR 的重複 ci-ready handoff 被抑制(#2504);修掉會靜默丟棄低 id rerun-to-green 轉換的 per-workflow notify cursor(#2520);以及 pr-state FYI 通知在 drain 時自動 ack(#2508、#2493)。
- **Team / project metadata 正確性** — 對已存在的 branch 也轉發 `repository_path` 而非丟棄(#2525、#2551);顯式的 `project_id` override,讓脆弱的 `source_repo` slug 推導不會誤導 team 路由(#2509、#2522);`update-add-member` / `project_id` 被釘住以撐過 daemon 重啟(#2565)。
- **內部簡化與衛生** — `#2050` byte-identical 簡化系列(dispatch / mcp / overlay / inbox helper + 死碼移除,#2357–#2365);`AgentState::Thinking` / `ToolUse` 併成單一 `Active`(#2461);`quickstart.rs` 從 2040 縮到 1100 LOC(#2611);`discord.rs` 拆成 per-concern 子模組(#2619);anti-monolith 檔案大小 ratchet + MCP-docs-vs-registry drift guard(#2483);外加例行依賴升級,包含 `quinn-proto` 的 RUSTSEC-2026-0185 安全修補(#2416)與 `quick-xml` 0.39 → 0.41(移除兩條 RUSTSEC ignore)(#2651)。

### Fixed

- **Canonical worktree 刪除事故——根因 + 偵測(#2668、#2669)** — 2026-07-06 的 canonical repo 刪除事故追蹤到 `repo release` 執行了 `git worktree remove --force`(git 對 main tree 會拒絕),接著 fall through 到無條件的 `remove_dir_all`。`validate_release_path` 現在以 git 本身(而非檔案系統啟發式)作為 source of truth,拒絕任何 primary / main working tree,使 bare repo 與 `--separate-git-dir` 的 main 都無法漏網(#2668);canonical heartbeat + `binding_state` liveness check 讓未來任何 canonical 遺失都會立刻示警,而不是靜默 40 分鐘(#2669)。
- **四道資料遺失防線(#2672、#2673、#2677、#2679)** — `bind_self` 重新 provisioning 不再讓既有的 `origin/<branch>` 變成孤兒(#2673);兩條手動 worktree-release 路徑都會把 dirty WIP 存成 recovery ref 的 snapshot,而非無條件丟棄(#2672);`agend-git` shim 對 feature-branch 的 force-push 要求 `--force-with-lease`,堵上任何 agent 都能靜默覆蓋別的 agent origin branch 的 footgun(#2677);以及收尾中的 task 會結清自己的 `dispatch_tracking` 列(不只是 `dispatch_idle` sidecar),讓 `sweep_stuck` 不再對已關閉的工作發牢騷(#2679)。
- **安全與可靠性稽核清掃——17 個根因修復(#2510)** — 把 `ci-watch` forge token 限制在可信主機的 SSRF / token-exfiltration gate(AUDIT2-001)、對破壞性工具的 per-caller ACL(AUDIT2-002/003)、擋下 cron DST fall-back 的 re-fire 風暴(AUDIT2-010)、atomic + locked 的 runtime-config 寫入(AUDIT2-012),以及 panic-isolated 的 crash-event dispatch(AUDIT2-007),外加零星的 TUI / tasks / skills / durability 修復。
- **Render-loop 凍結系列** — 重啟時把 OWNED 還原 spawn 延後到 render-first 背景池(#2343);redraw 上限訂在 30fps 並合併 wakeup(#2346);lock-free 發布的 `AgentState` mirror,終結 `core.lock` 爭用造成的凍結(#2380);`drain_output` 改成每 frame 有上限,阻止 PTY-backlog 畫面凍結(#2385);以及把重啟風暴吸收進有界的 boot / loading 階段(#2396)。
- **Rate-limit / server-overload 恢復,續集** — self-clear latch 在全新的 `ApiError` hook 上重置(#2415,接續 #2318 / #2319);過期的 `ServerRateLimit` badge 會讓位給 SRL 之後的新 hook 活動(#2470、#2471);以及一次 spike 量化出 agent 幾乎從不會自清 rate-limit block(2746 / 2746 筆 shadow 紀錄顯示 `self_cleared=false`),因此移除了死掉的 `rate_limit_self_cleared` 訊號與無效的 self-clear 指示——真正的恢復現在改走 hook-authoritative 路徑(#2674、#2675)。
- **Worktree / binding / dispatch race 完整性** — 型別化的 `LeaseError` 取代原本可能靜默回傳 `Ok` 但缺 binding 的 lease(#2464);安全的 same-agent rebind 修復取代了破壞性的 fallback(#2496、#2523);過期的 dispatch-checkout binding 現在 fail closed(#2500);push guard 改解析真正的 default branch,而非寫死 `origin/main`(#2662);`HourlyGc` 與 worktree-registry sweep 移出 daemon 的 main tick loop(#2614、#2616);以及堵上三類 worktree-reclaim 的偽陽性(#2657)。
- **Dirty canonical main working-tree guardrail(#2512)** — 一個未追蹤的散落檔案可以繞過所有既有的防護落進 canonical main working tree(agend-git shim 只看得到 git 指令;drift check 只看 HEAD 狀態;`.gitignore` 只比對完全相同的檔名);新的 L1 + L2 偵測器(`apply_to_canonical` → `Option<CanonicalDirtyReport>`)補上這個確切的缺口。
- **Health / respawn 硬化(#2480、#2538)** — decay transition 與 `respawn_ok` 現在都強制 process-liveness gate;crash-respawn 失敗會 escalate 而非靜默卡死;backend-exit 的 foreground-identity 偵測會發出 `backend_exited` 通知 + Unhealthy transition(#2546)。
- **PTY write-actor race 與 leak 硬化** — 用 per-writer thread isolation 修掉 fd-reuse race(#2620、#2630);把 idle busy-poll 換成 park-on-empty-queue(#2656);新增 fd-leak 回歸測試 + ConPTY sideload 稽核(#2613)。
- **Inbox 投遞可靠性(#2299)** — 還原過期的 `delivering` 列時 poll-reminder 會重新武裝(#2362);`mark_ci_watch_superseded` 改以 `correlation_id` 相等比對,而非文字子字串(#2370);poll-reminder 只計算真正的義務,不算已 drain 的 report(#2412);`DELIVERING` 在 session-reset 時透過 `ack_inbox` 送出並結清(#2425)。
- **dispatch_idle 殭屍 / quota-wedge 訊息(#2676、#2678)** — 回收 rate-limited agent 時現在也會清掉其 `dispatch_idle` sidecar,讓已釋放的 reviewer / query dispatch 不再發出過期的 watchdog 警報(#2676);quota-wedge escalation 現在持久地 one-shot,不再因 snapshot flicker 而自清——先前這曾導致相隔幾分鐘的重複觸發(#2678)。
- **Bug-audit 硬化(#2368、#2372)** — RAII connection-slot guard,讓 panic 的 `handle_session` 不會把計數器洩漏到 32-slot 上限而鎖死控制面(#2368);`validate_args` 把「存在但為 JSON null」的必填欄位視為缺漏並拒絕,而非靜默轉發空字串(#2372)。
- **Auth 與授權(#2369、#2378)** — Telegram 的 allowlist 檢查現在跑在 fleet-status 注入與 `加 task:` 板寫入之前(#2369);已審核的 authz 內容不再被誤標為 `AuthError`(#2378)。
- **Telegram 健壯性** — quote-reply 對應現在透過持久的 sent-message ledger 精確比對,而非 best-effort matching(#2570);TUI pane create / kill hook 在一次 post-#945 的 channel-ref 回歸後重新觸發(#2591);topic-reuse 的身分混淆漏洞已補上(#2593)。
- **send 驗證誤拒(#2681)** — reviewer 的 SHA-staleness gate 只在 `summary` 裡掃 PR URL,而對應的 evidence gate 卻掃 `summary + artifacts`,導致 URL 放在 `artifacts` 裡的 verdict 被誤拒進 fallback-inject 路徑;兩個 gate 現在共用同一個掃描面,無 URL 的拒絕訊息也改為一次到位可行動。
- **e2e local-precondition 摩擦(#2680)** — 兩個 real-daemon e2e 測試在本地 auto-bind 不可用時改為 skip 而非 fail,讓缺少的本地前置條件不再誤讀為假的 CI 失敗。
- **GR1 綁定劫持可觀測性(#2158、#2341、#2361、#2373)** — self-claim 綁定(無 task dispatch)現在會向 operator 顯示,且不再靜默 auto-arm ci-watch(#2341);該通知改寄到被綁 agent 的 team orchestrator,而非全域 operator inbox(#2361);per-branch fire-once latch 在 release 時清除,之後同分支的真正 re-claim 會重新顯示(#2373)。僅偵測——push 權限仍由 HMAC + guard 把關。
- **Split-pane 與 local-shell 還原(#2359、#2360)** — operator 開的本地 / scratch shell(`Ctrl+B c`)豁免於 #1441 的 unmanaged-spawn gate(#2359);還原的 split pane 會在 deferred attach 之後重新調整到其 content rect,不再卡在錯誤尺寸(#2360)。

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
- **Protocol 文件重新命名（歷史路徑 `docs/FLEET-DEV-PROTOCOL-v1.md` → 現行 `docs/FLEET-DEV-PROTOCOL.md`）**——移除 `-v1` 後綴。文件標頭內已有版本號（`v1.2`），也沒有進行中的 v2；路徑上的 `-v1` 是 2025 年留下的誤稱，容易讓人以為另有平行的 v2。`src/protocol.rs` 的 compile-time `include_str!`、`Cargo.toml` 的 `[package].include` whitelist、`tests/cargo_include_invariant.rs` 的 mock-pattern filter、README 連結，以及現已退役的 architecture quick-start 與 lint-discipline 紀錄均已更新；退役文件收錄於[歷史紀錄](docs/README.zh-TW.md)。手動覆寫舊路徑的 operator 必須自行重新命名（不自動遷移；這項機制很少使用）。

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

### 工作流程驗證 snapshot

- **2026-05-14 post-#779 partial-fix canary 通過**——距離完全不需 bypass 的流程仍有 1 個手動 git branch 步驟。記錄於 `/tmp/val-workflow-2026-05-14.md`（post-mortem reference）。

## [Workflow validation 2 — 2026-05-14] post #779 partial-fix canary pass（1 個手動 git branch 步驟）

## [0.6.1] - 2026-05-10

### 移除

- **`agend-terminal mcp` subcommand（Sprint 56 Track I，#531）**——local-mode stdio JSON-RPC server 退役。`Commands::Mcp` enum variant、`mcp::run` function、ACL machinery、framing helper 與 `proxy_or_local` fallback 全數從 `src/` 刪除。手動編輯 mcp.json 的 operator 會在下次啟動時由 daemon atomic upsert 將 config 改寫為使用 `agend-mcp-bridge`；新安裝會在 release artifact 中附帶 bridge（Phase 2a，v0.7+）。此後 bridge 是 canonical MCP server。問題由 changhansung 在 Windows 11 + kiro-cli backend 上回報，並經過 4 個連續 PR 調查（Phase 1 RCA / 2a packaging / 2b deprecation / 2c hard removal）。已退役 RCA 與完整架構推理收錄於[歷史紀錄](docs/README.zh-TW.md)。
- **`ensure_gitignore` worktree helper（#602、#604）**——`src/worktree.rs::ensure_gitignore` 會將 `.worktrees` 自動注入 project `.gitignore`，作為 Sprint-57-Wave-4 之前 layout 的向後相容後援。Wave 4 之後的 worktree 位於 repo 外（`$AGEND_HOME/worktrees/` 下），此注入因此多餘且會污染 user `.gitignore`。已移除 callsite、helper 與 obsolete test assert（-42 LOC）。由 @cheerc 回報。

### 新增

- **Bridge runtime invariant（Sprint 56 Track I-Phase2c，#531）**——新增 `tests/no_local_mcp_mode_invariant.rs::bridge_emits_daemon_error_when_daemon_down`：在沒有 daemon 執行的 clean home 中 spawn `agend-mcp-bridge`，並斷言 stdout/stderr 會出現 daemon-related error。這固定移除後的 contract：bridge 沒有可靜默降級的 local-handler fallback path。

## [0.6.0] — 2026-05-07

自 `0.5.0` 以來超過 50 個 commit，涵蓋 Sprint 53（`agend-git-shim` Phase 1–5 + production wiring）與 Sprint 54（`ci_watch` reliability overhaul + adaptive backoff + agent-visible health surface）。本 release 有兩大主題：multi-agent git isolation 獲得自己的 enforcement layer，而 agent 的 CI feedback 也足夠可靠，讓 operator 能信任 polling loop。

### 新增

- **`agend-git-shim`（Sprint 53）**——在 agent 與 `git` 之間加入五階段 shim layer。Phase 1：`prepare-commit-msg` hook 自動附加 `Agend-Agent`、`Agend-Branch`、`Agend-Issued-At`、`Agend-Task` trailer（具 idempotency，存在時略過）（#446）。Phase 2：`$AGEND_HOME/bin/git` 的 shim binary，deny matrix 涵蓋 `worktree add/remove/move`、跨分支 `checkout` 與 unbound-context operation；合法 operator override 可用 `AGEND_GIT_BYPASS=1` bypass（#447）。Phase 3：per-agent worktree lease/release lifecycle，帶有 `.agend-managed` marker（#449）。Phase 4：每小時 GC dry-run sweep 會標示 stale worktree 而不移除——operator-driven cutover 延後（#454）。Phase 5：hotspot detection telemetry，供後續調校（#455）。Windows 在範圍內（#448）。
- **Sprint 53 production wiring**——修復 §1.4 hard learning 中「Phase 1–5 已交付 binary 卻沒有 caller」的缺口。P0-1 dispatch hook 在含 branch field 的 `delegate_task` 上自動 bind 與 lease（#464）。P0-1.5 central lease registry 拒絕跨 agent branch claim conflict（#465）。P0-1.6 重用既有 checkout 前會驗證實際 HEAD（#466）。P0-2 將 `watch_ci` 接到 dispatch hook（整併 Hotfix C）（#467）。P0-3 anti-pattern CI lint gate 強制測試必須呼叫 production path `dispatch_auto_bind_lease`（#471）。P0-X `release_worktree` MCP tool——binding + worktree cleanup 的唯一真相來源，取代 ad-hoc `binding::unbind` call（#470）。P1-4 `gc_dry_run` MCP tool 將 Phase 4 GC finding 顯示給 operator（#479）。
- **`ci_watch` multi-caller fan-out（Sprint 54 P0-1）**——`ci watch` MCP action 現在附加至 `subscribers` array，不再 last-write-wins overwrite。無論 subscriber 數量，每 cycle 只 poll 一次；terminal classification 會 fan out 給所有 subscriber（不會 shadow-drop）。Schema 將 legacy `instance: "X"` migration 為 `subscribers: [{instance, subscribed_at}]`，並保留 legacy field read fallback。`ci unwatch` 只移除 caller；subscriber 清空時才刪除 watch file。（#484，關閉 `d-20260506155323776106-0`）
- **`ci_watch` adaptive backoff（Sprint 54 P0-2）**——依剩餘 quota 使用三區 curve：healthy（>50% remaining）使用設定 interval，cautious（10–50%）放寬 2×，critical（≤10%）放寬 4×。下限是 baseline，上限是 baseline×4。GitHub provider 在每次成功 response 解析 `X-RateLimit-Remaining` / `X-RateLimit-Limit`；GitLab + Bitbucket 發出 `None`（維持 baseline behavior）。Watch JSON 新增 `rate_limit_remaining` / `rate_limit_limit` / `effective_interval_secs` diagnostic field。從 rate-limit-until reset 的 recovery path 與 Sprint 53（Hotfix F）相同。（#490）
- **GitHub token auto-detect（Sprint 54 P0-4）**——daemon 依 `GITHUB_TOKEN` env → `gh auth token` → unauthenticated fallback 的順序解析 auth。結果快取在 process-wide `OnceLock`；絕不寫回 env（避免污染 child PTY）。若兩個來源都無 token，`ci watch` / `ci status` MCP response 會包含 canonical `setup_warning` field 與可操作文字。Daemon restart 會重新 discovery；涵蓋 daemon 已執行後才做 `gh auth login` 的情況。（#487，關閉 `d-20260506171309264856-1`）
- **Agent-visible CI health surface（Sprint 54 P0-5）**——`ci watch` response 新增 `rate_limit_active` / `rate_limit_until` / `next_poll_eta` health field。Daemon 在連續 3 次因 rate limit 略過後 fan out `[ci-watch-stalled]` inbox event（每個 stall window 恰好一次），並在第一次成功 poll 後 fan out `[ci-watch-resumed]`；兩種 event 都依 P0-1 fan-out contract 送給每個 subscriber。新的 `ci status` MCP action 回傳 caller-scoped 的 16-field health snapshot，並支援 optional `repo` / `branch` filter。（#492）
- **Sprint 54 P1-5——`cleanup_deployment_dirs` rmdir 空 parent**——完成 per-member cleanup 後，best-effort 對 deployment-directory parent 執行 `remove_dir`（非 recursive）；若 operator 放入檔案，non-empty error path 會保留它們。（#489）
- **Sprint 54 P1-7——`bind_self` MCP tool**——agent 不必經外部 dispatch，就能自行 bind 到具名 branch 的 fresh worktree。它重用 dispatch-hook lifecycle，讓 `binding.json` + worktree + `.agend-managed` marker + auto `watch_ci` registration 全都走同一 code path。拒絕 `main` / `master`（E4.5）與 cross-agent branch conflict。搭配 `release_worktree` unbind。解決 agent 需要 worktree、卻沒有可委派來源的 recovery case。（#493）

### 變更

- **Worktree lifecycle 由 daemon 管理**——agent 不再直接呼叫 `git worktree add/remove`。Dispatch 時 auto bind（P0-1）、透過 Phase 1 trailer 建立 audit trail，並以 `release_worktree` MCP tool（P0-X）離開。Crashed agent、stale dispatch 與 abandoned branch 會進入 daemon GC queue，不再成為 orphan filesystem entry。
- **CI watch architecture 拆分**——`ci_watch` tick loop 將 polling（每 cycle 一個 HTTP request，擁有 rate-limit + adaptive backoff + watch-state persistence）與 notification fan-out（terminal classification 後每 subscriber enqueue 一個 inbox）分離。過去 last-write-wins 的 multi-caller flow 現在可正確組合。（#484）
- **`watch_ci` MCP response shape**——`warning` field 改名為 canonical `setup_warning`（Sprint 54 P0-4）；新增 `subscribers` / `rate_limit_active` / `next_poll_eta` health field（P0-5）。Pre-Sprint-54 daemon 讀取 post-Sprint-54 watch JSON 時，仍可在一個 release cycle 內看到 legacy `instance` alias。
- **預設 PR open mode 為 `ready`**——implementer 不再預設將 PR 開為 draft。`--draft` 僅保留給不會 merge 的 smoke / verification PR、明確 WIP，以及 external-PR augmentation。Draft 不會出現在 GitHub UI 預設 filter；default-ready 讓 review pipeline 保持可見。（#491）

### 修復

- **`comms.rs` 在 `kind=report` reply path 自動 unbind（CRITICAL）**——每個 `kind=report` reply 都會呼叫 `binding::unbind`，即使 task 尚在進行也會清除 agent → branch binding。修復 cascade 後：Phase 1 trailer 正確觸發、orphan worktree 不再累積、P0-X release_worktree 不再是 no-op、Phase 4 GC 不再把合法 live binding 誤標為 suspect。`release_worktree` 的 single-mutation-point invariant 現在是關鍵保障。（#477）
- **TUI close path 略過 deployment teardown（#474）**——tab/pane 上的 `Ctrl+B x` close 會 bypass `cleanup_deployment_dirs`，使 custom-directory subdir 跨 daemon restart 洩漏。Close path 現在對每個 pane 執行 `full_delete_instance` + `reconcile_after_close`。（#475、#481）
- **`ci_watch` malformed head query（Hotfix F gap）**——Hotfix F（#461）修復了 `closed_at` freshness gap，但底層 GitHub query 仍錯誤：`head={branch}`（沒有 owner prefix）會被 GitHub API filter 靜默丟棄，因此 response 回傳的是整個 repo 最近 merge 的 PR，而非 watched branch。與 closed_at freshness 結合後，會在 in-flight PR watch 上出現 false-positive auto-clear。修正改用文件化的 `head={owner}:{branch}` 形式，並加入 defensive `head.ref` mismatch guard；response 不符時回傳 `Unknown`。已擷取 empirical regression proof（將 URL 改回裸 `head=` → owner-prefix test panic）。（#498）
- **`ci_watch` fresh-branch classification 修正（Hotfix F）**——daemon 因未檢查 `closed_at` freshness，而將 fresh-no-PR branch 自動清除為 `merged=true`。此狀態的 PR 現在分類為 `pending` 並繼續 polling。修正規則為 `closed_at > 1h ago = stale, not auto-clear`。（#461）
- **`ci_watch` no-PR-yet false positive（Hotfix E）**——沒有對應 PR 的 branch 被分類為 terminal，導致 notification 被丟棄。新增 60 秒 grace period + closed_at freshness check。（#458）
- **`agend-git-shim` 缺少 app-mode wiring（Hotfix D）**——`app::run_app`（CLI）未初始化 shim init function，使 Phase 1–5 operation 在 user-facing CLI 中成為 dead code。Init seam 移至 `bootstrap::prepare`，讓 daemon 與 app path 都涵蓋 wiring。（#457）
- **Dispatch 上的 `watch_ci` auto-watch（Hotfix C）**——含 branch field 的 `delegate_task` 不會自動建立 `ci-watch` registration。已明確接線；之後整併進 Sprint 53 P0-2。（#451、#467）
- **Server rate-limit retry 儲存 raw body（Hotfix A/B）**——retry loop 在 attempt 之間遺失原始 429 body，遮蔽真正 error message。現在會儲存 raw body 並於 inject 時 replay。Provenance side-channel message 會截短至 Telegram 長度限制，以防 oversize message drop。（#436、#452、#453）
- **Issue #456 deployment teardown cleanup gap**——`deployment teardown` 清除 deployment record，卻留下 workspace + config + channel topic registry。現在完整清理三者（workspace + config + registry）。（#459）
- **Issue #468——Gemini dismiss pattern substring 誤中 scrollback**——`try_dismiss_dialog` regex 會匹配 scrollback buffer 內的 dialog text，觸發 spurious dismissal。改用有 bounded prefix character class 的 anchored regex。（#469、#472）
- **`reply` MCP `no active channel` silent fallback（#488——第一個社群回報 issue）**——即使 Telegram message 有效，`reply` 仍持續回傳 `no active channel`。根因：MCP subprocess 無法連到 daemon，並靜默 fallback 至缺少 `ACTIVE_CHANNEL` registration 的 local handler，因而顯示誤導 error。修正分兩層：Tier 1 在 `proxy_or_local` 兩個 fallback branch 都發出 `tracing::warn!`，含 `tool` / `instance` / `error` field，讓未來 silent fallback 可觀測；Tier 2 引入 `requires_daemon_state(tool)` predicate。會接觸 `ACTIVE_CHANNEL` / `heartbeat_pair` 的 tool（`reply`、`react`、`download_attachment`）絕不靜默 fallback，而回傳 structured `{"error": "tool '<NAME>' requires daemon API; not reachable: <CAUSE>"}`。Stateless tool（`inbox`、`task`、`list_instances`、`send`）保留 offline-friendly fallback behavior。`requires_daemon_state` schema field 透過 `tools/list` 公開，讓 consumer 可預先 filter。（#495；感謝 @changhansung 回報）
- **Telegram 在 image + no-caption + download-fail 時靜默丟棄**——image 無 caption 且下載失敗（network / token / size）時，`handle_message` 會靜默丟掉 inbox message。User 看見 image 已送出，agent 卻未收到 inbox event，也沒有 log 顯示 failure。修正沿用 #488 silent-fallback 模式：增強的 `WARN` log 帶有 `file_id` + `sender_id` + `kind` + `error`；當 `is_image && text.is_empty()` 時，inbox text 現在為 `[image attached but download failed]`。Caption 絕不被覆寫——user-supplied text 永遠優先。（#497）
- **PTY-inject layer attachment indicator（silent-drop class layer 4）**——`#497` 修復 inbound layer（telegram → message store），但後續 layer 仍會丟失 signal：純 image 無 caption 成功下載、以 populated `attachments` 存入 inbox 時，PTY-inject formatter（`format_notification_for_inject`）會建立沒有 `attachments=[…]` field 的 `[AGEND-MSG]` header，inline body 也是空文字；agent 合理地將它視為意外空 message。修正加入兩種互補 indicator：`pointer_only=true` header 現在會以 kind-aggregated stable order 發出 `attachments=[1 photo, 2 document]`，`pointer_only=false` body 則在 text 為空但有 attachment 時 fallback 為 `[1 photo: cat.jpg]` / `[1 photo attached]` / `[1 photo, 2 document attached]`。Filename 來自 `original_filename`，不是 filesystem `path`，因此不洩漏 local path。新的 `notify_agent_with_attachments` variant 攜帶 metadata；plain `notify_agent` 變成 thin shim，使三個 non-telegram caller 維持舊 API。已擷取 empirical regression proof（將 `summarize_attachments_for_header` 改為一律回傳 `None` → 3 個 anchor test 以逐字 signature panic）。（#501）
- **TUI restart input routing**——Pane struct restoration 取代零碎 field update；後者會破壞 respawn 時的 input routing。（#445；感謝 @cheerc）
- **Telegram ANSI ESC + typed injection optimization**——從 outbound 移除 ANSI escape sequence，並最佳化 typed injection 以避免 ESC conflict。（#462；感謝 @cheerc）

### 社群

本 release 包含外部 contributor 的貢獻：

- **@cheerc**——#445（TUI Pane restart routing）、#462（ANSI ESC strip）、#473（fleet.yaml instructions wiring）、#474 issue（TUI close path）
- **@changhansung**——第一個社群回報 issue #488（`reply` MCP no-active-channel）

感謝你使用本專案並回報問題——multi-agent CLI tooling 的成敗仰賴 real-world workflow 揭露缺口。

### 文件

- **FLEET-DEV-PROTOCOL §13——`AGEND_GIT_BYPASS=1` Usage**——何時需要 bypass（在 bound path 上 worktree add/remove、daemon-internal git operation）、何時不需要（bound worktree 內的 routine operation 可正常通過），以及各 scenario hint。（#476）
- **README「Git Behavior Modification」揭露**——醒目的 pre-alpha banner section 說明修改內容（PATH shim、prepare-commit-msg hook、deny matrix、auto bind/lease）、原因（multi-agent safety、audit trail、lifecycle hygiene、foot-gun guard）、風險（agent 看到不同的 `git`、commit 增加 trailer、部分 command 意外被拒、shim update 需要 restart），以及 bypass path。（#478）
- **FLEET-DEV-PROTOCOL §7——PR open semantics**——規定 default-ready policy 與 `--draft` 保留的三種 scenario。（#491）
- **Sprint 53 PLAN doc + Sprint 54 PLAN doc**——wire-and-cleanup proposal（#463）與 reliability+docs sprint proposal（#483、#485 §5.1 amendment、#486 P0-3 absorption note）。公開記錄 §1.4 hard learning + Path A/C smoke gate classification policy。

### 內部

- **Sprint 53 §1.4 hard learning**——`cargo test green + dual VERIFIED + soak ≠ production wired`。在 pre-IMPL invariant 中攔下 Sprint 49 deadlock-class regression 的 cushion，沒有攔下 dead-code-class regression，因為沒有測試真正的 production entry point（`app::run_app`）。Sprint 54 PLAN §5 將 per-phase production-smoke gate 設為強制；§5.1 為 non-wiring refactor 分出可平行化的 Path C（`d-20260507004113587226-7`）。
- **Empirical regression-proof discipline**（`d-20260506171720519048-2`）——每個 Tier-2 fix 都要證明停用 production change 會使特定 test FAIL；恢復後回到 PASS。擷取的 FAIL signature 逐字附於 PR description。
- **`release_worktree` 是 binding lifecycle 的唯一真相來源**（`d-20260506171736738779-3`）——所有 comms.rs handler 將 binding state 視為 read-only；只有 dispatch hook（init）與 `release_worktree`（exit）可 mutation。#477 cascade 展示違反此原則的代價。
- **Cleanup lifecycle 分層**（`d-20260506171805866878-4`）——三個 tier 各有明確 ownership：per-pane（`full_delete_instance`）、per-deployment（`cleanup_deployment_dirs` + `reconcile_after_close`）、boot reconcile（`reconcile_orphans`）。新的 cleanup logic 必須指出由哪個 tier 擁有新 behavior。
- **Fleet IMPL/review dispatch policy**——只有 `dev`（IMPL）與 `reviewer`（review）可 dispatch；`claude-76f359` / `kiro-cli-*` / `gemini-*` 不是 designated。Dev 無法使用時，lead 採 Path A escalation。記錄於 operator m-57 + m-62 correction 後的 lead-side memory。

## [0.5.0] — 2026-05-04

### 新增

- **以 ID 為基礎的 routing migration（Sprint 46）**——每個 fleet instance 都會獲得 `InstanceId`（UUIDv4）。Routing 透過 `resolve_instance(name_or_id)` 以三步驟解析（完整 UUID → short-id → name）。取代 Sprint 44 M5 的 name-lookup 臨時修補。Self-route check 會比較 ID。Task event 與 dispatch tracking 新增 audit-trail field（`emitter_id`、`from_id`、`to_id`）。（#407、#409、#412）
- **CI hardening（Sprint 47）**——Job-level `timeout-minutes: 60` safety net。Per-step timeout（fmt 5m、clippy 10m、build 20m、test 20–30m、smoke 10m）。PR 使用帶 `cancel-in-progress` 的 concurrency group——被取代的 CI run 會自動取消。（#411）
- **File path migration infrastructure（Sprint 46 P2）**——新增 `inbox_path_resolved` 與 `metadata_path_resolved` helper，透過 symlink 將 name-based path migration 至 id-based path。（#409）

### 變更

- **大型檔案拆分 refactor（Sprint 48）**——三個 oversized file（總計約 8700 LOC）拆成 25 個 submodule，全部 ≤700 LOC：
  - `layout.rs`（2170 LOC）→ 6 個 submodule：`pane`、`tree`、`preset`、`split`、`tab`、`mod`（#414）
  - `channel/telegram.rs`（4201 LOC）→ 13 個 submodule：`state`、`topic_registry`、`send`、`inbound`、`error`、`creds`、`reply`、`bot_api`、`notify`、`adapter`、`ux_sink`、`bootstrap`、`mod`（#416、#419）
  - `render.rs`（2352 LOC）→ 7 個 submodule：`core_render`、`border`、`overlay`、`panels`、`panels_fleet`、`scratch`、`mod`（#421）
  - 解決 circular dependency：`split_chunks` 從 render 移至 layout/split（#414）
- **CI workflow cleanup**——合併重複的 clippy/test step，並將 checkout 升至 v5。（#422）

### 修復

- **Codex InteractivePrompt false positive**——移除 codex `Update available!|Press enter to continue` regex；它會誤中普通 idle prompt，造成 spurious operator notification。（#408）
- **create_instance 未持久化 topic_id**——`create_instance` 建立 Telegram topic，卻從未將 `topic_id` 寫入 `fleet.yaml`。Daemon restart 後該 topic 會成為 orphan。現在透過 `update_instance_field` 持久化；`describe_instance` 也會顯示 `topic_id`。（#417，關閉 #415）
- **Windows CI mock server hang**——Test mock server 新增 `Connection: close` header，讓 Windows CI 能可靠執行。（#420）

### 還原

- **Sprint 49 channel discipline correction**——Inject-only nudge mechanism（PR #424）因 daemon deadlock 與設計問題而 revert。後續重新設計追蹤於 issue #426。（#425）

### 內部

- Sprint 44 push-time semantic gate：claim verifier + pre-push hook（M1+M2）、reviewer SHA gate + ci-watch supersede（M3+M6）、hallucinated-fn extension（M4）。（#384、#385、#386）
- Sprint 44.5：post-merge rebuild hook + CI slowness investigation。（#388、#389）
- Sprint 45：橫跨 9 個 architecture group 的 15 個 PR——persistence/audit、set_var removal、shared runtime、lifecycle、channel、MCP、fleet config、state classifier、CLI/bootstrap。（#390–#404）
- Sprint 48 investigation：Windows 上 bitbucket test 在 tray feature 下 hang——根因是 `tao` Win32 message pump interference，不是 test logic。（#418）

## [0.4.1] — 2026-04-24

### 修復

- **0.4.0 的 `cargo install agend-terminal` build failure**——`src/protocol.rs` 對該 release 的歷史 protocol path 使用 `include_str!("../docs/FLEET-DEV-PROTOCOL-v1.md")`，但檔案不在 `Cargo.toml` `include` whitelist 中。因此 `cargo publish` 發往 crates.io 的 packaged tarball 缺少 bundled protocol doc，verification compile 以「No such file or directory」失敗。GitHub Release binary 從 source tree 建置，不是 packaged tarball，因此 v0.4.0 binary download 仍可使用——但 crates.io 上沒有 v0.4.0。除這一項 packaging fix 外，v0.4.1 與 v0.4.0 source 完全相同。

## [0.4.0] — 2026-04-24

自 `0.3.2` 以來超過 170 個 commit——包含 multi-agent collaboration plumbing（fleet protocol v1.1、correlation thread、health reporting）、inbox resilience + correlation、task-board dependency + deadline、`watch_ci` reliability overhaul，以及一系列 TUI / spawn lifecycle / Telegram fix。

### 新增

- **Fleet Development Protocol v1 + v1.1**——`protocol/.default/FLEET-DEV-PROTOCOL.md` 正式定義 source-of-truth、dispatch contract、ack absorption、`set_waiting_on` / timeout（§7），以及 Reviewer Contract addendum（wire-up grep enforcement）。它內嵌於 binary，第一次執行時會解壓至 `$AGEND_HOME/protocol/.default/`。
- **Live `<fleet-update>` injection**——daemon 會即時將 instance / team / role mutation broadcast 給每個 active agent（`fleet_broadcast`）；roster 與 role 無需 restart 即可傳播（#113、#123）。
- **MCP correlation thread**——`send_to_instance` / `delegate_task` 接受 `thread_id` + `parent_id`（reply 時自動繼承）；新的 `describe_thread` + `get_thread` MCP tool 顯示完整 conversation chain。
- **Health reporting MCP**——agent 呼叫 `report_health(reason, retry_after?, note?)`；operator 透過 `clear_blocked_reason` 清除。`BlockedReason` enum（Hang / RateLimit / QuotaExceeded / AwaitingOperator / PermissionPrompt / Crash）與 hang detector 共用 mutex，兩者不會 race-classify。
- **帶 dry-run mode 的 daemon watchdog**——每個 tick 對 per-backend fixture（Claude / Kiro / Codex / Gemini，包含 `kiro_false_usage_limit.txt` guard）執行 `classify_pty_output`，並將 `BlockedReason` 寫入 event_log。`AGEND_WATCHDOG_DRY_RUN=1` 在切換 live 前的一週 soak 期間只寫 log。
- **Task board：dependency + deadline**——`depends_on` 會在 parent 完成前自動 block downstream task；新增 `due_at` + `--duration` field；daemon sweep 會把 overdue task unclaim 回 `open`，並寫入 event_log + notification。
- **Task-board mutation integrity**——`claim` / `done` / `update` 現在會強制 assignee ownership 與 target existence；使用 descriptive error，不再 silent no-op（Sprint 5 #4 expanded scope）。
- **MCP `target` validation**——`send_to_instance` / `delegate_task` 會拒絕未知 instance name，不再 enqueue 至 phantom inbox；`delegate_task` 也像 `task create` 一樣，將 team 解析為 orchestrator（#136）。
- **Spawn `delivery_mode` field**——`send_to_instance` / `delegate_task` response 會區分 `pty` 與 `inbox_fallback`，讓 caller 知道 message 是 live 抵達 agent，或只落入 inbox（#140）。
- **Inbox correlation + observability**——加入 `thread_id` / `parent_id` field、`read_at` + TTL sweep（已讀 7d / 未讀 30d soft-delete）、`describe_message` MCP tool，以及會拒絕 future version 的 schema versioning。
- **Inbox disk resilience**——free space <5% 時進入 readonly mode；atomic append（tmp + fsync + rename）；`.draining` half-write recovery；per-file flock 涵蓋 enqueue / drain / sweep，且 recovery 在 lock 內進行。
- **大型 message 的 PTY header injection**——message >300 chars（Unicode-aware char count）時，只注入 `[AGEND-MSG] from=X id=Y kind=Z thread=T parent=P size=N`，field 會 sanitize control character；agent 透過 `inbox` MCP drain。Backend instruction 教導全部四個 CLI 辨識 header（可帶 optional ANSI prefix）。
- **Idle poll-reminder injection**——daemon 發現 idle agent + unread inbox > 0 時，注入 `[AGEND-MSG] kind=poll-reminder unread=N`，並以 atomic dedup 防止相同 count spam。
- **Schedule：重播錯過的 one-shot**——daemon startup 會掃描 `enabled && run_at < now && trigger=once`，在 24h window 內觸發（超過則丟棄並警告），透過真實 fire path 接線並有 integration test。
- **`watch_ci` reliability overhaul**——
  - `head_sha` tracking + PR 到達 terminal state 時 auto-clear（#119、#121）
  - registration 後第一次 poll 立即執行，不再等待 `interval_secs`
  - `per_page=5` + `select_runs_to_notify` 掃描 `last_run_id` 之後的每個 terminal run（rapid-push run 不再被之後的 in-progress run shadow）
  - `classify_runs_response` 區分 API error 與「沒有 run」——rate-limit JSON 不再靜默丟失 notification（#131）
  - background thread error 以 `warn` 記錄，不再靜默丟棄
  - `GITHUB_TOKEN` 未設定時，`watch_ci` MCP response 的 preventive `warning` field 會提供 `gh auth token` 提示（#133）
- **`save_metadata_batch`**——atomic batched write 取代 per-instance loop；後者會在 Windows CI 造成 cross-process write race。
- **Vertical-split mouse resize tolerance**——`│` separator 有 ±1 column hit zone，off-by-one click 不再落到 text selection（#139）。
- MovePaneTarget menu 中的**分割方向 picker**——將 pane 移到 target tab 時，可選 horizontal / vertical（#37）。
- **TUI usability hint**——Task Board overlay 顯示 `? help` indicator；main status bar 顯示 `Ctrl+B ? help`（#93、#94）。
- **`agend.md` 中 team-aware peer section**——auto-generated peer block 會區分 team member 與其他 fleet agent。
- **Pre-flight Claude session check**——當 `~/.claude/projects/<encoded-cwd>/` 沒有任何含 `"type":"user"` line 的 jsonl 時，daemon、TUI session restore 與 API spawn path 會預先將 `Resume → Fresh` 降級。消除 idle-pane restart 時短暫顯示「No conversation found to continue」error（#130）。

### 變更

- **Schema 內的 `watch_ci` throttle state**——`last_polled_at` field 取代 mtime-backdating workaround；first-poll-immediate 現在是 schema-local rule，不是 filesystem trick。
- **Crash-respawn 使用 `Fresh` mode**——crash 後 stale `--resume` 會穩定陷入「conversation not found」loop；respawn 現在略過它。
- **`spawn_one` 回傳實際使用的 `SpawnMode`**——Resume → Fresh downgrade 對 caller 可見，使 post-spawn gate（例如 broadcast suppression）依真實 outcome 運作，而非 requested mode。
- **獨立 `is_tab_bar_row` fn + `TAB_BAR_HEIGHT` const**——消除 mouse + render path 中的 magic `row == 0` check（#38）。
- **`spawn_one` 使用 backend preset `submit_key`**——Gemini 的 `\n\r` 不再 hardcode 為 `\r`（#98）。
- **Task ID 加入 microsecond + atomic seq**——concurrent create 時不會 collision（沿用 `decisions.rs` format）。
- **State detection 對 shell / stuck prompt 更溫和**——降低 stall classification 的 false positive（#122）。
- **Periodic tick 接入 app mode**——schedule、CI watch、health decay、sweep 在 app mode 中都以相同 daemon cadence 執行（之前僅 daemon mode）（#100）。

### 修復

- **`watch_ci` 會通知每種 terminal CI state**，不再只有 `failure`（#105）。
- **Mouse selection white-block residue + Cmd+C false PTY input**（#104）。
- **Ctrl+B prefix Shift+key binding** 在 Kitty-protocol terminal 上失效（#100、#102）。
- **Shift+Enter newline** + Repeat mode + 在需要 keyboard enhancement disambiguation 的 terminal 上使用 LF（#71、#72、#75）。
- **`compose_aware_inject` 在 agent idle 時自動 submit**——先前路徑需要手動 submit（#96、#99）。
- Status bar 的 **Help hint 靠右對齊**（#109）。
- **Telegram routing**——從傳入 home 解析 channel（#115）；將 thread home 傳入 react / edit / download（#116）；每次 API spawn 建立 topic；接通 UxEvent producer（#66、#68）；防止 block_on-inside-runtime panic（#69）；macOS contract test 不需要 bot（#48）。
- 將 MCP **`AGEND_INSTANCE_NAME` injection** 加入所有 backend 的 MCP config env（#61）。
- **Stale `ToolUse` / `Thinking` state** 透過 periodic tick expire（#102 follow-up）。
- **`delete_instance` guard** 只 block fleet member，不 block pure ad-hoc instance。
- 透過 `Sender` newtype **stamp sender identity**——修復空白 `[from:]` header injection。
- **Quickstart `working_directory`** 預設為 `$AGEND_HOME/workspace/general`。
- **Tab drag highlight + persistent selection**（#74）；4 項 UX fix——Shift+Enter / tab drag reorder / pane drag hit area / single-pane drag（#70）。
- **Notify-agent input race**——從 `notify_agent` 移除 `submit_key`（#81）。
- **Team tab 初始 ingest 時重新 tile TUI**。
- **Template member 使用 per-instance workdir**。
- 測試中的 **`AGEND_HOME` env var race**——Windows CI flake source（#107）。

### 移除

- watch_ci 的 **`per_page=1` polling**（由 per_page=5 + multi-run scan 取代）。
- watch_ci 的 **Mtime-based throttle state**（由 schema field 取代）。

### 文件

- **USAGE.md**——startup mode、architecture、keyboard shortcut（#73）。
- **Fleet Development Protocol v1 + v1.1**，包含 §7 Waiting and timeout（#62、#64）。
- **Wave 3 Stage B-UX design + Reviewer Contract v0.1**（#50）。
- **Track 1 design**——waiting_on annotation + heartbeat（A2 + A5 fix）（#58）。

## [0.3.2] — 2026-04-22

Tray-resident arc、Task #9 Option C dual-track elimination、codebase-review correctness fix 與 performance hotspot。

### 新增

- **System tray integration（`agend-terminal tray`，Cargo `--features tray`）**——在三個平台提供 native menu-bar / system-tray support。Icon color 依狀態區分（offline / idle / active）；每 2 秒 poll status；menu 頂部有 disabled status label；「Open App」會啟動設定的 terminal emulator；可切換 Autostart（launch-at-login）。Linux 以 AppImage 出貨，bundle GTK + AppIndicator library，並用 custom AppRun 在啟動時強制執行 tray subcommand；macOS + Windows release tarball 預設包含此 feature。
- **Dual-track fn drift detector**（`tests/no_dual_track_drift.rs`）——integration test 掃描 `src/ops.rs` 與 `src/mcp/handlers.rs` 的 top-level fn definition，body divergence 時 panic，byte-identical duplicate 時 warning。#31 hardening 涵蓋 top-level fn body 中的 raw string literal（fail-loud；guard 只作用於 extracted body，tests/impl block 不會 false-fail）、`extern "C" fn` / `extern "Rust" fn` prefix handling，以及 `match_balanced_brace` 無法關閉 detected fn 時的 silent-drop panic。
- **Positive-pin CREATE_TEAM dispatch test**——以 `RecordingNotifier` 為基礎的 in-process assertion，驗證 `spawn_one` 成功時恰好發出一個具有 expected payload 的 `ApiEvent::TeamCreated`；完成 C2 的三件式 equivalence bracket，並關閉 LESSONS-04-21 open item。

### 變更

- **Task #9 Option C——dual-track elimination**——shared helper 整併至 `src/agent_ops.rs`；`src/api.rs` 拆成 per-tool handler module（`handlers/instance.rs`、`handlers/team.rs`、`handlers/*`）；`src/ops.rs` 縮減為單一 `start_instance` wrapper（Task #12 之後完全刪除，inline 至 MCP dispatcher）；移除 21 個 dead CLI-wrapper fn 與 crate-level `#![allow(dead_code)]` attribute。`validate_branch` 也從 `src/worktree.rs` 移至 `agent_ops`。
- **MCP tool ACL 透過 `OnceLock` 快取**——在 startup 解析一次，不再每次 tool call 解析。
- **Layout pane-id enumeration**——新的 `collect_pane_ids()` 避免 layout traversal hotspot 中的 recursive allocation。
- **拆分 `spawn_agent`**——抽出 `build_command()`，提升清晰度與 unit-testability。

### 修復

- **Invalid state regex 現在會 panic**，不再靜默降低 state detection 能力。
- **`strip_ansi` 不再於 cursor-move sequence 插入 phantom space**；先前會破壞 captured output。
- **MCP stdio framing** 在 `Content-Length` error recovery 期間遇到 EOF 時回傳 `None`，不再 hang loop。
- **Cron schedule robustness**——`parse_run_at` 拒絕 invalid timezone（先前 fallback 至 UTC）；bad tz 時略過 schedule，不再誤觸發；`.schedule_last_check` 會 atomic write。
- **Fleet `ready_pattern` hardening**——resolve 時驗證 regex 並設 size ceiling，關閉 ReDoS surface。
- **Tray「Open App」不再凍結 tray**——terminal launch 會 detach。

## [0.3.1] — 2026-04-21

自 `0.3.0` 以來，大量工作已落到 `main`。以下依領域整理重點。

### 新增

- **Terminal app（`agend-terminal app` / 無參數啟動）**——in-process spawn 並 attach agent 的 multi-tab、multi-pane TUI。每個 agent 一個 tab、nested split、joined pane border、layout preset（`Ctrl+B Space`）、zoom、rename、scroll mode、command palette、decision / task overlay。Session layout 會透過與 `fleet.yaml` reconcile 來持久化。
- **Tmux-style keybind**（`Ctrl+B` prefix）：`c n p l 0-9 & , . w " % o x z [ d ?`，加上 repeat mode。
- **Pane interaction**——drag 交換 pane、以 arrow key resize、每 pane mouse scroll、selection + clipboard（`arboard`）、IME-aware cursor。
- **MCP spawn instance 的 auto tab/pane**——agent 透過 `create_instance` / `create_team` 建立的 instance 會自動在 TUI 顯示新 pane。
- **Windows-support Phase A**——移除 `nix` dependency；使用 `fs2` 做 file lock；透過 `src/process.rs` 提供 PID helper，並使用 platform-conditional `libc` / `windows-sys` impl；將 `/tmp` hardcoding 換成 `dirs::home_dir()` / `std::env::temp_dir()`。UDS 當時仍是最後一個 Windows blocker；已退役 Windows plan 收錄於[歷史紀錄](docs/README.zh-TW.md)。
- **Connect command（`agend-terminal connect`）**——在 running daemon 中動態註冊 external agent（只使用 inbox，不做 PTY management），供 headless environment 使用。
- **App mode 中的 Telegram**——status-bar connection indicator、將 notification route 至 owning pane。
- **CI & release workflow**——artifact upload、per-platform build。

### 變更

- **Fleet 唯一真相來源**——`fleet.yaml` 保存 agent definition；`session.json` 只保存 layout。Startup 時 session 會與 fleet reconcile。
- **Unified daemon**——standalone daemon 與 in-process app 共用 `DaemonCore`；兩種 mode 都有 API server + MCP tool。
- **Logging**——所有 `eprintln!` migration 至 `tracing`；log timestamp 使用 local timezone。
- **Agent instruction**——auto-written `agend.md` 涵蓋 identity / role / peer / MCP tool usage；各 backend variant 為 `.claude/rules/agend.md`、`.kiro/steering/agend.md`、`AGENTS.md`、`GEMINI.md`、`.codex/config.toml`。
- **Backend alias**——接受 `kiro` 作為 `kiro-cli` alias 等；serde alias 防止 fleet.yaml breakage。
- **Code review follow-up**——多輪 hardening 已落地：統一 mutex poison recovery、split fallback 不再 leak forwarder thread、預先產生 team handler instruction、layout hint parsing 改名（`from_str` → `parse_hint`）、在 clippy 1.95 下修正 overlay bound。
- **Drag / resize hardening**——drag border 與 state color disambiguate；tmux-style resize direction；split-ratio bound 依 cell count scale；title hit-testing 使用 Unicode width。
- **Mouse event routing**——overlay modal、drag guard、zoom gating；zoom 時 mouse click 不再切換 pane。

### 修復

- Shutdown 會在 kill process 前乾淨 drain registry。
- Delete-instance 會移除 working directory、metadata、session entry、Telegram topic。
- Respawn 保留 `--mcp-config` 與 `--settings` flag；使用 `fresh_args` 終止 resume crash loop。
- Codex trust directory prompt 會 auto-dismiss；`.codex/config.toml` 限定 per-project scope。
- Claude Code 會收到 `--mcp-config`，從而使用 agend MCP server。
- Daemon restart 時清除 orphaned Telegram topic。
- Worktree creation 處理 empty-repo + set-git-config edge case。
- Bugreport 會 redact `group_id`。
- 各種 clippy 1.95 fix（`collapsible_if`、`type_complexity`、`unwrap_used`、overlay bounds match guard）。
- **每次 spawn 都使用 unique instance name**——以 6-hex suffix 對 `fleet.yaml` ∪ `workspace/` ∪ `inbox/` 檢查；pane close 會清除 workspace 與 inbox entry，避免下一次 spawn 意外 resume stale agent session。
- **Codex trust prompt auto-dismiss 現在可在 macOS 運作**——dismiss pattern 會對 VTerm-rendered screen 執行（不再對 raw byte 使用手刻 strip_ansi），因此 Ink-style char-by-char cursor-positioned paint 仍能 match。Codex dismiss key 從 LF 改成 CR——macOS/openpty 不像 ConPTY 會在 input 將 LF→CR，所以 LF 先前會靜默成為 Ctrl+J（將 selection 下移）。

### 移除

- Obsolete stale debug block；`workspaces/` → `workspace/` directory rename；移除 `agend-terminal agent` CLI subcommand（agent 現在使用 MCP，而非 CLI，進行 inter-agent communication）。

---

## [0.3.0] — 2026-03（tag：`release: v0.3.0`）

> Commit `85f2bc3`——「release: v0.3.0 — fleet orchestration + stability」

### 新增

- **Fleet orchestration**——以 `fleet.yaml` 作為 first-class config；dynamic instance 的 Telegram topic persistence；`fleet.yaml` single-source reconciliation。
- **MCP tool surface**——涵蓋 user comms、agent comms、instance lifecycle、decision、task、team、schedule、deployment、repo sharing 的 35 個 tool。MCP socket pooling（透過 daemon proxy）。
- **Quickstart wizard**（`agend-terminal quickstart`）——interactive setup，處理既有 `fleet.yaml` / `.env`。
- **Demo command**（`agend-terminal demo`）——帶 crash recovery 的 split-screen live conversation。
- **Bugreport command**——單檔 diagnostic export。
- **Git worktree isolation**——`src/worktree.rs`，auto per-agent worktree；original repo 不受影響。
- 透過 `tracing` 的 **structured logging**。
- **Protocol versioning** + fleet snapshot。
- **CI loop**——auto-watch GitHub Actions，並將 failure log inject 至 agent。
- **Friendly error**、`--json` output、shell completion。
- **Telegram integration**——topic-per-instance routing、crash notification。

### 變更

- 透過 `flock` 做 file locking（crash 時 auto-release）。
- `AGEND_TERMINAL_HOME` → `AGEND_HOME`，default dir 為 `~/.agend`。
- `create_instance` 套用 backend preset arg（例如 `--yolo`、`--dangerously-skip-permissions`）。
- 縮減 Tokio feature（`full` → `rt,net,io-util,fs,macros,time`）。
- 對 instance name、branch name、path、download filename 做 input sanitization。

### 修復

- Exit code 0 時 respawn（daemon-managed agent 不應靜默消失）。
- 修正全部 5 個 backend 的 MCP config format。
- Delete flow：沒有 spurious crash log；清除 stale session ID。
- Shutdown flag 區分 crash 與 daemon stop；`Ctrl+C` 期間抑制 crash handling。
- Attach 使用 daemon run directory，而非 CLI process PID。
- Respawn 時保留 `HealthTracker`。
- Attach mode 支援 mouse scroll。

### Baseline

- PTY ownership 使用 `portable-pty`；VTerm 使用 `alacritty_terminal`。
- TUI 使用 `ratatui` + `crossterm`。
- Release 時僅支援 Unix；Windows support 在 0.3.0 後仍在進行（Phase A 已落到 `main`）。
- Backend：Claude Code、Kiro CLI、Codex、OpenCode、Gemini CLI。

---

[Unreleased]: https://github.com/suzuke/agend-terminal/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/suzuke/agend-terminal/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/suzuke/agend-terminal/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/suzuke/agend-terminal/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/suzuke/agend-terminal/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/suzuke/agend-terminal/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/suzuke/agend-terminal/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/suzuke/agend-terminal/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/suzuke/agend-terminal/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/suzuke/agend-terminal/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/suzuke/agend-terminal/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/suzuke/agend-terminal/compare/85f2bc3...v0.3.1
[0.3.0]: https://github.com/suzuke/agend-terminal/commit/85f2bc3
