[English](GITLAB-MIRROR-SETUP.md)

# GitLab Mirror Setup Guide — GitLab 鏡像設定指南

當 GitHub Actions 服務降級時，可透過 GitLab 作為備援 CI 通道使用。另請參閱 [CI-DOWN-SOP.md](CI-DOWN-SOP.zh-TW.md)。

## 1. 建立 GitLab 專案

1. 前往 [gitlab.com](https://gitlab.com) → **New project** → **Create blank project**
2. 名稱：`agend-terminal`（或與你的 GitHub repo 名稱一致）
3. 可見性：**Private**（建議——source of truth 留在 GitHub）
4. 取消勾選「Initialize repository with a README」
5. 點擊 **Create project**

## 2. 設定 Pull Mirror

GitLab 會依排程自動從 GitHub 拉取。

1. 在你的 GitLab 專案中：**Settings → Repository → Mirroring repositories**
2. Git repository URL：`https://github.com/suzuke/agend-terminal.git`
3. Mirror direction：**Pull**
4. Authentication method：**Password**——使用 GitHub Personal Access Token（PAT）
   - 在 [github.com/settings/tokens](https://github.com/settings/tokens) 建立 PAT
   - 需要的 scopes：`repo`（讀取 private repo）或 `public_repo`（若為 public）
   - 將 PAT 貼上作為密碼
5. 若只想鏡像 `main`，勾選 **Mirror only protected branches**；若要鏡像所有 branch 則保持不勾選
6. 點擊 **Mirror repository**

GitLab 預設每 **5 分鐘** 同步一次。你也可以點擊 **Update now** 強制立即同步。

## 3. 驗證 Mirror 正常運作

1. 推送一個 commit 到 GitHub
2. 等待最多 5 分鐘（或在 GitLab mirror 設定中點擊 **Update now**）
3. 確認該 commit 出現在 GitLab 的 repository 檢視中
4. 檢查 **CI/CD → Pipelines**——repo 中的 `.gitlab-ci.yml` 應會自動觸發一條 pipeline

## 4. 在 GitHub Actions 停擺期間檢查 GitLab CI

當 GitHub Actions 服務降級時：

1. 前往你的 GitLab 專案 → **CI/CD → Pipelines**
2. 找到你需要驗證的 branch 對應的 pipeline
3. 確認 `fmt`、`clippy` 和 `test` jobs 全部通過
4. 將結果作為 merge 證據，填入 [CI-DOWN-SOP](CI-DOWN-SOP.zh-TW.md) 的 PR comment 模板中

## 5. 選用：將狀態回報給 GitHub

若要將 GitLab CI 結果以 commit status 的形式顯示在 GitHub PR 上：

1. 建立一個帶有 `repo:status` scope 的 GitHub PAT
2. 在 GitLab 中：**Settings → CI/CD → Variables** → 新增 `GITHUB_STATUS_TOKEN`（masked）
3. 在 `.gitlab-ci.yml` 中加入一段 `after_script`：

```yaml
after_script:
  - |
    curl -s -X POST \
      -H "Authorization: token $GITHUB_STATUS_TOKEN" \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/suzuke/agend-terminal/statuses/$CI_COMMIT_SHA" \
      -d "{\"state\":\"$([ $CI_JOB_STATUS = success ] && echo success || echo failure)\",\"target_url\":\"$CI_PIPELINE_URL\",\"context\":\"gitlab-ci/$CI_JOB_NAME\"}"
```

這是選用的——在正常運作期間，GitHub Actions 仍是主要 CI。