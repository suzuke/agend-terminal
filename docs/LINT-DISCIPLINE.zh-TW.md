[English](LINT-DISCIPLINE.md)

# Lint Discipline — 跨平台 pre-push 檢查清單

**目的**：在 push 之前抓出那些反覆出現、在 Sprint 56–57 消耗了大量 fix-forward
循環的跨平台 lint 問題，讓 CI matrix 的工作是「驗證」而不是「發現」。

**搭配工具**：`scripts/clippy-all-platforms.sh`。本文件記錄的是這支腳本想要
揭露的*模式*。只跑腳本而沒有內化這些模式，會讓你對腳本偵測不到的失敗
模式視而不見（因為那些問題發生在 link/runtime 階段，而不是 lint 階段）。

---

## 快速檢查清單（pre-push）

每次 push 牽涉到平台相關的程式碼之前都要執行：

```bash
scripts/clippy-all-platforms.sh           # full matrix
scripts/clippy-all-platforms.sh --quick   # host only (fast iteration)
```

如果腳本回報某個 target `failed`，**先在本機修好**，不要倚賴 CI 去發現。
一次 CI 循環大約耗費 10–15 分鐘的牆鐘時間（wall time）才能 fix-forward；
本機循環每次 `cargo clippy` 大約只要 30 秒。

---

## 需要留意的模式

### 1. cfg-gated `dead_code`

**症狀**：`error: function/struct/method is never used` 出現在跟撰寫程式碼
時不同的平台上。觸發原因是該函式只在 `#[cfg(target_os = "...")]` 或
`#[cfg(feature = "...")]` 區塊內被呼叫；在沒有啟用那個 cfg 的平台上，這個
symbol 變成無人引用，於是 `-D warnings` 就擋下來。

**修正方式**：把 `#[allow(dead_code)]` 的範圍縮到該 symbol 上，並加上註解
說明這個平台條件性。*不要*把 `#[allow(dead_code)]` 直接貼在父 module
上——那會把真正的 dead code 一起藏起來。

```rust
// Used only on Windows (see cfg block at line 142). Other platforms
// see this as dead, hence the explicit allow.
#[allow(dead_code)]
fn windows_only_helper(...) -> ... { ... }
```

**Sprint 57 事件**：Wave 3 PR-2 r1 + r2（commit 438878b）——service
template 的測試 helper 以 `cfg(unix)` 設限，在 Windows runner 上 clippy
失敗。在正確的 `#[allow(dead_code)]` 範圍落地之前經歷了兩次 fix-forward 循環。

---

### 2. fire-and-forget spawn 理由

**症狀**：clippy 不會直接強制這一點——但專案的 **Phase 5b invariant test**
（`tests/spawn_rationale_invariant.rs`）會。每一處 `tokio::spawn` 和
`thread::spawn` 都必須帶有以下其中一項：

- 在呼叫端加上 `// fire-and-forget: <reason>` 註解，或
- 明確儲存 `JoinHandle` 以便優雅 join。

**修正方式**：補上理由註解。理由應該說明*為什麼*沒有任何東西在等待這個
task 完成（例如「logging is best-effort」、「background cache warmer，daemon
shutdown 透過 global cancellation token 等待」）。

```rust
tokio::spawn(async move {
    // fire-and-forget: telemetry is best-effort; daemon shutdown
    // happens via the global cancellation token observed inside the
    // future, no join needed.
    emit_telemetry(...).await;
});
```

**參考**：`FLEET-DEV-PROTOCOL.md` §10.5。測試是豁免的（測試 helper
可以臨時 spawn）；trait method 沿用呼叫端的理由。

---

### 3. 格式感知的 shell-template 跳脫

**症狀**：某個 service template（例如 systemd unit 檔、launchd plist、
PowerShell 腳本）用某一種 shell 的規則跳脫字元，到另一種平台上就靜默
損壞。測試在寫出該 template 的平台上通過；卻在*消費*該 template 的那個 OS
上於 CI 失敗。

**修正方式**：每個 template renderer 都必須是*格式感知*的——跳脫規則要
依目標 shell 來挑，而不是 host shell。POSIX sh 跳脫（`'$value'`）≠
PowerShell 跳脫（`"$value"` 搭配 backtick 跳脫）≠ JSON 跳脫（反斜線跳脫）。

**Sprint 57 事件**：Wave 3 PR-3 r2（commit 71cb3b6）——跨平台 service
template 安裝器的跳脫表在所有平台上都假設 POSIX shell；PowerShell 的消費端
拿到壞掉的路徑。一次 fix-forward 循環。

---

### 4. Windows `.exe` 副檔名處理

**症狀**：helper binary 的路徑解析（`agend-git`、`agend-mcp-bridge`）或
test-harness 的 binary 查找在 Windows 上漏掉 `.exe` 後綴。binary 明明存在，
查找卻因為 `Path::exists()` 回傳 false 而失敗。

**修正方式**：用 `std::env::consts::EXE_SUFFIX` 來組出預期的檔名。永遠不要
硬寫 `.exe`（會在非 Windows 的測試路徑上爆掉），也永遠不要省略它（會在
Windows 上爆掉）。

```rust
let bin_name = format!("agend-git{}", std::env::consts::EXE_SUFFIX);
```

**Sprint 58 事件**：在 Wave 2 PR-1（#11 helper-staleness warn，commit
9a2fc32）中主動抓到——`classify_helper_staleness` 使用 `EXE_SUFFIX`，所以
doctor 診斷在 Windows 上是正確的。

---

### 5. mtime 跨平台分支

**症狀**：`std::fs::metadata().modified()` 在某些檔案系統上回傳 `Err`
（較舊的 NFS、較舊的 Windows ReFS），或回傳一個帶有平台特定解析度的
時間戳（FAT32 = 2 秒粒度）。

**修正方式**：遇到 `Err` 時優雅降級。給出第四分支的分類器（例如
`classify_helper_staleness` 裡的 `UndeterminableDaemonPath`），而不是 panic
或靜默回傳一個會誤導操作者的預設值。

```rust
match std::fs::metadata(&path).and_then(|m| m.modified()) {
    Ok(mtime) => HelperStaleness::classify_from_mtime(mtime, ...),
    Err(_) => HelperStaleness::UndeterminableDaemonPath,
}
```

**參考**：Wave 2 PR-1（#11）的 PR 描述——完整的 enum 推理。

---

### 6. 路徑分隔符 + canonical-path round-trip

**症狀**：測試斷言 `path.to_str() == "a/b/c"`，但 Windows 產出的是
`"a\\b\\c"`。或者：某個儲存的 canonical path 在 host 上 round-trip 沒問題，
到另一個 OS 上卻正規化成不同的形狀（大小寫敏感度、UNC 前綴 `\\?\`）。

**修正方式**：透過 `Path::components()` 來斷言，或把 `Path::canonicalize()`
的結果跟其他 `Path::canonicalize()` 的結果相比（絕對不要比對原始字串）。
避免在測試預期值裡嵌入字面的路徑分隔符。

---

### 7. 對時序敏感的跨平台測試

**症狀**：某個測試 sleep 100ms 後斷言心跳已觸發。在 Linux runner 上通過
（scheduler 快），在 macOS runner 上 flaky（10–50ms 誤差），或在 Windows 上
（kernel timer 解析度較粗）也 flaky。

**修正方式**：在測試裡用更寬鬆的 sleep 預算，或（較佳）讓測試由一個確定性
事件（channel、condvar、被觀測的計數器）來驅動，而不是牆鐘時間。

---

## 當腳本幫不上忙時

跨平台 clippy gate 抓的是 **lint 層級**的跨平台問題（cfg 分支、dead_code、
cfg-gated 區塊裡的型別錯誤）。它**不會**抓到：

- **Link 層級的失敗**：目標平台上缺少 C 函式庫（非 Linux 上的 gtk、純
  Windows 上的 openssl-sys）。腳本會把這些歸類為
  `skipped (build-script C-dep)` 並交給 CI matrix。
- **Runtime 行為差異**：shell-template 跳脫語意（模式 3）、路徑分隔符處理
  （模式 6）、時序 flake（模式 7）。這些需要針對性的測試，並對照本文件 review。
- **僅在 linker 階段出現的警告**：例如只有在完整 build/link 跑起來時才會觸發的
  unused import。那些仍然需要 CI matrix。

預期的工作流程是：

1. 編輯程式碼。
2. `cargo clippy --features tray --bin agend-terminal --tests -- -D warnings`
   （僅 host，快速）。
3. **`scripts/clippy-all-platforms.sh`** —— 抓出模式 1、4、5、6。
4. `cargo test --features tray`（僅 host，快速）。
5. `git push` → CI matrix 在全部 3 個平台上驗證 link + runtime。
6. CI 綠燈 → merge。

如果你跳過第 3 步，模式 1、4、5、6 可能讓你陷入 fix-forward 循環——每個
平台的每次循環都要耗費約 10–15 分鐘的牆鐘時間。本機腳本只要約 30 秒。

---

## 歷史（這道 gate 是何時加入的）

- **Sprint 56**：在 Track I-Phase2c 硬移除清理期間，觀察到 4 次平台特定的
  fix-forward 循環。
- **Sprint 57 Wave 3 PR-2 r1 + r2**：平台設限的測試 helper 上的 dead_code
  （模式 1）—— 2 次 fix-forward 循環。
- **Sprint 57 Wave 3 PR-3 r0**：格式感知的 service template 跳脫
  （模式 3）—— 1 次 fix-forward 循環。
- **Sprint 58 Wave 3 PR-1**（這道 gate）：加入本機 helper 腳本和本文件。
  依一般 FINAL LOCK 採取 Shape (c)——被動由操作者解決、不自動安裝、不注入
  git-hook。與 Wave 2 PR-1（#11 helper-staleness warn）的 Q3 設計模式對齊。