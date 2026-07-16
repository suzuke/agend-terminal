---
name: Bug 回報
about: 附有親見證據、使用者可察覺的問題
title: 'bug: '
labels: ['bug']
---

[English](bug_report.md)

## 既有討論檢查（必填）

<!-- 若回報內容與既有（未結案或已結案）issue 重複，或在沒有新證據的情況下
重新提出已解決的議題，可能會直接關閉而不再 review。先快速搜尋可節省大家的時間。 -->

- [ ] 我已查看 [docs/KNOWN_ISSUES.md](https://github.com/suzuke/agend-terminal/blob/main/docs/KNOWN_ISSUES.md)——這不是其中已列為刻意延後處理的項目。
- [ ] 我已搜尋**未結案與已結案**的 issue，確認是否有先前回報或結論，並在下方連結找到的內容。
- [ ] 這不是已解決 issue 的重複回報；若是重新檢視既有 issue，我已說明哪些新證據或變更值得重新開啟。

## 症狀

<!-- 用一句話說明使用者／operator 看見了什麼問題。
請描述問題，不要描述修正方式。 -->

## 重現方式

<!-- 觸發問題所需的最少步驟；最好可以直接複製貼上。
範例：
1. `agend-terminal start --agents foo:claude --foreground`
2. 等待 Telegram topic（約 3 秒）
3. 檢查 `$AGEND_HOME/fleet.yaml` → 預期 topic_id=<n>，實際為 null

若無法穩定重現，請標記為「間歇性」，並描述你觀察到的觸發條件。 -->

## 預期結果與實際結果

<!-- 預期：……
     實際：…… -->

## 版本／環境

<!-- - agend-terminal：`cargo pkgid` 或 commit SHA（`git rev-parse HEAD`）
     - daemon build：daemon 啟動 log 第一行的 commit
     - OS：macOS 14.5 / Linux Ubuntu 24.04 / Windows 11 / ……
     - Backend（若相關）：claude-code 2.1.81 / codex-cli x.x /
       kiro-cli x.x / opencode x.x / agy x.x / grok x.x
     - Shell：zsh / bash / fish -->

## 具體證據

<!-- 親見時間戳（UTC）、log 行、PR／commit ref、截圖，以及相關的
inbox / fleet.yaml / topics.json 片段。
原始輸出請保持逐字不變；轉述時盡量少用術語。 -->

## 根本原因（若已知）

<!-- bug 所在的 file:line。若尚未追查到，可略過本節——operator 可能會
另行派發根因調查。 -->

## 引入時間（若已知）

<!-- git blame 或 PR ref。若是全新的 bug，可略過。 -->

## 建議修正（若有）

<!-- 只需簡述方向。不要預先定死實作——實作細節留到 PR 階段討論。 -->

## 優先級提示

<!-- P1  阻擋 operator／資料遺失／靜默失敗類
     P2  降低品質，但有替代做法
     P3  清理／潤飾／潛在風險 -->
