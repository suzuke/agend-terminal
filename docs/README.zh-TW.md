[English](README.md)

# AgEnD Terminal — 文件總覽

[專案 README](../README.zh-TW.md)以外的所有文件，依主題分類。每一份 repository
自有的 Markdown 都以英文／繁體中文成對維護；任一語言都不能在不知不覺中少掉
section、table、code block 或 link target。

> 第一次接觸？先看[快速開始](FEATURE-quickstart.zh-TW.md)，再讀
> [Fleet 設定](FEATURE-fleet.zh-TW.md)。

## 擺放與維護政策

- Repository root 只放 `README`、`CONTRIBUTING`、
  `CHANGELOG` 與 runtime `CLAUDE` 這四組。
- 一般維護中文件平坦放在 `docs/`。`FEATURE-` 等穩定 filename
  prefix 用來辨識文件家族，避免為了目錄層級製造大量 link churn。
- Platform-owned template 留在 `.github/`；loader-owned specification
  與 consumer 同置於 `skills/` 或 `tests/fixtures/`。
- 每個英文 `name.md` 都有同目錄的 `name.zh-TW.md`，並具備雙向
  language navigation 與一致結構。
- `vendor/agentic-git` 是獨立管理文件的 pinned git submodule，不屬於
  superproject 文件。
- Plan、audit、review snapshot 與已完成 incident note 不留在 active tree；
  仍有效的規則必須併入真正擁有該主題的維護中文件。

`docs_bilingual_invariant` integration test 會強制 placement、pairing、
navigation、heading、code fence、table、link target 與 index 規則。自然語言品質
仍需要 bilingual review。

## 歷史紀錄

Git、pull request 與 issue 才是 history system；目前 branch 不再維護第二份
archive。文件整理前的最後一棵樹是：

```sh
git show fb7ba195854e12d787d8daf9999c2dd128b7bfd2:<former-path>
```

任何原本位於 `docs/archived/`、`docs/audit/`、
`docs/design/` 或已移除 point-in-time 文件，都可用該命令還原。決策軌跡請查
artifact 內提到的 issue/PR，或執行 `git log -- <former-path>`。歷史文字只是
「當時相信什麼」的 evidence，不是現行契約。

## 入門

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| 快速開始 | [EN](FEATURE-quickstart.md) | [中文](FEATURE-quickstart.zh-TW.md) | 從安裝到第一個 live fleet |
| Fleet 設定 | [EN](FEATURE-fleet.md) | [中文](FEATURE-fleet.zh-TW.md) | `fleet.yaml`、backend、role、team 與 workspace |
| 使用指南 | [EN](USAGE.md) | [中文](USAGE.zh-TW.md) | 日常操作與常見流程 |
| CLI 參考 | [EN](CLI.md) | [中文](CLI.zh-TW.md) | Command 與 flag |

## 功能指南

### 核心

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| Agent 互動 | [EN](FEATURE-agent-interaction.md) | [中文](FEATURE-agent-interaction.zh-TW.md) | Input、output、prompt 與 capture |
| TUI 介面 | [EN](FEATURE-tui.md) | [中文](FEATURE-tui.zh-TW.md) | Pane、tab、render 與 keybinding |
| Skills 系統 | [EN](FEATURE-skills.md) | [中文](FEATURE-skills.zh-TW.md) | 安裝與使用 skill |
| 通訊 | [EN](FEATURE-communication.md) | [中文](FEATURE-communication.zh-TW.md) | `send`、`inbox` 與 `reply` |
| 任務看板 | [EN](FEATURE-task-board.md) | [中文](FEATURE-task-board.zh-TW.md) | Durable work tracking |
| 團隊 | [EN](FEATURE-teams.md) | [中文](FEATURE-teams.zh-TW.md) | Agent 分組與協調範圍 |
| Worktree 隔離 | [EN](FEATURE-worktree.md) | [中文](FEATURE-worktree.zh-TW.md) | Branch-bound managed worktree |

### 進階

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| CI Watch | [EN](FEATURE-ci-watch.md) | [中文](FEATURE-ci-watch.zh-TW.md) | Hosted-CI monitoring 與 handoff |
| 健康與監控 | [EN](FEATURE-health.md) | [中文](FEATURE-health.zh-TW.md) | Liveness、hung detection 與 blocked reason |
| Dispatch Idle | [EN](FEATURE-dispatch-idle.md) | [中文](FEATURE-dispatch-idle.zh-TW.md) | Idle work pickup 與 tracking |
| Channels | [EN](FEATURE-channels.md) | [中文](FEATURE-channels.zh-TW.md) | Telegram 與 Discord adapter |
| 決策 | [EN](FEATURE-decisions.md) | [中文](FEATURE-decisions.zh-TW.md) | Scope decision 與 correction |
| 排程與部署 | [EN](FEATURE-schedules.md) | [中文](FEATURE-schedules.zh-TW.md) | Recurring work 與 deployment template |

### 維運

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| 服務管理 | [EN](FEATURE-service.md) | [中文](FEATURE-service.zh-TW.md) | Launch path 與 OS supervisor |
| 診斷 | [EN](FEATURE-diagnostics.md) | [中文](FEATURE-diagnostics.zh-TW.md) | Log、snapshot 與 troubleshooting |
| 設定 | [EN](FEATURE-configuration.md) | [中文](FEATURE-configuration.zh-TW.md) | Runtime setting 與 ownership |

## 架構與參考

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| 架構地圖 | [EN](architecture.md) | [中文](architecture.zh-TW.md) | 現行 subsystem 與閱讀路徑 |
| Architecture-14 台帳 | [EN](ARCHITECTURE-14-LEDGER.md) | [中文](ARCHITECTURE-14-LEDGER.zh-TW.md) | 現行 convergence outcome 與 evidence |
| Daemon Lock Ordering | [EN](DAEMON-LOCK-ORDERING.md) | [中文](DAEMON-LOCK-ORDERING.zh-TW.md) | Lock hierarchy 與 deadlock prevention |
| Hung-State 契約 | [EN](HUNG-STATE-TRANSITIONS.md) | [中文](HUNG-STATE-TRANSITIONS.zh-TW.md) | Detection transition 與 productive-output gate |
| Recovery Stages | [EN](RECOVERY-STAGES.md) | [中文](RECOVERY-STAGES.zh-TW.md) | 分階段 automatic recovery |
| Backend Matrix | [EN](BACKEND-CAPABILITY-MATRIX.md) | [中文](BACKEND-CAPABILITY-MATRIX.zh-TW.md) | Backend signal、provider、resume 與 MCP |
| MCP Tools | [EN](MCP-TOOLS.md) | [中文](MCP-TOOLS.zh-TW.md) | Tool registry 與 bridge/proxy 契約 |
| 環境變數 | [EN](env-vars.md) | [中文](env-vars.zh-TW.md) | 支援的 environment contract |
| Git 行為 | [EN](GIT-BEHAVIOR.md) | [中文](GIT-BEHAVIOR.zh-TW.md) | Agent git policy 與 shim 行為 |
| 相容性 | [EN](COMPATIBILITY.md) | [中文](COMPATIBILITY.zh-TW.md) | On-disk format 保證 |
| 已知問題 | [EN](KNOWN_ISSUES.md) | [中文](KNOWN_ISSUES.zh-TW.md) | 刻意延後的 user-visible issue |
| Skills 參考 | [EN](SKILLS.md) | [中文](SKILLS.zh-TW.md) | Skill catalog 與 lock format |
| Source of Truth | [EN](SOURCE-OF-TRUTH.md) | [中文](SOURCE-OF-TRUTH.zh-TW.md) | Code、docs 與 evidence authority |

## 維運與治理

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| Fleet 開發協定 | [EN](FLEET-DEV-PROTOCOL.md) | [中文](FLEET-DEV-PROTOCOL.zh-TW.md) | Normative multi-agent workflow |
| Solo Profile | [EN](SOLO-PROFILE.md) | [中文](SOLO-PROFILE.zh-TW.md) | 單 agent 的比例化應用 |
| Incident Runbook | [EN](RUNBOOK.md) | [中文](RUNBOOK.zh-TW.md) | Incident、CI outage 與 clean-agent recipe |
| GitLab Mirror | [EN](GITLAB-MIRROR-SETUP.md) | [中文](GITLAB-MIRROR-SETUP.zh-TW.md) | Backup CI mirror 設定 |
| 發布 | [EN](RELEASING.md) | [中文](RELEASING.zh-TW.md) | Versioning、publish 與 smoke check |

## Repository 與同置文件

| 文件 | EN | 中文 | 位置 |
|---|---|---|---|
| Project README | [EN](../README.md) | [中文](../README.zh-TW.md) | repository root |
| Contributing | [EN](../CONTRIBUTING.md) | [中文](../CONTRIBUTING.zh-TW.md) | repository root |
| Changelog | [EN](../CHANGELOG.md) | [中文](../CHANGELOG.zh-TW.md) | repository root |
| Claude Runtime Notes | [EN](../CLAUDE.md) | [中文](../CLAUDE.zh-TW.md) | repository root |
| Telegram Setup Skill | [EN](../skills/setup-telegram/SKILL.md) | [中文](../skills/setup-telegram/SKILL.zh-TW.md) | loader-owned skill directory |
| Fixture Capture Playbook | [EN](../tests/fixtures/state-replay/CAPTURE-RECIPES.md) | [中文](../tests/fixtures/state-replay/CAPTURE-RECIPES.zh-TW.md) | fixture directory |
| Bug Template | [EN](../.github/ISSUE_TEMPLATE/bug_report.md) | [中文](../.github/ISSUE_TEMPLATE/bug_report.zh-TW.md) | GitHub convention |
| Feature Template | [EN](../.github/ISSUE_TEMPLATE/feature_request.md) | [中文](../.github/ISSUE_TEMPLATE/feature_request.zh-TW.md) | GitHub convention |
| Pull Request Template | [EN](../.github/PULL_REQUEST_TEMPLATE.md) | [中文](../.github/PULL_REQUEST_TEMPLATE.zh-TW.md) | GitHub convention |
