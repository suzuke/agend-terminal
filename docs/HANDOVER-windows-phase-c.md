# Handover: Windows Port — Phase C

> **Status: SHIPPED** — Windows Phase C landed on `main`. Doc retained for historical/provenance.

> Date: 2026-04-17
> Prereq: Read `docs/PLAN-windows-support.md` and `docs/EVAL-cross-platform.md`.

## 目前狀態

**已完成（已在 `main`）：**
- **Phase A** — paths、file locking、PID helpers、chmod guards、deps 條件化
- **Phase B** — IPC UDS → TCP loopback（commit `19db351` merge, `307770a` feat）
  - 新 `src/ipc.rs`：`bind_loopback` / `write_port` / `read_port` / `connect_{api,agent}` / `probe_agent`
  - 每個 agent 一個 `{run_dir}/{name}.port`，daemon 用 `api.port`，atomic tmp+rename
  - `daemon.rs` / `api.rs` / `tui.rs` / `cli.rs` / `mcp/*` / `agent.rs` / `verify.rs` / `ops.rs` 全部走 `ipc.rs`
  - Integration tests + repro scripts 已改為讀 `api.port` 並用 TCP 連線
  - 同時修掉 `ops.rs` / `mcp/handlers.rs` 原本 `list_agents` 掃錯目錄（掃 `home` 找 `.sock`）的 bug
  - 本地用 `cargo-xwin` + `brew llvm` 驗證過 `cargo check --target x86_64-pc-windows-msvc` 綠

**本地驗證跑過的：**
- `cargo build` — pass
- `cargo test --bin agend-terminal` — 288/288
- `cargo test --test integration` — 6/6
- `cargo clippy -- -D warnings` — pass
- `cargo fmt --check` — clean
- `cargo xwin check --target x86_64-pc-windows-msvc` — pass

## 還缺什麼（Phase C）

| 項目 | 內容 | 備註 |
|---|---|---|
| **C.1** Windows CI | `.github/workflows/ci.yml` 加 `windows-latest` matrix；`cargo build --release` + `cargo test` | **先做這個**。原生 Windows runner 是唯一能驗證 ConPTY 行為 + link 階段的方式。dev 機 `cargo check` 只證明 type 正確，不證明 runtime。 |
| **C.2** `.cmd`/`.ps1` wrappers | `src/instructions.rs` + `src/mcp_config.rs` 目前只產 `.sh`（MCP wrapper、statusline 腳本）；Windows 要 `.cmd` 等價品 | Plan 裡有 sketch（`script_ext()` / `script_header()` helper）。腳本本體不長，是 `set VAR=...` + `call ...` 這種。 |
| **C.3** ConPTY 行為驗證 | `portable-pty` 在 Windows 走 ConPTY；ANSI filter、無 SIGWINCH、line buffering 可能影響 `src/state.rs` 的 agent-state regex | 要在 Windows 實跑 spawn cmd.exe/powershell.exe 並對比 VTerm 輸出；可能要微調 state regex |
| **C.4** Backend smoke | claude/codex/kiro-cli/opencode/gemini 在 Windows PATH 解析；`doctor` 對缺失 backend 要給乾淨錯誤而不是 crash | 各家 CLI 對 Windows 支援度不一，見 plan C.4 表格 |
| **C.5** End-to-end smoke | `start` / `list` / `attach` / `inject` / `app` / `stop` 流程 | 發版收尾 |

**最小可發版路徑：C.1 → C.2 → C.5**。C.3 / C.4 可以在 CI 綠後針對具體失敗修。

## 已知本地限制（不是發版缺失）

- 本機 macOS 上只做了 `cargo xwin check`，沒做 `cargo xwin build`。`build` 會進 link 階段，`ring` 會呼叫 `lld-link`。Homebrew `llvm` formula 沒附 `lld`，要另外 `brew install lld` 才能本地 build。Windows runner 原生有 `lib.exe` / `link.exe`，不受此限制。

## 重要檔案

```
src/ipc.rs                  # Phase B 的核心：TCP + port registry
src/daemon.rs               # daemon 主流程 + run_dir 管理 + serve_agent_tui
src/api.rs                  # NDJSON over TCP API server + client call()
src/tui.rs                  # attach 連 agent TCP
src/process.rs              # cross-platform is_pid_alive / terminate
src/instructions.rs         # MCP wrapper / statusline scripts — Phase C.2 要改
src/mcp_config.rs           # MCP config 生成 — Phase C.2 要改
src/state.rs                # Agent state detection regex — Phase C.3 可能要調
.github/workflows/ci.yml    # Phase C.1 要加 windows-latest
```

## Memory / 約束提醒

（來自 `~/.claude/projects/.../memory/`）

- **No install without consent** — 要裝 cargo-xwin、brew llvm、或 rustup target 一定要先問
- **Fix now, never defer** — 順手發現 bug 就修，別「之後」
- **English-only comments** — code 註解純英文
- **Automate verification** — 優先 script/test 而不是手動 repro
- **No manual binary copy** — 不要 cp binary 到 /opt/homebrew/bin
- **Git worktree workflow** — feature 工作一律開 worktree，不直接動 main

## 建議做法

1. 先開新 worktree：`git worktree add .claude/worktrees/windows-port-phase-c -b worktree-windows-port-phase-c`
2. 在 worktree 裡先做 C.1（CI matrix）— 推到遠端讓 GitHub Actions 跑看看會不會掛
3. 根據 C.1 失敗訊息決定 C.2 / C.3 的順序
