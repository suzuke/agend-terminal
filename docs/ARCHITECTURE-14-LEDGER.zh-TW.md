[English](ARCHITECTURE-14-LEDGER.md)

# Architecture-14 收斂 ledger

這是 Architecture-14 收斂計畫的權威進度 ledger。它記錄的是架構 outcome，
不是 PR throughput：merged PR 可作為某項目的 evidence，但不會只因 merge
本身就完成該項目。

## Snapshot 與權威

- Snapshot 日期：2026-07-17
- `agend-terminal` baseline：`main` 上的
  `3f80ee5c75a087c5309dcb6d8d28ba7f3948edf5`
- vendored `agentic-git` baseline：`8e0fcafc25ec3e6844ca181014f6d9bb2ffbccb3`
- Snapshot 時的 GitHub state：PR #2818 已 merge 並納入此 source baseline；
  upstream agentic-git PR #37 已在 `8e0fcafc` merge，issue #34 已關閉
- 計畫狀態：**0 done、9 in progress、5 pending、0 blocked**

Evidence 依下列順序排序：

1. Current-baseline source 或可重現的 test。
2. Merged commit，加上其 exact-head 與 protected-main verification record。
3. 目前的 issue、task 或 decision record。
4. 歷史 report；在與 current source 對照前只可視為 lead。

若 evidence 互相矛盾，以排名較高者為準。尤其不能把 issue title 與舊有
source count 當成目前事實。

## Status 與完成規則

| Status | 意義 |
|---|---|
| `done` | 每個 required slice 都已 merge、整體 invariant 已展示、protected `main` 的 exact CI 為 green，且 required runtime/deployment smoke test 全數通過。 |
| `in progress` | 至少一個 durable foundation 或 bounded slice 已 merge，但 target invariant 仍有一部分未完成。 |
| `pending` | 可能已有 design 或 prerequisite，但 implementation 尚未建立 target invariant 的實質部分。 |
| `blocked` | 外部條件改變前無法安全推進。只有 dependency 通常代表 `pending`，不是 `blocked`。 |

每個項目的完成都要求下列條件全部成立：

1. Failure-first test 涵蓋真正的 production entry point 與 restart/replay
   boundary，而不只 helper。
2. Review 綁定 exact subject head，且 stale verdict 會被拒絕。
3. Branch CI 通過、slice merge，接著 protected `main` 上 exact merge SHA
   的 CI 再次通過。
4. 影響 runtime 的工作通過相關 daemon restart、deployment 與
   cross-platform smoke matrix。
5. Rollback 必須是經測試的 exact-merge revert，或針對無法安全 downgrade
   的 durable state 提供已記錄的 forward repair。

Rollback 必須保留 invariant：絕不可用刪除 evidence、關閉 admission check，
或恢復已知 fail-open path 的方式 recovery。在 durable schema 或 authority
cutover 前，slice 可以使用經測試的 exact-merge revert。Cutover 後，則需要
演練過的 downgrade 或 forward-repair procedure，保留 WIP、generation、
journal 與 unsettled obligation。適用的 rollback path 未經實際演練前，
項目不得標為 `done`。

依風險排序的執行順序為：

`1 → 4 → 10 → 3 → 5 → 2 → 6 → 8 → 9 → 7 → 11 → 12 → 14 → 13`

這不是完整 dependency order。彼此獨立的 safety slice 可平行進行，但每次
exact-main close loop 後都必須重新評估順序。

## 摘要

| # | Architecture outcome | Status | 目前剩餘 invariant |
|---:|---|---|---|
| 1 | Exact authority identity | in progress | Generation-scoped admission 必須涵蓋 crash recovery 與每個 mutation/review/release entry point。 |
| 2 | Unified durable workflow | pending | 一個 replayable workflow episode 必須從 dispatch 一路擁有 authority，直到 exact-main completion。 |
| 3 | Ledger/outbox authority | in progress | Reporter-scoped 與 CI-scoped settlement 是 bounded slice；各 durable row 仍須收斂到單一 action authority。 |
| 4 | Strict task-board routing and owner normalization | in progress | Routing 與 typed blank-owner normalization 已 merge；membership-change settlement 尚未 end-to-end canonical。 |
| 5 | Usage-limit takeover | in progress | Operator-capability ingress 已強制；仍缺 generation-fenced replacement transaction 與 exact-once resume。 |
| 6 | Review provenance | in progress | Exact revoke 與 pre-CI assignment 已 merge；reviewer transfer/restart/reassignment 仍不完整。 |
| 7 | Ordered merge train | pending | 尚無 durable、restart-resumable merge queue 擁有所有 gate。 |
| 8 | Notification routing and obligation settlement | in progress | 所有 actionable notification 都必須共用 correlated delivery 與 settlement 語意。 |
| 9 | Session continuity | pending | 尚未實作 coherent checkpoint proof 與 exactly-once fresh-session resume。 |
| 10 | Transactional worktree lifecycle | in progress | Managed release 與 branch retirement 現已 fail closed；upstream agentic-git PR #37 已釘住 narrow submodule-classification slice，但所有 destructive path 共用單一 permit/journal 與 shared-directory deletion 仍待處理。 |
| 11 | Shared `RuntimeCore` | in progress | App 與 headless mode 仍有重複 ownership 與 local API loopback。 |
| 12 | Typed backend capability contract | pending | Partial model/resume type 必須成為完整的 per-backend capability matrix。 |
| 13 | Typed invariant migration | in progress | String/source guard 仍需有系統地稽核並改為 proof type。 |
| 14 | Windows process reliability | pending | 缺少 native Windows process-tree、ConPTY、handle、timeout 與 restart proof。 |

## 1. Exact authority identity——in progress

**Target invariant。** Production 中任何 mutation、review、restart 或
release path 都不得以 bare instance name 或 stale generation 行動。
Competing lease 與 A-to-B identity replacement 必須 fail closed，且 replay
後仍維持正確。

**目前 evidence。** [PR #2777](https://github.com/suzuke/agend-terminal/pull/2777)
（`e13a3f30`）merge 了 fail-closed workspace identity guard；其 merge train
也完成 exact-main verification。Current source 仍在
[`pane_factory.rs`](../src/app/pane_factory.rs#L340) 以
`crash_tx: None` 建立 app-mode agent，而
[`respawn_watchdog.rs`](../src/daemon/per_tick/respawn_watchdog.rs#L20)
記錄 normal crash-respawn machinery 在此 mode 不會作用。因此
[issue #2765](https://github.com/suzuke/agend-terminal/issues/2765) 仍在
critical path 上。

**剩餘 invariant。** 引入 generation/incarnation-scoped crash admission，
並稽核每個 production authority entry point，避免 name lookup 在 validation
後重新取得 authority。

**完成／驗證。** Deterministic same-owner restart、foreign-owner refusal、
stale-generation、competing-lease、crash-before-admission 與 daemon-replay
test 必須透過真正的 app 與 headless entry point。Runtime smoke 必須證明
stale exit 無法替 replacement instance publish `Restarting`。

## 2. Unified durable workflow——pending

**Target invariant。** Dispatch、delivery、branch linkage、review、CI、merge、
protected-main verification 與 terminal settlement，必須是單一 durable、
replayable aggregate 的 phase。Premature done、merge、head move 與 restart
都要 fail closed。

**目前 evidence。** Foundation 分散於
[PR #2763](https://github.com/suzuke/agend-terminal/pull/2763)（`7a9c12b1`，
freshness-aware PR state）、
[PR #2789](https://github.com/suzuke/agend-terminal/pull/2789)（`f935ef95`，
dispatch prevalidation）、
[PR #2797](https://github.com/suzuke/agend-terminal/pull/2797)（`a306be9c`，
plan-ack authority）與
[PR #2798](https://github.com/suzuke/agend-terminal/pull/2798)（`af24226f`，
durable protected-CI handoff）。它們尚未形成一個 aggregate。
[Issue #2454](https://github.com/suzuke/agend-terminal/issues/2454) 也是
boundary symptom：此 baseline 的 `src/mcp/handlers` 下仍有 18 個 production
`crate::api::call` / `call_at` reference（排除 test/repro module 與 doc
comment），而不是歷史 title 的 35 個。

**剩餘 invariant。** 一個等同 `DispatchSaga` 的 authority 必須擁有 phase
transition、exact subject identity、receipt、recovery 與 settlement；個別
watcher 或 inbox row 不得成為彼此競爭的 workflow truth。

**完成／驗證。** Replay 每個合法 transition，並在每次 durable write 與
side effect 之間注入 crash。Premature done/merge、stale head、duplicate
delivery、daemon restart 與 protected-main CI failure，都必須保留一個可
recovery 的 non-terminal episode。

## 3. Ledger/outbox authority——in progress

**Target invariant。** Dropped/full/disconnected wake 或 daemon restart
絕不可丟失 action 或執行兩次。Terminal 或 discarded row 永遠不得重新變成
actionable。

**目前 evidence。** [PR #2766](https://github.com/suzuke/agend-terminal/pull/2766)
（`7e3277cd`）新增 durable reviewer-assignment authority 與 pending outbox；
[PR #2788](https://github.com/suzuke/agend-terminal/pull/2788)（`029fa3a7`）
讓 obsolete-assignment retirement durable；
[PR #2798](https://github.com/suzuke/agend-terminal/pull/2798)（`af24226f`）
新增 durable protected-CI handoff episode。後續 bounded slice 又在
[PR #2808](https://github.com/suzuke/agend-terminal/pull/2808)（`76d9ab33`）
新增 CAS-checked terminal feature-watch removal，並在
[PR #2813](https://github.com/suzuke/agend-terminal/pull/2813)（`17df827f`）
新增 reporter-scoped dispatch settlement。這些仍是各自獨立的 ledger，
lifecycle rule 彼此相關但不完全相同；新 slice 縮小 replay ambiguity，
但尚未建立單一 outbox authority。

**剩餘 invariant。** 讓 action authority 收斂至 workflow 與 notification
consumer 共用的 row-first persistence、stable correlation identity、
monotone state、CAS claim/settlement、explicit supersession 與 restart
reconciliation。

**完成／驗證。** 在 enqueue、delivery、ack、supersede 與 settlement 前後
注入 crash；從每個 durable prefix replay。證明 at-least-once delivery 搭配
idempotent action、沒有 terminal re-fire，也沒有 silent discard。

## 4. Strict task-board routing and owner normalization——in progress

**Target invariant。** Task ID 只能解析至一個 opaque board route；ambiguous
或 unreadable routing 必須 fail closed。Empty/whitespace owner 一律 normalize
為 unassigned，ACL decision 在 membership change 前後使用單一 canonical
owner identity。

**目前 evidence。** [PR #2769](https://github.com/suzuke/agend-terminal/pull/2769)
（`64f6953e`）merge strict routing 並關閉
[issue #2760](https://github.com/suzuke/agend-terminal/issues/2760)。
[PR #2797](https://github.com/suzuke/agend-terminal/pull/2797)（`a306be9c`）
在 transition lock 下驗證 plan-ack authority；
[PR #2799](https://github.com/suzuke/agend-terminal/pull/2799)（`be1b3546`）
修正 cross-board、cross-lease auto-release。
[PR #2809](https://github.com/suzuke/agend-terminal/pull/2809)（`ba4dc043`）
接著引入 typed `AssigneePatch` handling：owner omitted 時保留目前值、
explicit null 會清除，而 blank/whitespace string 在 task write boundary
normalize 為 unassigned（`src/tasks/handler.rs`）。

**剩餘 invariant。** 為因 team 或 project membership change 而 orphan 的
task 加入明確 administrative settlement/migration path，並證明每個 secondary
ACL/serialization path 都使用 normalized owner representation。

**完成／驗證。** Routing/ACL matrix 必須涵蓋 default 與 project board、
unreadable/duplicate ID、blank owner、team assignee、membership change、
replay 與 concurrent owner transition。任何 actor 都不得只因兩條 path 對
同一 identity 使用不同 normalization，而取得或失去 authority。

## 5. Usage-limit takeover——in progress

**Target invariant。** 一個 usage-limit episode 只能建立一個 checkpoint、
notification 與 takeover action。Replacement 受 generation fence；dirty 或
unproven state 必須拒絕；resume exactly once。

**目前 evidence。** [PR #2759](https://github.com/suzuke/agend-terminal/pull/2759)
（`864bd5db`）merge durable UsageLimit control-plane episode。這是 Slice 1：
建立 episode identity 與 durability，不是 transactional replacement。
[PR #2814](https://github.com/suzuke/agend-terminal/pull/2814)（`e4fd2d20`）
新增 operator-invoked `usage_limit_takeover` MCP action，並在 API ingress
強制 operator capability，且 real handler 與 ingress path 一起受 test
涵蓋。這關閉了 agent-self-invocation authority gap，但 handler 仍是 bounded
takeover slice，不是完整 replacement saga。

**剩餘 invariant。** 實作 mandatory operator-invoked、idempotent
replacement transaction，具備 generation fencing、coherent checkpoint
proof、每個 partial state 的 recovery 與 typed backend refusal。Automatic
takeover 在 telemetry 證明需要前仍為 optional；不得把它當成關閉 safety
invariant 的必要條件。

**完成／驗證。** Repeated signal、concurrent operator request、dirty
worktree、unsupported backend、replacement 期間 crash，以及 old/new backend
output race，都必須收斂為一個 replacement 與一個 resume claim。

## 6. Review provenance——in progress

**Target invariant。** Merge authority 只能來自綁定 exact repository、
branch、head SHA、assignment generation、reviewer incarnation 與 review
class 的 receipt。Head/reviewer change 會使舊 authority 失效；daemon restart
後要 reconcile GitHub mirror。

**目前 evidence。** Foundation 包括
[PR #2766](https://github.com/suzuke/agend-terminal/pull/2766)（`7e3277cd`）、
[PR #2772](https://github.com/suzuke/agend-terminal/pull/2772)（`bce3cb39`）、
[PR #2783](https://github.com/suzuke/agend-terminal/pull/2783)（`cf83697f`）與
[PR #2788](https://github.com/suzuke/agend-terminal/pull/2788)（`029fa3a7`）。
[PR #2805](https://github.com/suzuke/agend-terminal/pull/2805)（`02b7dd67`）
新增 orchestrator-authorized exact-target revoke surface（
[#2782](https://github.com/suzuke/agend-terminal/issues/2782) 的 slice 1）；
[PR #2806](https://github.com/suzuke/agend-terminal/pull/2806)（`742ecc1a`）
在 terminal CI 前建立 exact-subject pending PR-state record，並關閉
[#2800](https://github.com/suzuke/agend-terminal/issues/2800)；
[PR #2807](https://github.com/suzuke/agend-terminal/pull/2807)（`afbf9c84`）
透過 durable cleanup intent，讓 review-worktree deletion authority-proven。
[PR #2818](https://github.com/suzuke/agend-terminal/pull/2818)（`31130f93`）
在初始 signed binding 中加入 typed、daemon-provisioned disposable-review
provenance，包含 exact-head 與 new-branch admission，以及不依賴 assignment
authority 的 fail-closed release gate。

**剩餘 invariant。** Issue #2782 的 orchestrator exact-revoke scope 已關閉，
但較廣的 Architecture-14 lifecycle 仍要求 reviewer restart/swap 與
orchestrator transfer/reassignment，能在不靠 delete-only workaround 的情況下
release 或移動 exact assignment。Restart 後 reconcile receipt，並在每次
transition 保留 exact-head merge gate。

**完成／驗證。** Reviewer restart/swap、stale generation、head move、CI
完成前 assignment、duplicate verdict、GitHub mirror restart 與 revoke
interruption，都必須保留正確 merge authority。

## 7. Ordered merge train——pending

**Target invariant。** Concurrent PR 以一個 exact base/head identity
序列化。Rebase 會使既有 gate 失效；restart 保留 queue position；只有 exact
reviewed 且通過 CI 的 head 可以 merge。

**目前 evidence。** [PR #2763](https://github.com/suzuke/agend-terminal/pull/2763)
提供 freshness-aware PR state，
[PR #2796](https://github.com/suzuke/agend-terminal/pull/2796)（`528b0c28`）
則向 CI watch 暴露 exact target SHA。最近的 merge train 成功完成，但靠的是
orchestration policy，不是 durable queue。PR #2806 已關閉 #2800，因此現在
可以在 terminal CI 前 assign review，同時讓 exact-head CI 維持 merge gate；
此 prerequisite 本身不會序列化 train。

**剩餘 invariant。** 建置由 unified workflow aggregate 擁有、可在 restart
後 resume 的 queue，具備明確 invalidation 與 successor activation。

**完成／驗證。** 三個 PR 的 clean chain 與 conflicting chain 都必須
deterministically serialize。注入 rebase、force-push、merge failure、daemon
restart、branch deletion 與 newer-main commit；任何 stale gate 都不得存活。

## 8. Notification routing and obligation settlement——in progress

**Target invariant。** 每個 actionable notification 都有一個 intended
recipient、stable correlation identity、durable delivery state 與 exact
obligation settlement。Delayed duplicate 與 reminder residue 不得造成危害。

**目前 evidence。** [PR #2771](https://github.com/suzuke/agend-terminal/pull/2771)
（`d983dbf5`）移除 blocked-state projection split brain；
[PR #2788](https://github.com/suzuke/agend-terminal/pull/2788)（`029fa3a7`）
durably supersede obsolete review assignment；
[PR #2798](https://github.com/suzuke/agend-terminal/pull/2798)（`af24226f`）
以 durable episode settle protected-CI handoff。Live audit 也發現 team/project
membership transition 可能讓舊 task 沒有任何 actor 可以 settle，因而把此
項目連到 item 4。

**剩餘 invariant。** 以 item 3 ledger contract 取代各 notification 特有的
settlement rule，內容包括 recipient incarnation、parent/correlation identity、
supersession、timeout、discharge 與 terminal-task reconciliation。

**完成／驗證。** Wrong-recipient、reconnect duplicate、delivery 與 ack 間
restart、supersede persistence failure、terminal linked task、project
membership change 與 poll-reminder replay test，都必須只 settle intended
obligation，不得碰其他 row。

## 9. Session continuity——pending

**Target invariant。** 達到 configured context threshold（計畫 acceptance
scenario 為 80%）時，系統會記錄 immutable coherent checkpoint 與獨立
restart action journal。Fresh session 只能 claim exact next step 一次；dirty
或 mid-transaction state 不得自動 restart。

**目前 evidence。** 已有 runtime-wide threshold persistence 與 item 5
usage episode foundation，但沒有完整 checkpoint/action protocol。目前
[issue #2765](https://github.com/suzuke/agend-terminal/issues/2765) 顯示
app-mode crash recovery 尚未受 generation admission。每 instance threshold
configuration 的 [issue #2779](https://github.com/suzuke/agend-terminal/issues/2779)
明確不是此 correctness outcome 的必要條件。

**剩餘 invariant。** 將 checkpoint proof
`Requested → Ready | Refused | Superseded` 與 exclusive action journal
`Prepared → OldSessionStopped → NewSessionStarted → ResumeClaimed →
Consumed | NeedsOperator` 分開持久化，並使用 task/binding/head/clean-state
與 incarnation proof。

**完成／驗證。** Threshold repetition、dirty state、partial checkpoint、
stop/start crash、duplicate resume delivery、stale incarnation 與 unhandled
query/task obligation，都必須 replay 至一個 safe next action 或明確的
operator stop。

## 10. Transactional worktree lifecycle——in progress

**Target invariant。** Create、reuse、rebase、force reclaim、release、
deletion、janitor、retention 與 GC，共用 normalized path lock、typed lifecycle
permit、durable journal、CAS recovery、recursive-submodule handling 與
Windows-safe rollback。

**目前 evidence。** 已 merge 的 foundation 包括
[PR #2768](https://github.com/suzuke/agend-terminal/pull/2768)（`35d5e664`）、
[PR #2778](https://github.com/suzuke/agend-terminal/pull/2778)（`6a584839`）、
[PR #2780](https://github.com/suzuke/agend-terminal/pull/2780)（`b2d61b81`）、
[PR #2786](https://github.com/suzuke/agend-terminal/pull/2786)（`f7e717f2`）、
[PR #2787](https://github.com/suzuke/agend-terminal/pull/2787)（`4c7d814d`）、
[PR #2790](https://github.com/suzuke/agend-terminal/pull/2790)（`b4d2be1f`）與
[PR #2799](https://github.com/suzuke/agend-terminal/pull/2799)（`be1b3546`）。
Ledger 建立後的 slice 大幅縮小 destructive lifecycle path：
[PR #2810](https://github.com/suzuke/agend-terminal/pull/2810)（`0b127f2a`）
使用 exact binding fingerprint、marker identity validation 與 WIP
preservation，把 daemon-managed `repo release` 委派給 canonical guarded
release；
[PR #2815](https://github.com/suzuke/agend-terminal/pull/2815)（`06efae12`）
強制 branch-retirement disposition 與 occupancy gate；
[PR #2816](https://github.com/suzuke/agend-terminal/pull/2816)（`1d83b423`）
讓 checkout-recovery sweep 遵守 active path lock；
[PR #2818](https://github.com/suzuke/agend-terminal/pull/2818)（`31130f93`）
則為 self-provisioned review worktree 加入 typed `disposable_review`
checkout provenance、exact provisioned-head CAS、new-branch proof，以及
terminal-task/occupancy/PR cleanup gate。這些都是 foundation，尚未形成
單一 durable lifecycle transaction。
[Issue #2764](https://github.com/suzuke/agend-terminal/issues/2764) 仍存在：
deletion 在 removal 前 capture fleet state，但
[`cleanup_working_dir`](../src/agent_ops.rs#L504) 只收到 home/name/path，
並信任 on-disk identity artifact，因此不一定能拒絕共用 canonical directory
的另一個 live instance。Upstream
[agentic-git PR #37](https://github.com/suzuke/agentic-git/pull/37)
（`8e0fcafc25ec3e6844ca181014f6d9bb2ffbccb3`）已 merge，且 vendored gitlink
已釘在此 exact commit，關閉 [issue #34](https://github.com/suzuke/agentic-git/issues/34)
的 narrow submodule-classification gap；較廣的 item-10 lifecycle invariant
仍未完成。

**剩餘 invariant。** 把所有 mutation root 與 leaf 遷移到相同 typed
permit/capability 與 durable journal，包括 janitor/retention/GC 和 branch
creation；先完成 shared live-owner deletion 與剩餘 permit/journal migration。
Upstream submodule-classification slice 已釘住，但不代表 item 10 已完成。

**完成／驗證。** 在 alias、symlink、corrupt binding、nested submodule、
process death 與 concurrent new lease 條件下，對
create/reuse/rebase/release/delete 進行 race。Recovery 必須保留 WIP，並在
macOS、Linux 與 Windows 收斂，不能出現未 journal 的 destructive operation。

## 11. Shared `RuntimeCore`——in progress

**Target invariant。** Owned TUI mode 與 headless `run_core` 對 registry、
tick、recovery、API 與 shutdown 使用同一套 service ownership model。
Attached mode 是明確 non-owner。Worker 不重複，restart order 有明確定義。

**目前 evidence。** Extraction 與 ordering foundation 包括
[PR #2770](https://github.com/suzuke/agend-terminal/pull/2770)（`054125d3`）
與 [PR #2775](https://github.com/suzuke/agend-terminal/pull/2775)
（`a1d82f47`）。剩餘規模可直接從 source 看出：
[`run_app`](../src/app/mod.rs#L678) 到 line 1732 結尾仍約 1,055 lines，
沒有 `AppState` struct，且 `src/mcp/handlers` 下仍有 15 個 production
local-API loopback。因此
[issue #2453](https://github.com/suzuke/agend-terminal/issues/2453) 與
[issue #2454](https://github.com/suzuke/agend-terminal/issues/2454) 仍成立。
Issue #2765 的 app-mode crash gap 也是 ownership symptom。

**剩餘 invariant。** 抽取一個 owned runtime/service graph 與 direct
in-process command boundary，再讓 TUI 成為該 core 的 client，而不是第二套
daemon implementation。

**完成／驗證。** Mode matrix 必須證明 registry/tick/API/recovery 恰有一個
owner、startup 有 causal order、shutdown 反向執行、restart 會收斂，且
attached mode 不擁有 service。Production MCP path 不得只為存取同 process
state 而繞經 local API。

## 12. Typed backend capability contract——pending

**Target invariant。** 每個 registered CLI/custom/raw backend 都明確宣告
model、restart/resume、state signal、usage-limit、checkpoint、native
delegation 與 nested-execution capability。不支援的 operation 回傳 typed
error；caller 絕不從 terminal text 或 flag 猜測 support。

**目前 evidence。** [PR #2757](https://github.com/suzuke/agend-terminal/pull/2757)
（`5aee597e`）新增 capability-gated explicit model intent 與 typed
`set_model`。Source 也有 partial type，例如
[`ResumeMode`](../src/backend.rs#L258) 與
[`model_capability`](../src/backend.rs#L799)。
[issue #2744](https://github.com/suzuke/agend-terminal/issues/2744) 的 residual
只剩自動 observation/capture in-session model change；explicit path 已實作。
[agentic-git issue #26](https://github.com/suzuke/agentic-git/issues/26)
追蹤缺少 machine-verifiable embedder/delegation contract 的問題。

**剩餘 invariant。** 把 partial field 收斂為一個 typed contract，強制每個
control-plane operation 依此 branch。Native child execution 必須採
deny-first，直到 writer identity、binding、event 與 quiescence proof 可用。

**完成／驗證。** Generated matrix 必須涵蓋每個 registered backend 與每個
operation 的 supported、unsupported、degraded case。Custom/raw backend 與
unknown version 必須以 stable typed error fail，不得猜測 behavior。

## 13. Typed invariant migration——in progress

**Target invariant。** 每個退役的 source-string 或 grep guard 都改由更強的
proof type、private API，或 runtime admission check 加 real-entry/replay
test 取代。任何保留的 scan 都有書面 threat-model rationale。

**目前 evidence。** [PR #2773](https://github.com/suzuke/agend-terminal/pull/2773)
（`e4841350`）新增 fixture-only real-git provenance seam；
[PR #2774](https://github.com/suzuke/agend-terminal/pull/2774)（`1fe1461d`）
exercise production nonblocking file lock。這些是 bounded example，不是
systematic retirement。Upstream
[agentic-git issue #26](https://github.com/suzuke/agentic-git/issues/26)
缺少 machine-verifiable embedder contract；
[issue #34](https://github.com/suzuke/agentic-git/issues/34) 則暴露具體
classification gap。

**剩餘 invariant。** 等 item 1–12 與 14 穩定後，盤點所有 load-bearing
scan，依 threat model 分類，且只有當新 typed boundary 嚴格更強時才替換。
支援性的 module split
[agentic-git issue #30](https://github.com/suzuke/agentic-git/issues/30)
不是 completion prerequisite。

**完成／驗證。** 每次 removal 都要展示舊 alias/re-export/rename bypass
為 RED、新 production entry point 為 GREEN，並包含 restart/replay。Audit
output 必須列舉並說明每個保留 scan。

## 14. Windows process reliability——pending

**Target invariant。** Windows Job Object process-tree kill、ConPTY
lifecycle、handle closure、file-lock behavior、path/identity handling、
timeout diagnostic、log retention、restart 與 shutdown 都必須 deterministic，
不能長時間 silent hang。

**目前 evidence。** 已有通用 tick-stall diagnostic，但尚無完整 Windows
reliability program 或 native runtime proof。macOS/Linux 成功與
cross-compilation 都不是 Windows process semantics 的 evidence。

**剩餘 invariant。** 建立 native Windows process ownership 與 diagnostic
contract，再於 real Windows runner 上關閉每個 process/PTY/handle failure
mode。

**完成／驗證。** Native Windows CI 與 runtime smoke 必須 deterministic
涵蓋 child/grandchild tree kill、ConPTY open/close、leaked handle、lock
contention、Unicode/long path、保留 log 的 timeout、daemon restart 與
shutdown。Bounded watchdog 必須一律產生可採取行動的 diagnostic，不得
silent hang。

## 經 source 驗證的 issue intake

此 matrix 決定 issue 是否改變 Architecture-14 scope。「Excluded」不表示
issue 無效；只表示它不是關閉這 14 個 correctness outcome 的必要條件。

| Issue | Snapshot classification | Architecture-14 mapping | Disposition/evidence |
|---|---|---:|---|
| [agend-terminal #2453](https://github.com/suzuke/agend-terminal/issues/2453) | 已確認的 architecture debt | 11 | `run_app` 約 1,055 lines，且沒有 `AppState`。 |
| [agend-terminal #2454](https://github.com/suzuke/agend-terminal/issues/2454) | 已確認的 architecture debt | 2, 11 | 仍有 15 個 production MCP-to-local-API loopback；title 的 35 已過時。 |
| [agend-terminal #2744](https://github.com/suzuke/agend-terminal/issues/2744) | Partial residual | 12 | Explicit typed model intent 已在 #2757 merge；只剩 automatic in-session observation/capture。 |
| [agend-terminal #2760](https://github.com/suzuke/agend-terminal/issues/2760) | 已在 main 修正；closed | — | Strict routed lookup 已於 #2769 merge；不可繼續放在 critical path。 |
| [agend-terminal #2762](https://github.com/suzuke/agend-terminal/issues/2762) | **排除的 optional feature** | — | 在 measured MCP discovery failure 證明有 correctness need 前，CLI fallback 維持 experimental。 |
| [agend-terminal #2764](https://github.com/suzuke/agend-terminal/issues/2764) | 已確認的 current safety bug | 10 | Cleanup 未使用 pre-delete fleet snapshot 拒絕 shared canonical directory 的每一個其他 live owner。 |
| [agend-terminal #2765](https://github.com/suzuke/agend-terminal/issues/2765) | 已確認的 current bug | 1, 9, 11 | App-created agent 仍使用 `crash_tx: None`；restart publication 未經 generation admission。 |
| [agend-terminal #2779](https://github.com/suzuke/agend-terminal/issues/2779) | **排除的 optional feature** | — | Per-instance threshold 增加 configurability，不是缺少的 correctness invariant。 |
| [agend-terminal #2781](https://github.com/suzuke/agend-terminal/issues/2781) | **排除的獨立 P3 bug** | — | Decimal percentage formatting/Kiro regex inconsistency 確實存在，但不是 Architecture-14 dependency。 |
| [agend-terminal #2782](https://github.com/suzuke/agend-terminal/issues/2782) | Acceptance scope 已修正；closed | 6 | #2805 新增 orchestrator exact revoke 並關閉 issue；restart/reviewer-swap transfer 與完整 reassignment settlement 仍屬較廣 Architecture-14 hardening。 |
| [agend-terminal #2800](https://github.com/suzuke/agend-terminal/issues/2800) | 已在 main 修正；closed | — | #2806 在 terminal CI 前建立 exact-subject pending PR state；不可繼續放在 critical path。 |
| [agentic-git #26](https://github.com/suzuke/agentic-git/issues/26) | 已確認的 contract gap | 12, 13 | 沒有 machine-verifiable embedder/binding/event contract。 |
| [agentic-git #30](https://github.com/suzuke/agentic-git/issues/30) | 只有 supporting refactor | — | Library 仍很大，但 module split 本身不會關閉 invariant。 |
| [agentic-git #34](https://github.com/suzuke/agentic-git/issues/34) | Upstream 已修正；已釘住 | 10, 13 | Upstream PR #37 已 merge fail-closed submodule classification，並釘在 `8e0fcafc25ec3e6844ca181014f6d9bb2ffbccb3`；較廣的 item-10 invariant 仍未完成。 |

Dependency-only update、formatting cleanup、optional CLI fallback（#2762）、
per-instance threshold configuration（#2779）、decimal-context P3（#2781）
與 agentic-git module split（#30）都不得算成 Architecture-14 progress，
也不得用來延誤 correctness close loop。

## 維護此 ledger

每次更新都必須：

1. 把兩個 source baseline 都推進到 exact immutable SHA。
2. 重新查詢 linked issue 與 PR state，並重新檢查引用的 source line。
3. 把 merged slice 記為 evidence；除非已證明完整 remaining invariant 與
   completion gate，否則不得改變 item status。
4. 新發現的 correctness gap 必須先加入 issue matrix，才能 mapping 至 item；
   optional feature 維持明確排除。
5. 保留 item numbering 與 name。在 item 內補充細節，不得重新編號計畫。
6. Item 改為 `done` 前，記錄 exact protected-main CI 與
   runtime/deployment evidence。
