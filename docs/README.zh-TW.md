[English](README.md)

# AgEnD Terminal — 文件總覽

[專案 README](../README.zh-TW.md) 以外的所有文件，依主題分類。多數面向使用者的
指南都有雙語版本——自由選擇 **EN** 或 **中文**。內部設計與流程筆記僅有英文。

> 第一次接觸？先看[快速開始指南](FEATURE-quickstart.zh-TW.md)，再讀
> [Fleet 設定](FEATURE-fleet.zh-TW.md)參考。

## 入門

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| 快速開始 | [EN](FEATURE-quickstart.md) | [中文](FEATURE-quickstart.zh-TW.md) | 首次啟動逐步教學，從安裝到跑起一個 fleet |
| Fleet 設定 | [EN](FEATURE-fleet.md) | [中文](FEATURE-fleet.zh-TW.md) | `fleet.yaml` 結構——backend、role、team、工作目錄 |
| 使用指南 | [EN](USAGE.md) | [中文](USAGE.zh-TW.md) | 日常操作、Telegram 綁定、常見流程 |
| CLI 參考 | [EN](CLI.md) | [中文](CLI.zh-TW.md) | 每一個 `agend-terminal` 子命令與旗標 |

## 功能指南

### 核心

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| Agent 互動 | [EN](FEATURE-agent-interaction.md) | [中文](FEATURE-agent-interaction.zh-TW.md) | 操控 agent：輸入注入、輸出擷取、prompt |
| TUI 介面 | [EN](FEATURE-tui.md) | [中文](FEATURE-tui.zh-TW.md) | 多 pane 終端 UI、pane、tab、快捷鍵 |
| Skills 技能系統 | [EN](FEATURE-skills.md) | [中文](FEATURE-skills.zh-TW.md) | 跨 backend 安裝與使用 skill |
| 通訊系統 | [EN](FEATURE-communication.md) | [中文](FEATURE-communication.zh-TW.md) | `send`／`inbox`／`reply`——agent 如何對話 |
| 任務看板 | [EN](FEATURE-task-board.md) | [中文](FEATURE-task-board.zh-TW.md) | 共享的工作追蹤：create、claim、done |
| 團隊 | [EN](FEATURE-teams.md) | [中文](FEATURE-teams.zh-TW.md) | 把 agent 分組並界定協調範圍 |
| Git Worktree 隔離 | [EN](FEATURE-worktree.md) | [中文](FEATURE-worktree.zh-TW.md) | per-agent worktree 與其池化方式 |

### 進階

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| CI 監控 | [EN](FEATURE-ci-watch.md) | [中文](FEATURE-ci-watch.zh-TW.md) | 從 fleet 監看 GitHub Actions |
| 健康與監控 | [EN](FEATURE-health.md) | [中文](FEATURE-health.zh-TW.md) | 存活偵測、hung 偵測、blocked-reason 回報 |
| Dispatch Idle 追蹤 | [EN](FEATURE-dispatch-idle.md) | [中文](FEATURE-dispatch-idle.zh-TW.md) | idle agent 何時、如何接手工作 |
| 頻道（Telegram／Discord） | [EN](FEATURE-channels.md) | [中文](FEATURE-channels.zh-TW.md) | 透過聊天工具遠端操控與接收通知 |
| 決策記錄 | [EN](FEATURE-decisions.md) | [中文](FEATURE-decisions.zh-TW.md) | 記錄 scope 決策與更正 |
| 排程與部署 | [EN](FEATURE-schedules.md) | [中文](FEATURE-schedules.zh-TW.md) | cron 式例行任務與模板部署 |

### 維運

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| 服務管理 | [EN](FEATURE-service.md) | [中文](FEATURE-service.zh-TW.md) | 把 daemon 安裝成系統服務 |
| 診斷工具 | [EN](FEATURE-diagnostics.md) | [中文](FEATURE-diagnostics.zh-TW.md) | log、snapshot 與疑難排解工具 |
| 設定 | [EN](FEATURE-configuration.md) | [中文](FEATURE-configuration.zh-TW.md) | 執行期設定旋鈕與其所在位置 |

## 參考

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| 架構 | [EN](architecture.md) | [中文](architecture.zh-TW.md) | daemon 設計、worktree pool、健康監控、channel 生命週期 |
| MCP 工具 | [EN](MCP-TOOLS.md) | [中文](MCP-TOOLS.zh-TW.md) | 全部 30 個 agent 協調 MCP 工具，依類別列出 |
| 環境變數 | [EN](env-vars.md) | [中文](env-vars.zh-TW.md) | 每一個 `AGEND_*` 變數與其作用 |
| Git 行為 | [EN](GIT-BEHAVIOR.md) | [中文](GIT-BEHAVIOR.zh-TW.md) | daemon 對被啟動 agent 的 git 環境做了哪些修改 |
| 相容性政策 | [EN](COMPATIBILITY.md) | [中文](COMPATIBILITY.zh-TW.md) | `$AGEND_HOME` 底下磁碟格式的穩定性保證 |
| 已知問題 | [EN](KNOWN_ISSUES.md) | [中文](KNOWN_ISSUES.zh-TW.md) | 刻意暫緩的項目——開 issue 前請先看 |
| Skills 參考 | [EN](SKILLS.md) | [中文](SKILLS.zh-TW.md) | skill 目錄與 lock 格式 |
| 啟動流程 | [EN](launch-flows.md) | [中文](launch-flows.zh-TW.md) | 啟動 daemon 的每一種方式；冷啟動 vs 熱啟動 |
| 秘訣：乾淨的 Claude Instance | [EN](RECIPE-clean-claude-instance.md) | [中文](RECIPE-clean-claude-instance.zh-TW.md) | 啟動不含全域指令的 Claude Code agent |
| 競品比較 | [EN](competitor-comparison.md) | [中文](competitor-comparison.zh-TW.md) | AgEnD-Terminal 與類似工具的比較 |

## 貢獻與維運

| 文件 | EN | 中文 | 內容 |
|---|---|---|---|
| 貢獻指南 | [EN](../CONTRIBUTING.md) | [中文](../CONTRIBUTING.zh-TW.md) | 開發環境設定、branch/PR 流程、review 期待 |
| 發布 | [EN](RELEASING.md) | [中文](RELEASING.zh-TW.md) | 切版本、versioning、發布 |
| 發布 Smoke 檢查清單 | [EN](release-smoke-checklist.md) | [中文](release-smoke-checklist.zh-TW.md) | 發布前的手動檢查 |
| Runbook | [EN](RUNBOOK.md) | [中文](RUNBOOK.zh-TW.md) | 常見事故的維運手冊 |
| CI 中斷 SOP | [EN](CI-DOWN-SOP.md) | [中文](CI-DOWN-SOP.zh-TW.md) | CI 無法使用時該怎麼辦 |
| GitLab Mirror 設定 | [EN](GITLAB-MIRROR-SETUP.md) | [中文](GITLAB-MIRROR-SETUP.zh-TW.md) | 把 repo 鏡像到 GitLab CI |
| Lint 紀律 | [EN](LINT-DISCIPLINE.md) | [中文](LINT-DISCIPLINE.zh-TW.md) | CI 強制的 clippy／rustfmt 慣例 |
| 更新日誌 | [EN](../CHANGELOG.md) | [中文](../CHANGELOG.zh-TW.md) | 逐版本變更歷史 |

## 內部設計與流程

深入的設計筆記與多 agent 開發協定。這些文件記錄了在程式碼庫內部工作的貢獻者所需的
實作細節，僅以英文維護。

<details>
<summary><strong>架構深入探討</strong></summary>

- [Architecture Groups](ARCHITECTURE-GROUPS.md) — 原始碼樹的子系統分組
- [Architecture Quick Start](ARCHITECTURE-QUICK-START.md) — 新貢獻者的入門導覽
- [Daemon Lock Ordering](DAEMON-LOCK-ORDERING.md) — 鎖的取得順序與死鎖避免
- [Hung State Transitions](HUNG-STATE-TRANSITIONS.md) — hung 偵測狀態機
- [Recovery Stages](RECOVERY-STAGES.md) — crash 復原的分段
- [MCP↔Daemon Proxy Contract](MCP-DAEMON-PROXY-CONTRACT.md) — bridge 協定
- [Skill System Architecture](skill-system-architecture.md) — skill 如何依 backend 解析
- [Project Feature Groups](project-feature-groups.md) — 功能對模組的對應
- [Loop Engineering Mapping](loop-engineering-mapping.md) — supervisor loop 職責
- [F685 Fixture Corpus](F685-FIXTURE-CORPUS.md) — state-replay fixture 語料庫
- [F9 Productive-Output Gate](F9-PRODUCTIVE-OUTPUT-GATE.md) — productive-output gate 設計
</details>

<details>
<summary><strong>Fleet 開發協定與流程</strong></summary>

- [Fleet Dev Protocol](FLEET-DEV-PROTOCOL.md) — 多 agent 協調契約
- [Reviewer Contract v0.1](REVIEWER-CONTRACT-v0.1.md) — reviewer 的 verdict／evidence 規則
- [Protocol: Verdict Delivery Confirmation](PROTOCOL-VERDICT-DELIVERY-CONFIRMATION.md)
- [Protocol: Parallel Filler Opt-In Schema](PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md)
- [Process: Lead Closeout & Claim-State Discipline](PROCESS-LEAD-CLOSEOUT-CLAIM-STATE-DISCIPLINE.md)
- [Process: LOC Estimation Methodology](PROCESS-LOC-ESTIMATION-METHODOLOGY.md)
- [Process: systemd / loginctl Operator Hardening](PROCESS-SYSTEMD-LOGINCTL-OPERATOR-HARDENING.md)
- [Process: Tier-2 Dual-Review Lessons Learned](PROCESS-TIER2-DUAL-REVIEW-LESSONS-LEARNED.md)
- [Refactor Plan](REFACTOR-PLAN.md) — 進行中的架構整理計畫
</details>

> 已被取代與點時間性的文件存放於 [`archived/`](archived/)。
