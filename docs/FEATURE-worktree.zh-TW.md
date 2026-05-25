# Worktree Management

這份文件說明 git worktree 的建立、綁定、釋放、GC，以及
`bind_self` / `release_worktree` / `force_release_worktree` /
`repo action=checkout` / `repo action=release` 之間的分工。

## 1. 設計初衷

- 每個 agent 應該在自己的工作空間工作。
- 這樣可以避免 branch 衝突。
- 也可以避免多 agent 共享同一個 checkout。
- worktree 是這個隔離模型的核心。
- worktree 讓 branch 與 agent 的關係可追蹤。
- 這也讓 release 與 GC 可以獨立治理。
- canonical 佈局是 `~/.agend/worktrees/<agent>/<branch>/`。
- 這是 daemon-managed 的新 layout。
- 舊 layout 仍可被偵測。
- 但新 create 不應回到舊 layout。
- 綁定資訊放在 `binding.json`。
- daemon-managed 標記放在 `.agend-managed`。
- pinned 狀態放在 `.agend-pinned`。
- GC 只看 daemon-managed worktree。
- operator-created worktree 不應被 daemon 自動刪。

## 2. 目錄與標記

- `worktree_path(home, agent, branch)` 是 canonical path helper。
- 它會回傳 `<home>/worktrees/<agent>/<branch>/`。
- `legacy_worktree_path` 只用於偵測舊 layout。
- 舊 layout 是 `<source_repo>/.worktrees/<agent>/`。
- `is_daemon_managed()` 檢查 `.agend-managed` 是否存在。
- `.agend-managed` 內會寫 `agent=...`。
- `.agend-managed` 內會寫 `branch=...`。
- `.agend-managed` 內會寫 `leased_at=...`。
- `release()` 之後還會補 `released_at=...`。
- `.agend-pinned` 表示 operator override。
- pinned 的 worktree 不應進 GC。
- binding.json 才是 runtime lease 的關鍵來源。
- source_repo 會從 binding.json 讀回。
- 這讓 release 能從 owning repo 角度執行 git 操作。

## 3. worktree 建立

- `worktree::create()` 先檢查是不是 git repo。
- 不是 git repo 就直接回 `None`。
- 若 repo 沒有 HEAD，會嘗試補一個 init commit。
- 這是為了讓 worktree 建立可在空 repo 上啟動。
- init commit 失敗會回 `None`。
- branch 若未指定，預設是 `agend/<instance_name>`。
- branch 會先經過 `validate_branch`。
- 不合法的 branch 會被拒絕。
- worktree path 會根據 agent 與 branch 組合。
- 若目標目錄已存在，會先驗證實際 HEAD。
- 如果現有 worktree 的 branch 不一致，視為 lease conflict。
- 如果現有 worktree 的 branch 一致，會直接 reuse。
- reuse 時回傳的 `WorktreeInfo` 仍指向既有 path。
- 若 worktree 不存在，才呼叫 `git worktree add`。
- `git worktree add` 失敗時會嘗試另一條相容路徑。
- create 的重點是「可重用、可驗證、可失敗回報」。

## 4. create 的使用者面

- `repo action=checkout` 是外部最常見的入口。
- `bind=true` 代表 atomic provision + bind。
- `bind=false` 代表只建立檢視用 worktree。
- `bind=true` 需要 `AGEND_INSTANCE_NAME`。
- `bind=true` 不接受 protected branch。
- `bind=true` 會把 HEAD 放在 named branch。
- `bind=false` 會以 detached HEAD 建立。
- `bind=true` 會走後續 tail-ops。
- tail-ops 包含 marker、binding.json、ci watch。
- `bind_self` 也是一種綁定入口。
- 但它適合 mid-lifecycle 重綁。
- `bind_self` 可使用 fleet.yaml 的 source_repo。
- `bind_self` 也支援 `rebase_mode=true`。
- fresh-task dispatch 傾向用 `repo action=checkout bind:true`。
- mid-lifecycle recovery 傾向用 `bind_self`。

## 5. lease 與 bind

- `worktree_pool::lease()` 會拒絕 protected branch。
- protected set 由 `agent_ops::is_protected_ref` 定義。
- lease 會呼叫 `worktree::create()`。
- lease 成功後會寫 `.agend-managed`。
- lease 也會寫 binding.json。
- binding.json 是 release 的關鍵輸入。
- lease 的 binding 會把 worktree、source_repo 一起記下。
- lease 是 daemon-managed worktree 的建立語意。
- `bind_self` 與 `repo action=checkout bind:true` 最終都會走 lease 邏輯。
- 這讓綁定狀態與檔案系統狀態一致。
- bind-in-flight guard 也由 release/cleanup 清掉。
- lease 失敗通常代表 branch conflict 或 validation 問題。

## 6. soft release

- `worktree_pool::release()` 是 soft mark。
- soft release 不會刪掉 worktree。
- 它會先解除 binding。
- 它會把 released_at 寫進 `.agend-managed`。
- 這讓 GC 有 grace window。
- soft release 適合切換到可回收狀態。
- 它是 Phase 3 soft mark，不是 hard delete。
- `release()` 之後 worktree 仍存在。
- 但它已經變成 GC 候選種子。
- 若 binding 已無，之後可進一步做 hard release。

## 7. hard release

- `release_full()` 是 hard release。
- 這是 `release_worktree` MCP tool 的核心。
- 它會先讀 binding.json。
- 如果沒有 binding，回傳 idempotent no-op。
- 它只會處理 `.agend-managed` worktree。
- 如果 worktree 沒有 marker，會拒絕刪除。
- 這是防止 operator-created worktree 被誤刪。
- 若 worktree path 已不存在，會先 prune 相關 git metadata。
- 若 `git worktree remove --force` 成功，會標記 worktree_removed。
- 若 git command 失敗，會 fallback 到 remove_dir_all。
- remove_dir_all 失敗也不應卡住 binding 清理。
- release_full 會清 binding。
- release_full 也會清 bind-in-flight。
- 這避免 stale lock 卡住 rebind。
- release_full 之後還會做 branch cleanup 評估。
- branch cleanup 只有在 managed_verified 時才可進行。

## 8. branch cleanup

- branch cleanup 只針對已釋放的 daemon-managed worktree。
- 它不是一般釋放的主責。
- 它會檢查 branch 是否 protected。
- protected branch 不會被刪 local ref。
- 它會先 fetch --prune 遠端。
- 它會檢查 branch 是否已 merge into main。
- 它也會檢查 remote tracking ref 是否已消失。
- 只要符合條件，就可以刪本地 branch。
- 這是 release_full 的附加清理。
- 若分支未 merged，會跳過。
- `branch_cleanup_skipped_reason` 會說明原因。
- 這讓 release 的回應可審計。

## 9. emergency cleanup

- `force_release_worktree` 是 emergency tool。
- 它是給 stale worktree directory 使用的。
- 它的 target 是 `<home>/worktrees/<agent>/<branch>/`。
- 它會先驗證 path 安全。
- 安全檢查會拒絕 pool 外路徑。
- 它會嘗試直接 remove_dir_all 目錄。
- 目錄刪除失敗不阻止 binding 清理。
- 它會再呼叫 `release_full()`。
- 它還會做 git metadata prune。
- 這是為了處理 no-binding 但 dir 殘留的狀況。
- `rebase_clean_self()` 是共用 helper。
- `bind_self(rebase_mode=true)` 也會用到它。
- 這讓 recovery 與 emergency cleanup 共用同一組安全邏輯。

## 10. repo release

- `repo action=release` 是 generic path release。
- 它接受一個 path。
- 它會先 validate/canonicalize path。
- 不安全的系統路徑會被拒絕。
- `HOME` 自己也不能刪。
- path 太淺也會拒絕。
- 成功時會先嘗試 `git worktree remove --force`。
- 失敗時會 fallback 到 remove_dir_all。
- 它不依賴 agent binding。
- 因此它比較像「對某個路徑做釋放」。
- `release_worktree` 則是 agent-centric。
- 兩者一個偏 path，一個偏 agent。

## 11. GC 語意

- `gc_dry_run()` 是非破壞性的。
- 它只列候選。
- `gc_cutover()` 才真的刪。
- `gc_cutover()` 需要 `AGEND_WORKTREE_GC=1`。
- 沒設 env 會直接跳過。
- GC 只看 daemon-managed worktree。
- GC 也會跳過 pinned。
- GC 也會跳過還有 active binding 的 worktree。
- GC 必須看到 `released_at`。
- 沒有 `released_at` 就視為仍 active。
- GC grace window 是 24 小時。
- 超過 grace 才算候選。
- `gc_dry_run` 會記錄 event_log。
- `gc_cutover` 也會記錄 event_log。
- 這讓 operator 可以先看 plan 再切 cutover。

## 12. GC 候選條件

- candidate 必須是 daemon-managed。
- candidate 不能被 pin。
- candidate 要能解析出 agent 名稱。
- candidate 不能還有 binding。
- candidate 必須有 `released_at`。
- `released_at` 必須超過 24 小時。
- 新 layout 與 legacy layout 都會被掃。
- 新 layout 是 `<home>/worktrees/<agent>/<branch>/`。
- legacy layout 是 `<home>/workspace/*/.worktrees/*/`。
- `evaluate_candidate()` 是真正的條件中心。
- 這讓 dry-run 與 cutover 共用判斷。
- 因此候選一致性比較好。

## 13. pin / unpin

- `.agend-pinned` 是人工保留標記。
- `pin()` 會寫入時間戳。
- `unpin()` 會移除 pin file。
- `is_pinned()` 只是檢查檔案是否存在。
- pin 是 operator override。
- 它用來阻止 GC。
- 這對長期保留的 worktree 很重要。
- pin 不會改 binding。
- pin 也不會改 released_at。
- 它只是 GC 的排除條件。

## 14. orphan 與 reconcile

- `reconcile_orphan_leases()` 是 boot-time log-only 掃描。
- 它會找 runtime binding.json。
- 若 worktree 不存在，會告警。
- 它不會直接刪除。
- 這是診斷用途。
- 這跟 GC 不一樣。
- GC 是刪。
- reconcile 是觀測。
- 如果 binding 與檔案系統不一致，先看這裡。
- 這有助於找出 stale registry state。

## 15. 安全與邊界

- 不要直接對 pool 外路徑做 remove_dir_all。
- 不要把 operator-created worktree 當 daemon-managed。
- 不要在未驗證 branch 的情況下 lease。
- 不要讓 protected branch 進 lease。
- 不要讓 release_full 在 marker 不存在時硬刪。
- 不要跳過 bind-in-flight 清理。
- 不要把 GC 當成 release 的替代品。
- release 是顯式釋放。
- GC 是延後回收。
- force_release_worktree 是 emergency recovery。

## 16. 使用流程

- 新任務先用 `repo action=checkout bind:true`。
- mid-lifecycle 恢復用 `bind_self`。
- 準備釋放時先用 `release_worktree`。
- 若還有殘留 dir，再用 `force_release_worktree`。
- 只想看候選，先跑 `gc_dry_run`。
- 確認無誤且要切回收，再設 `AGEND_WORKTREE_GC=1`。
- 若要保留工作樹，先 pin。
- 若要解除保留，再 unpin。
- 若要清理整條路徑，先確認 binding 與 marker。
- 如果看到 stale bind-in-flight，先看 release_full 路徑。

## 17. 實作檢查點

- canonical path 要一致。
- 新增行為要保留 daemon-managed marker。
- release 不應把 operator worktree 刪掉。
- GC 不應掃到還在 bind 的 worktree。
- GC grace 必須尊重 released_at。
- 舊 layout 只做相容，不做新創建。
- emergency cleanup 要維持 path safety。
- release_full 的 binding 清理不可漏。
- branch cleanup 必須看 protected set。
- 任何新入口都要對齊 worktree pool 語意。

## 18. 總結

- worktree 是 agent 與 branch 的隔離層。
- `repo action=checkout bind:true` 是新建綁定的主入口。
- `bind_self` 是 mid-lifecycle 重綁入口。
- `release_worktree` 是正式釋放入口。
- `force_release_worktree` 是 emergency recovery 入口。
- `repo action=release` 是 path-centric 釋放入口。
- `gc_dry_run` / `gc_cutover` 負責延遲回收。
- `.agend-managed`、`.agend-pinned`、binding.json 共同描述狀態。
- 如果 worktree 出問題，先判斷是 lease、release、還是 GC。
- 這個模組的目標是讓工作空間隔離、可回收、可恢復。
