[English](START-HERE.md)

# 從這裡開始

> **Status：** 目前目標路由
> **Audience：** Operator、agent、reviewer 與 incident responder
> **Authority：** 下方連結的主題指南，加上 protected-main source/test
> **Last verified：** 2026-09-22，`main@62b28f36`

依照目前目標選擇路徑。先開一份指南；只有需要更多細節時才沿著連結繼續。

## Operator

**REQUIRED：** 先看[快速開始](FEATURE-quickstart.zh-TW.md)，再看[Fleet 設定](FEATURE-fleet.zh-TW.md)。
命令請查 [CLI](CLI.zh-TW.md)，日常操作請查[使用指南](USAGE.zh-TW.md)。

**STOP：** 如果是 production symptom，請改走下方 Incident 路徑，不要從 feature guide
直接修改 fleet state。

## Agent

**REQUIRED：** 閱讀[Fleet 開發協定](FLEET-DEV-PROTOCOL.zh-TW.md)。接著用[通訊](FEATURE-communication.zh-TW.md)
處理訊息、用[Task Board](FEATURE-task-board.zh-TW.md)追蹤 durable work，並用[Worktree 隔離](FEATURE-worktree.zh-TW.md)
處理 branch-bound work。

**OPTIONAL：** 需要精確 action 或欄位時，查 [MCP 工具](MCP-TOOLS.zh-TW.md)。

## Reviewer

**REQUIRED：** 先看[真相源矩陣](SOURCE-OF-TRUTH.zh-TW.md)，再核對[MCP 工具](MCP-TOOLS.zh-TW.md)與相關 feature guide。

**OPTIONAL：** 驗證 operational claim 時，可查[健康狀態](FEATURE-health.zh-TW.md)、[Recovery Stages](RECOVERY-STAGES.zh-TW.md)，
以及 bilingual invariant。

## Incident

**REQUIRED：** 先看[Runbook](RUNBOOK.zh-TW.md)，再看[Diagnostics](FEATURE-diagnostics.zh-TW.md)與[健康狀態](FEATURE-health.zh-TW.md)。
遇到 restart 或 hung-agent 行為時，閱讀[Recovery Stages](RECOVERY-STAGES.zh-TW.md)與[Worktree 隔離](FEATURE-worktree.zh-TW.md)。

**STOP：** 歷史稽核 section 只能當 evidence，不能當成目前操作指引。
