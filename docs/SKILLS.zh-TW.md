[English](SKILLS.md)

# Skills

為 agend-terminal 支援的五種 backend（Claude Code、Codex、Gemini CLI、OpenCode、Kiro CLI）提供統一的社群 skill 探索機制。

一個 skill 就是一個目錄（通常包含 `SKILL.md` 加上相關的支援檔案），agent 的 backend 會在啟動時載入它。agend-terminal 將每個 skill 的**單一副本**存放在 `~/.agend-terminal/skills/` 之下，並讓每個 backend 透過各自慣用的子目錄探索到它——通常是用 symlink，在 Windows 上則退而使用複製。

## 設計初衷

不同 backend 各自有自己的 skill 探索路徑：

| Backend | 工作目錄內的探索路徑 |
|---------|------------------------------------------|
| Claude  | `.claude/skills/`                        |
| Codex   | `.codex/skills/`                         |
| Gemini  | `.gemini/skills/`                        |
| OpenCode| `.opencode/skills/`                      |
| Kiro    | `.kiro/skills/`                          |

沒有 agend-terminal 時，你得為每個 agent、每個 backend 的目錄都各複製一份所有 skill。agend-terminal 只存一份標準來源，並自動把它呈現給每個 backend。

## Architecture

```
~/.agend-terminal/skills/                ← single source of truth
  ├── skill-forge/
  │   └── SKILL.md
  ├── opencli-adapter-author/
  │   └── SKILL.md
  └── ...

agent working directory/
  ├── .claude/skills/   → symlink → ~/.agend-terminal/skills/
  ├── .codex/skills/    → symlink → ~/.agend-terminal/skills/
  ├── .gemini/skills/   → symlink → ~/.agend-terminal/skills/
  ├── .opencode/skills/ → symlink → ~/.agend-terminal/skills/
  └── .kiro/skills/     → symlink → ~/.agend-terminal/skills/
```

在 Unix 上，各 backend 的項目都是 symlink（零維護）。在 Windows 上 agend-terminal 退而複製檔案；重新執行 `install` 會替換掉受管理的目標。

狀態檔案：

- `~/.agend-terminal/skills/<name>/` — 標準的 skill 內容
- `~/.agend-terminal/skills-lock.json` — 每個 skill 的來源 + 釘選版本（git 用 commit SHA，本地路徑用 mtime）+ 安裝時間戳
- `~/.agend-terminal/.skills-stage/<digest>/` — 短暫存在的暫存副本，當某個 agent 只需要部分 skill 時使用（見下方 fleet.yaml 整合）。超過 7 天後會被 GC 回收。

## CLI

所有命令都以 `agend-terminal skills <subcommand>` 形式執行。

### Add

```
agend-terminal skills add <source>
```

`<source>` 可以是本地路徑，也可以是 git URL（`https://…`、`git@…`、`ssh://…`，或任何以 `.git` 結尾的字串）。skill 目錄名稱取自來源的 basename，所以 `git clone … repo-foo` 會變成 `~/.agend-terminal/skills/repo-foo/`。

- 本地路徑：遞迴複製到標準來源根目錄。
- Git URL：以 `git clone --depth=1` 複製到標準來源根目錄；釘選版本就是產生出來的 HEAD SHA。

重新加入一個已存在的名稱會就地覆蓋並更新 lock 項目；如果你想從原始來源重新整理，請改用 `update`。

### Remove

```
agend-terminal skills remove <name>
```

刪除 `~/.agend-terminal/skills/<name>/` 並清除它的 lock 項目。具冪等性——對一個不存在的名稱執行時不會有任何作用。

### List

```
agend-terminal skills list
```

列出 `~/.agend-terminal/skills/` 之下的每個目錄，連同記錄的來源和釘選版本（若缺漏則顯示 `(unrecorded)` / `(unpinned)`）。

### Update

```
agend-terminal skills update          # update every skill with a recorded source
agend-terminal skills update <name>   # update just one
```

對 `skills-lock.json` 中儲存的來源重新執行 `add`。在 lock 存在之前就加入的 skill（或手動匯入的）需要重新加入；遇到這種情況時 `update` 會顯示明確的錯誤訊息。

### Install（手動）

```
agend-terminal skills install <working_dir>
```

在 `<working_dir>` 之下建立五個各 backend 的子目錄，並讓每個都指向 `~/.agend-terminal/skills/`（symlink，Windows 上為複製）。當你想讓 skill 在某個並非由 daemon 產生的目錄裡可見時使用——對於受管理的 agent，daemon 會自動執行相同的 install（見下文）。

## Daemon 整合

當 daemon 啟動一個 agent 時，它會對該 agent 的工作目錄呼叫 `skills::install_for_agent`。結果與手動執行 `skills install <working_dir>` 相同，但多了一項能力：per-instance 過濾。

### 透過 fleet.yaml 設定 per-instance 白名單

```yaml
instances:
  reviewer:
    backend: claude
    skills:
      - skill-forge
      - code-review-expert
```

行為：

- 省略 `skills:`（預設）—— `~/.agend-terminal/skills/` 之下的每個 skill 都會暴露給該 agent。
- `skills: [name1, name2]` —— 只有指定名稱的 skill 會被暫存到 `~/.agend-terminal/.skills-stage/<digest>/` 之下的一個臨時 digest 目錄，再把那個暫存目錄 symlink 進各 backend 路徑。該 agent 只會看到那些 skill。
- `skills: []` —— 明確選擇不使用：各 backend 目錄會被建立，但不含任何 skill（只有 daemon 的 `.agend-skills-managed` 標記）。
- 在標準來源中不存在的名稱會被跳過並發出警告；該 agent 仍會啟動。

這些暫存副本擁有依白名單決定的穩定名稱（排序後白名單的 SHA-256 前綴）。要求不同子集的多個 agent 可以並存而不衝突，daemon 也會在啟動時 GC 回收超過七天的暫存區。

## Smoke test

```
cargo test skills::
```

執行 `src/skills.rs::tests` 中的 24 個單元／整合測試——涵蓋 add/remove/list/install/update、skills-lock 的往返、SHA-256 暫存 digest，以及暫存區 GC（包含 TOCTOU 同次執行排除）。在乾淨的 checkout 上全部通過。

其他好用的一次性檢查：

```
agend-terminal skills list                            # canonical source inventory
agend-terminal skills install /tmp/scratch-agent      # exercises the symlink/copy path
cat ~/.agend-terminal/skills-lock.json                # inspect pinned versions
```

## 範例

從 GitHub 安裝一個社群 skill：

```
agend-terminal skills add https://github.com/mattpocock/skills.git
agend-terminal skills list
```

釘選一個你正在反覆修改的本地 skill：

```
agend-terminal skills add ~/projects/my-skill
# … edit files …
agend-terminal skills update my-skill   # re-runs add → updates the mtime version
```

把某個 agent 限制在一個子集內：

```yaml
# fleet.yaml
instances:
  doc-writer:
    backend: claude
    skills: [writing-style-guide, markdown-linter]
```

執行 `agend-terminal start` 之後，`~/.agend-terminal/workspace/doc-writer/.claude/skills/` 會解析到一個只含那兩個 skill 的暫存區。

## 疑難排解

| 症狀 | 可能原因 |
|---------|--------------|
| `list` 顯示 `no skills installed under …` | 還沒加入任何 skill；先執行 `add`。 |
| `git clone failed for <url>` | git 無法使用，或該 URL 需要認證；請手動 clone 後再 `add` 該本地路徑。 |
| Backend 忽略一個新加入的 skill | agent 程序仍以舊的啟動狀態執行中；重啟該 instance，讓 daemon 重新執行 `install_for_agent`。 |
| Backend 目錄存在但是空的 | 該路徑上有一個較舊、非 daemon 管理的目錄。agend-terminal 拒絕碰沒有 `.agend-skills-managed` 標記的目錄——把它移走或刪除，然後重新安裝。 |
| 某個 instance 意外缺少某個 skill | 檢查 `fleet.yaml`——那個 instance 可能有一個明確的 `skills:` 白名單把它排除在外。 |
| `.skills-stage/` 之下的磁碟空間持續增長 | 暫存區會在 daemon 啟動時 GC 回收超過七天的內容；重啟 daemon 即可強制清掃一次。 |

## 來源沿革

skills 功能在 Sprint 60–62 之間陸續推出：

- #585 — 在 agent 啟動時自動安裝（Sprint 61 W1 PR-1）
- #586 — `fleet.yaml` per-instance 白名單（Sprint 61 W1 PR-2）
- #590 — SHA-256 前綴暫存 digest（Sprint 62 W1 PR-1）
- #591 — 暫存區 GC，含 TOCTOU 同次執行排除（Sprint 62 W1 PR-2）

實作位於 `src/skills.rs`（單一模組，約 650 LOC + 24 個測試）。CLI 介面在 `src/cli.rs` 的 `Sprint 60 W2 PR-1 — agend skills CLI subcommands` 標題之下。