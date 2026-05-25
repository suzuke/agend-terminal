# Skills 跨 Backend 技能系統

Skills 系統讓你將可重用的技能（prompt、工具定義、參考文件）安裝一次，所有 backend 的 agent 都能自動使用。不需要為每個 backend 分別設定。

## 使用情境

> **適用對象：** Operator 和 agent 皆適用。

**Operator 安裝 code review 技能。** 你在 GitHub 上找到一個社群維護的 code review 技能。執行 `agend-terminal skills add https://github.com/user/code-review-expert.git` 一次，fleet 中的每個 agent——無論它跑的是 Claude、Gemini 還是 Kiro——都能使用這個技能。不需要逐一為每個 backend 設定。

**Agent 啟動時載入技能。** 當 daemon 啟動一個 dev agent 時，skills 系統已經在 agent 的工作目錄下建立好 symlink。Agent 的 backend 從慣例路徑讀取 `SKILL.md`，取得技能的能力——prompt 範本、行為準則或參考資料——不需要任何手動載入步驟。

**Per-agent 篩選。** 你的 reviewer 只應該使用 review 相關的技能，不需要部署或重構技能。你在 fleet.yaml 的 reviewer 設定中加入 `skills: [code-review-expert]`。Daemon 會建立篩選後的 stage 目錄，reviewer 只看得到它需要的技能。

## 設計理念

不同的 AI backend（Claude、Codex、Gemini、OpenCode、Kiro）各有自己的技能目錄慣例（`.claude/skills/`、`.codex/skills/` 等）。手動在每個目錄下維護相同的技能檔案既繁瑣又容易不同步。

Skills 系統提供一個統一的來源目錄 `~/.agend-terminal/skills/`，透過 symlink 自動映射到每個 backend 的慣例路徑。安裝一次，五個 backend 同時生效。

---

## 快速開始

```bash
# 從 GitHub 安裝技能
agend-terminal skills add https://github.com/user/my-skill.git

# 從本地目錄安裝
agend-terminal skills add ~/projects/my-skill

# 列出已安裝的技能
agend-terminal skills list

# 更新技能
agend-terminal skills update my-skill

# 移除技能
agend-terminal skills remove my-skill
```

安裝後，下次啟動 agent 時技能會自動生效。

---

## 技能目錄結構

### 統一來源

所有技能儲存在單一目錄下：

```
~/.agend-terminal/
├── skills/                    ← 統一技能來源
│   ├── skill-forge/
│   │   ├── SKILL.md          ← 技能描述文件（必要）
│   │   └── [其他支援檔案]
│   ├── code-review-expert/
│   │   └── SKILL.md
│   └── ...
└── skills-lock.json           ← 版本鎖定記錄
```

### 每個 Agent 的映射

daemon 啟動 agent 時，自動在 agent 的工作目錄下建立 symlink：

```
<agent-working-dir>/
├── .claude/skills/   → ~/.agend-terminal/skills/
├── .codex/skills/    → ~/.agend-terminal/skills/
├── .gemini/skills/   → ~/.agend-terminal/skills/
├── .opencode/skills/ → ~/.agend-terminal/skills/
└── .kiro/skills/     → ~/.agend-terminal/skills/
```

每個 backend 啟動時讀取自己慣例路徑下的 `SKILL.md`，看到的都是同一份來源。

---

## CLI 命令

### `skills add <source>`

從 git repo 或本地路徑安裝技能。

```bash
# Git 來源（自動 shallow clone）
agend-terminal skills add https://github.com/user/skill-forge.git
agend-terminal skills add git@github.com:user/skill-forge.git

# 本地路徑（完整複製）
agend-terminal skills add /path/to/my-skill
agend-terminal skills add ./relative/path
```

來源類型自動判斷：URL 或 `.git` 結尾視為 git，其餘視為本地路徑。

安裝完成後，`skills-lock.json` 記錄來源和版本（git SHA 或檔案修改時間），供後續 `update` 使用。

重複安裝同名技能會覆蓋更新。

### `skills list`

列出所有已安裝的技能，包含來源和版本資訊。

```bash
agend-terminal skills list
```

輸出範例：

```
skill-forge
  source: https://github.com/user/skill-forge.git
  version: abc123d
  installed_at: 2026-05-16T10:00:00Z

code-review-expert
  source: /Users/suzuke/projects/code-review
  version: 1747402800
  installed_at: 2026-05-20T08:30:00Z
```

### `skills update [<name>]`

重新從原始來源拉取最新版本。

```bash
# 更新單一技能
agend-terminal skills update skill-forge

# 更新所有技能
agend-terminal skills update
```

Git 來源會重新 clone 取得最新 commit；本地路徑會重新複製。版本鎖定自動更新。

### `skills remove <name>`

移除技能及其鎖定記錄。

```bash
agend-terminal skills remove skill-forge
```

已移除的技能在下次 agent 啟動時不再可見。操作冪等——移除不存在的技能不會報錯。

### `skills install <working_dir>`

手動安裝技能到指定工作目錄。通常由 daemon 自動執行，此命令用於除錯或一次性設定。

```bash
agend-terminal skills install /tmp/test-agent-wd
```

---

## 撰寫技能

一個技能就是一個包含 `SKILL.md` 的目錄。`SKILL.md` 是 backend 讀取的進入點，格式為 Markdown。

### 最小結構

```
my-skill/
└── SKILL.md
```

### 帶支援檔案

```
my-skill/
├── SKILL.md           ← 主要描述文件
├── templates/
│   └── review.md      ← 範本檔案
└── examples/
    └── usage.py       ← 範例程式
```

`SKILL.md` 的內容由 backend 定義如何解析。以 Claude 為例，`SKILL.md` 通常包含技能說明、觸發條件、和提示內容。AgEnD Terminal 本身不解析 `SKILL.md` 內容——只負責將目錄送達正確位置。

---

## 每個 Agent 的技能篩選

透過 `fleet.yaml` 的 `skills` 欄位，可以控制每個 agent 看到哪些技能。

### 預設行為：安裝所有技能

```yaml
instances:
  dev:
    backend: claude
    # 沒有 skills 欄位 → 安裝所有技能
```

### 只安裝特定技能

```yaml
instances:
  reviewer:
    backend: claude
    skills:
      - code-review-expert
      - skill-forge
```

reviewer 只會看到 `code-review-expert` 和 `skill-forge`，其他技能對它不可見。

### 完全不安裝技能

```yaml
instances:
  eval-runner:
    backend: gemini
    skills: []    # 空陣列 = 不安裝任何技能
```

### 篩選實作原理

當 `skills` 指定了允許清單：

1. 系統計算允許清單的 SHA-256 摘要
2. 在 `~/.agend-terminal/.skills-stage/<digest>/` 建立篩選後的副本
3. Agent 的 symlink 指向篩選後的 stage 目錄，而非統一來源

Stage 目錄在 7 天後自動清理。相同的允許清單會重用同一個 stage 目錄。

---

## 自動安裝時機

Daemon 在以下時機自動為 agent 安裝技能：

| 時機 | 說明 |
|------|------|
| 冷啟動 | daemon 啟動時，在 spawn 每個 agent 前同步安裝 |
| 崩潰重啟 | agent crash 後 respawn 前重新安裝 |
| Stage 2 重啟 | 乾淨重啟流程中重新安裝 |
| TUI 新增 agent | 透過 `Ctrl+B c` 或命令面板新增時自動安裝 |

安裝是同步的——agent 啟動時 `SKILL.md` 已經就位。

---

## 安裝模式

### Symlink（預設）

Unix 系統上使用 symlink，零複製、即時反映來源變更。

### Copy + Marker（降級）

Windows 或 symlink 不可用時，完整複製目錄並寫入 `.agend-skills-managed` 標記檔案。

標記檔案的用途：
- 有標記 → daemon 管理的副本，可以安全覆蓋更新
- 沒有標記 → 使用者手動建立的技能目錄，daemon 不會覆蓋

---

## 版本鎖定

`skills-lock.json` 記錄每個技能的安裝資訊：

```json
{
  "skills": {
    "skill-forge": {
      "source": "https://github.com/user/skill-forge.git",
      "version": "abc123def456...",
      "installed_at": "2026-05-16T10:00:00Z"
    }
  }
}
```

- **source**：原始來源（`update` 時從這裡拉取）
- **version**：git commit SHA 或檔案修改時間戳
- **installed_at**：安裝時間

寫入使用 atomic write，不會因 crash 而損壞。

---

## 疑難排解

### 技能沒有生效

1. 確認技能目錄包含 `SKILL.md`
2. 用 `agend-terminal skills list` 確認技能已安裝
3. 檢查 agent 工作目錄下的 symlink 是否正確：
   ```bash
   ls -la <agent-wd>/.claude/skills/
   ```
4. 如果 fleet.yaml 有 `skills:` 允許清單，確認技能名稱在清單中

### 手動建立的技能目錄被跳過

這是正常行為。如果你在 agent 工作目錄下手動建立了 `.claude/skills/`，daemon 不會覆蓋它。如果想改由 daemon 管理，先手動刪除該目錄，daemon 下次啟動時會自動重建 symlink。

### Windows 上 symlink 失敗

Windows 預設需要開發者模式或管理員權限才能建立 symlink。系統會自動降級為 copy 模式。如果想使用 symlink：

1. 開啟「設定 → 開發人員選項 → 開發人員模式」
2. 或以管理員身分執行

### 更新後技能沒有更新

Symlink 模式下修改會即時反映。Copy 模式下需要重新執行 `agend-terminal skills update` 或重啟 daemon。
