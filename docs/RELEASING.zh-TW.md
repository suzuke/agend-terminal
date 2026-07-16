[English](RELEASING.md)

# Releasing agend-terminal — 發布流程

適用對象：任何負責切出版本的 maintainer。推送 tag 之後，整條 pipeline 完全
自動化——你的工作就是依序完成下面四個步驟。這裡沒有任何需要口耳相傳的隱性
知識；如果某個步驟說得不清楚，就在讓你困惑的那個 PR 裡順手修正這份文件。

## TL;DR

```sh
# on an up-to-date main checkout
# 1. bump version
$EDITOR Cargo.toml          # version = "X.Y.Z"
cargo update -w             # refresh Cargo.lock for the new version

# 2. changelog
$EDITOR CHANGELOG.md        # move [Unreleased] items into a new "## [X.Y.Z] — YYYY-MM-DD"
                            # section AND add the compare link at the bottom:
                            #   [X.Y.Z]: .../compare/vPREV...vX.Y.Z
                            #   point [Unreleased] at vX.Y.Z...HEAD

# 3. land the bump via the normal PR flow, then tag the merge commit
git tag -a vX.Y.Z -m "vX.Y.Z"   # ANNOTATED tag — not lightweight
git push origin vX.Y.Z

# 4. watch the Release workflow — everything below is automatic
```

## 推送 tag 會觸發什麼（`.github/workflows/release.yml`）

```
gate ──► build (5 targets) ──┐
     └─► appimage ───────────┴─► release (GH Release + SHA256SUMS) ──► publish (crates.io)
```

1. **gate** — 在任何 artifact 開始建置之前，遇到以下情況就讓這次發布失敗：
   - `Cargo.toml version` ≠ tag（去掉 `v`），
   - `CHANGELOG.md` 沒有 `## [X.Y.Z]` 這個 section，
   - crate 在宣告的 MSRV（`rust-version = "1.88"`）上已無法編譯。
   - `cargo-semver-checks` 也會在這裡執行，但在 pre-1.0 階段是 **soft-fail**：
     它會印出 breaking-change 報告供人判斷，但不會擋住流程。
     等 1.0 上線後再把它升級成 hard-fail。
2. **build / appimage** — artifact matrix 維持不變（5 個 target 加上 AppImage）。
3. **release** — 帶有 `generate_release_notes` 和 `SHA256SUMS` 的 GitHub Release。
4. **publish** — 先 `cargo publish --dry-run`，再 `cargo publish` 到 crates.io。
   - 使用 `CRATES_IO_TOKEN` 這個 repository secret。如果沒有設定這個 secret，
     這個 job 會**優雅地跳過**（綠燈，並在 job summary 中附上警告）——它絕不會
     讓一次發布變紅。要啟用發布功能：crates.io → Account Settings → API Tokens
     （scope：`publish-update`，限制在這個 crate），然後 `Settings → Secrets and
     variables → Actions → New repository secret → CRATES_IO_TOKEN`。
   - **絕不會在 pre-release tag 上執行**（任何含有 `-` 的 tag）。
   - 如果該版本已經存在於 crates.io，`cargo publish` 會失敗。這是預期中的保護
     機制，不是 bug：這個 job 只會在剛推送的 tag 上執行，而 gate 已經證明 tag
     與 `Cargo.toml` 相符——會撞到這個錯誤代表這個版本是在流程之外被發布出去
     的（請改用下一個 patch 版本重新跑一次發布）。

## Pre-releases

打成 `vX.Y.Z-rc.N`（annotated，跟正式版本一樣）。pipeline 會跑 gate → build →
GitHub Release，但**跳過 crates.io publish**（publish job 的 `if` 排除了含有 `-`
的 tag）。gate 的 changelog 檢查找的是基底的 `## [X.Y.Z]` section，所以在第一個
rc 之前就要先把 release notes 草擬好。如果你想在 releases 頁面上標記它，請手動把
這個 GitHub Release 標成 pre-release。

## Tag hygiene

- 一律使用 **annotated** tag（`git tag -a`）。歷史註記：v0.5.0–v0.7.0 是用
  lightweight 方式建立的；從 v0.7.1 開始，annotated 成為規則（annotated tag
  帶有 tagger/date metadata，也是 `git describe` 偏好的對象）。
- 對 `main` 上**包含版本 bump 的那個 merge commit** 打 tag——絕不要打在 branch
  head 上。無論哪種方式，gate 都會強制 version/changelog 的一致性。

## 不用 tag 也能驗證 workflow 變更

`workflow_dispatch` 會對 `main` 跑同一條 pipeline：gate 中與 tag 耦合的檢查
（version==tag、changelog）會自我跳過，MSRV 加 semver-checks 仍然會跑，artifact
會被建置並上傳，而 `release`/`publish` 這兩個 job 維持關閉（受 tag-ref 守護）。
在合併 release.yml 的修改之前先用這個方式驗證。

## 撤回一個有問題的版本

yank 會把某個版本從新的 `cargo install`/dependency resolution 中隱藏起來，但
不會刪除它（現有的 lockfile 仍可繼續運作）：

```sh
cargo yank --version X.Y.Z            # needs a token with yank scope
cargo yank --version X.Y.Z --undo     # if yanked by mistake
```

同時也要編輯 GitHub Release：把它標成 pre-release，或在 notes 中加上警告，指向
已修正的版本。然後透過正常流程把修正當成一個新的 patch release 發出去——絕不要
重用或移動已發布的 tag。

## Toolchain 政策（MSRV 地板 vs CI Check）

兩條 pin，刻意分開（#1994 / #2339 / #2340）：

| 角色 | Toolchain | 位置 | 目的 |
|------|-----------|------|------|
| **MSRV 地板** | **1.88**（宣告） | `Cargo.toml` `rust-version`、`ci.yml` 的 `MSRV check (1.88)`、`release.yml` gate | rustc ≥ 1.88 就能 `cargo install`／編譯 locked tree。擋住 Dependabot 靜默抬高地板（sysinfo 0.39 → rustc 1.95 那類）。 |
| **CI Check** | **當日 stable**（浮動） | `ci.yml` `check` matrix（`dtolnay/rust-toolchain@stable` + fmt/clippy/test） | 抓新 clippy 與編譯器行為。**不**釘在 1.88。 |

不要只因為 stable 前進（例如 1.96／1.97）就 bump `rust-version`。
新 stable 上的 clippy 拒絕項用一個機械小 PR 修到 `main`，MSRV 維持 1.88。
只有依賴**必須**升級且沒有 1.88 相容 pin 時才抬 MSRV——見下節。

## MSRV bumps

`Cargo.toml` 裡的 `rust-version` 是唯一的真實來源；gate 的 `cargo +1.88 check`
pin 必須在同一個做 bump 的 PR 裡一起更新（在 `ci.yml` 與 `release.yml` 裡
grep `1.88`）。把一次 MSRV bump 當成 minor-version 事件，並在 changelog 點出。
新地板仍應偏保守，不要對齊「最新 stable」。

## Release smoke test（目標：30 分鐘）

Tag 前，請針對精確的 release commit 或其 CI artifact 執行本節。

### Preflight

- [ ] 停止前一個 session 遺留的 daemon，並從 repository root 操作。
- [ ] 執行 `cargo build --release`；確認 `agend-terminal doctor` exit 0。
- [ ] 確認每個待測 backend 的 credential；測 Telegram 時設定
  `AGEND_TELEGRAM_BOT_TOKEN`。

### Backend matrix

對每個已安裝 backend：spawn、送出 `echo hello`、執行一次 tool call（例如
`list files in /tmp`）、正常退出，並確認沒有 orphan process。跳過的 backend
必須記在 sign-off。

| Backend | Ready evidence | 正常退出 | 額外檢查 |
|---|---|---|---|
| Claude Code (`claude`) | 30 秒內出現 `❯` 或 `bypass permissions` | `/exit` | `admin cleanup-branches` preview exit 0 且不刪除任何東西 |
| Kiro CLI (`kiro-cli`) | 30 秒內出現 `Trust All Tools active` 或 `ask a question` | `/quit` | trust dialog 已 dismiss |
| Codex (`codex`) | 20 秒內出現 `OpenAI Codex` 或 `›` | `exit` | trust-directory dialog 已 dismiss |
| OpenCode (`opencode`) | 45 秒內出現 `Ask anything` 或 `tab agents` | `/exit` | alt-screen 內的 mouse wheel 留給 backend（#744） |
| Agy (`agy`) | 20 秒內出現 `Antigravity CLI` 或 `Type your message` | `/exit` | `.agents/mcp_config.json` 載入 fleet MCP tools（#1547） |
| Grok (`grok`) | 30 秒內出現 `Grok Build` 或 `❯` | `/exit` | project-trust dialog 已 dismiss |

### Cross-cutting checks

- [ ] `Ctrl+B n` / `Ctrl+B p` 切換 tab、`Ctrl+B o` 切換 pane，且
  `Ctrl+B d` 可乾淨 detach。
- [ ] 在一般、非 alt-screen pane 中，mouse wheel 可捲動 history。
- [ ] Channel 啟用時，Telegram 訊息抵達正確 agent pane。
- [ ] 一次 disposable `repo(action=checkout, bind=true)` 加上
  `release_worktree` 後，`binding_state` 為 unbound 且無 dangling worktree。
- [ ] 設定 `AGEND_CAPTURE_FIXTURES=1` 時，一次 backend run 會寫出 `.cap` 與
  `.cap.meta.json`；完成後 unset。

### Sign-off

把下列內容貼進 release PR：

```text
Date: YYYY-MM-DD
Operator: <name>
agend-terminal version: <version>
OS / arch: <value>

Backend versions tested:
- claude:
- kiro-cli:
- codex:
- opencode:
- agy:
- grok:

Backends skipped (reason):
-

Known deviations / failures:
-

Overall verdict: [ ] PASS  [ ] PASS with caveats  [ ] FAIL
```

六個 backend 全部通過時，在 PR 加上 `Real-backend smoke: ✓ all 6 backends`。
