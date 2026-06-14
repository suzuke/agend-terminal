[English](CI-DOWN-SOP.md)

# CI-Down SOP：GitHub Actions 降級應變流程

當 GitHub Actions 降級或發生中斷時的標準作業流程。

## 1. 觸發條件

啟動本 SOP 前，以下兩個條件都必須滿足：

- **A.** 同一個 repo 有 **>=2 個 PR** 在 push 後 **10 分鐘** 仍沒有任何 workflow 執行
- **B.** [githubstatus.com](https://www.githubstatus.com) 顯示 Actions **degraded** 或 **outage**

如果只滿足其中一個條件，先等待並重新檢查，再決定是否升級處理。

## 2. 本地測試關卡

在該 PR 的 worktree 裡執行以下三個命令，全部都必須通過。

```bash
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

**注意：** 這涵蓋編譯、單元/整合測試、lint 和格式檢查。跨平台驗證（Windows/Linux）延後到恢復之後再做。

## 3. PR 留言範本

本地驗證通過後，在 PR 上貼出這則留言：

```
## Local CI Verification (GitHub Actions degraded)

- [x] `cargo test --all` — passed (N tests)
- [x] `cargo clippy --all-targets -- -D warnings` — clean
- [x] `cargo fmt --check` — clean
- [ ] Cross-platform (deferred to post-recovery)

GitHub Status: [degraded since HH:MM UTC](https://githubstatus.com)
Verified by: @agent-name
```

把 `N tests`、`HH:MM UTC` 和 `@agent-name` 換成實際的值。

## 4. Merge 流程

1. Agent 執行 3 個本地測試關卡命令，並用上面的範本貼出 PR 留言
2. Lead 確認 review 已完成且本地驗證已貼出 → `gh pr merge --admin`
3. Actions 恢復後，main 分支的 CI 會自動補做驗證

## 5. 恢復之後

不需要任何特別處置。main 分支的 CI pipeline 會在每次 push 到 main 時執行，因此 Actions 恢復後，已 merge 的 PR 都會自動完成驗證。