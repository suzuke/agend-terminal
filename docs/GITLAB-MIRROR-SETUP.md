[繁體中文](GITLAB-MIRROR-SETUP.zh-TW.md)

# GitLab Mirror Setup Guide

Backup CI channel via GitLab for use when GitHub Actions is degraded. See also [CI-DOWN-SOP.md](CI-DOWN-SOP.md).

## 1. Create GitLab Project

1. Go to [gitlab.com](https://gitlab.com) → **New project** → **Create blank project**
2. Name: `agend-terminal` (or match your GitHub repo name)
3. Visibility: **Private** (recommended — source of truth stays on GitHub)
4. Uncheck "Initialize repository with a README"
5. Click **Create project**

## 2. Configure Pull Mirror

GitLab will automatically pull from GitHub on a schedule.

1. In your GitLab project: **Settings → Repository → Mirroring repositories**
2. Git repository URL: `https://github.com/suzuke/agend-terminal.git`
3. Mirror direction: **Pull**
4. Authentication method: **Password** — use a GitHub Personal Access Token (PAT)
   - Create PAT at [github.com/settings/tokens](https://github.com/settings/tokens)
   - Scopes needed: `repo` (read access to private repos) or `public_repo` (if public)
   - Paste the PAT as the password
5. Check **Mirror only protected branches** if you only want `main`, or leave unchecked to mirror all branches
6. Click **Mirror repository**

GitLab syncs every **5 minutes** by default. You can also click **Update now** to force an immediate sync.

## 3. Verify Mirror Works

1. Push a commit to GitHub
2. Wait up to 5 minutes (or click **Update now** in GitLab mirror settings)
3. Confirm the commit appears in GitLab's repository view
4. Check **CI/CD → Pipelines** — a pipeline should trigger automatically from the `.gitlab-ci.yml` in the repo

## 4. Check GitLab CI During GitHub Actions Downtime

When GitHub Actions is degraded:

1. Go to your GitLab project → **CI/CD → Pipelines**
2. Find the pipeline for the branch you need to verify
3. Check that `fmt`, `clippy`, and `test` jobs all pass
4. Use the result as merge evidence in the [CI-DOWN-SOP](CI-DOWN-SOP.md) PR comment template

## 5. Optional: Report Status Back to GitHub

To show GitLab CI results as commit statuses on GitHub PRs:

1. Create a GitHub PAT with `repo:status` scope
2. In GitLab: **Settings → CI/CD → Variables** → add `GITHUB_STATUS_TOKEN` (masked)
3. Add an `after_script` to `.gitlab-ci.yml`:

```yaml
after_script:
  - |
    curl -s -X POST \
      -H "Authorization: token $GITHUB_STATUS_TOKEN" \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/suzuke/agend-terminal/statuses/$CI_COMMIT_SHA" \
      -d "{\"state\":\"$([ $CI_JOB_STATUS = success ] && echo success || echo failure)\",\"target_url\":\"$CI_PIPELINE_URL\",\"context\":\"gitlab-ci/$CI_JOB_NAME\"}"
```

This is optional — during normal operations GitHub Actions is the primary CI.