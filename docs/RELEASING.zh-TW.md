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

## MSRV bumps

`Cargo.toml` 裡的 `rust-version` 是唯一的真實來源；gate 的 `cargo +1.88 check`
pin 必須在同一個做 bump 的 PR 裡一起更新（在 release.yml 裡 grep `1.88`）。把一次
MSRV bump 當成一個 minor-version 事件來看待，並在 changelog 中特別點出來。