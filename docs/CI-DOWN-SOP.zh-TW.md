[English](CI-DOWN-SOP.md)

# CI-Down SOP：GitHub Actions 降級應變流程

當 GitHub Actions 降級或發生中斷時的標準作業流程。

## 1. 觸發條件

啟動本 SOP 前，以下兩個條件都必須滿足：

- **A.** 同一個 repo 有 **>=2 個 PR** 在 push 後 **10 分鐘** 仍沒有任何 workflow 執行
- **B.** [githubstatus.com](https://www.githubstatus.com) 顯示 Actions **degraded** 或 **outage**

如果只滿足其中一個條件，先等待並重新檢查，再決定是否升級處理。

## 2. 凍結 merge 與本地診斷

啟用本 SOP 期間，**不得 merge 受影響的 PR**。本地通過是有用的診斷證據，
但不是 CI 證據，也不會授權使用 `--admin`、`--force` 或其他方式繞過 merge gate。

在該 PR 的 worktree 裡執行以下三個命令，全部都必須通過。

```bash
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

**注意：** 這只涵蓋本機平台上的編譯、單元／整合測試、lint 和格式檢查，
不能取代 repo 的 hosted 跨平台檢查。

## 3. PR 留言範本

本地驗證通過後，在 PR 上貼出這則留言：

```
## Local CI Verification (GitHub Actions degraded)

- [x] `cargo test --all` — passed (N tests)
- [x] `cargo clippy --all-targets -- -D warnings` — clean
- [x] `cargo fmt --check` — clean
- [ ] Hosted cross-platform CI — blocked by the current Actions incident

GitHub Status: [degraded since HH:MM UTC](https://githubstatus.com)
Verified by: @agent-name

Merge status: FROZEN until CI runs on this PR head and all required checks pass.
```

把 `N tests`、`HH:MM UTC` 和 `@agent-name` 換成實際的值。

## 4. Merge 流程

1. 記錄受影響 PR、branch 與不可變的 PR head SHA，並保持 PR 開啟。
2. 執行三項本地診斷，並用上面的範本貼出結果。
3. Actions 降級期間持續凍結 merge；本地綠燈與已完成的 review 都不會放寬 CI gate。
4. Actions 恢復後，確認它對同一個 PR head 執行。必要時重新註冊該 branch 的
   `ci` watch；若 outage 期間完全沒建立 workflow run，請對未變更的 PR branch
   觸發 workflow，而不是先 merge。
5. 獨立執行 `gh pr checks <PR#>`。只有命令以 0 結束、所有 required check
   都成功，且 review／verdict gate 也成立時，PR 才具備 merge 資格。

## 5. 恢復之後

不得把稍後的 `main` run 當成尚未驗證之 PR 的補驗證。每個被凍結的 PR 都要從
記錄的 head SHA 恢復，取得該 PR 完整的 CI 結果，再走正常 merge 流程。若 outage
期間 head 已改變，先前的 review 已 stale，必須針對新 head 重新審查。
