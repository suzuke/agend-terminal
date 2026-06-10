# 迴圈工程 × agend-terminal：六零件對照

> 一張把「迴圈工程（loop engineering）」概念拆成六個零件、逐項對照 agend-terminal 實作的表。
> 用途：對外解釋 agend 的設計理念、佐證「為什麼要這麼複雜」、或作為 README/簡報素材。

## 背景

「迴圈工程」的核心主張：別再一個個指令地催 coding agent，改成設計一套會自動催它的迴圈——把親自下 prompt 的那個「你」換成系統，你只定義目標，由 AI 反覆迭代直到完成。

該概念把一個「跑得住的迴圈」拆成五個零件加一份外部記憶：

1. **automation** — 任務按時觸發、自行發掘與分類
2. **worktree** — 多個代理人平行作業而不互相干擾
3. **skills** — 把專案知識寫在外部，避免代理人每次重新猜測
4. **plugins / connectors（MCP）** — 串接既有工具與服務
5. **sub-agents** — 提案者與審查者分離，避免模型替自己的作業打分數
6. **外部記憶** — 模型每次執行間會遺忘，記憶必須存於磁碟而非上下文

下表逐項對照 agend-terminal 的實作狀態。

## 對照表

| # | 零件 | 概念描述 | agend 實作 | 源碼佐證 | 評註 |
|---|------|----------|-----------|----------|------|
| 1 | **automation** | 按時觸發、自行發掘分類 | ✅ 做滿且更深 | `src/schedules.rs`、`src/daemon/cron_tick.rs`、`src/daemon/{idle,handoff_timeout,inbox_stuck,helper_staleness}_watchdog.rs` | 文章只講「按時觸發」；agend 多一層**卡住偵測**——不只啟動迴圈，還抓迴圈空轉/卡死。比概念更進階。 |
| 2 | **worktree** | 多代理平行不互踩 | ✅ 做滿 | `src/worktree.rs`、`src/worktree_pool.rs`、`src/worktree_cleanup.rs` | 連**池化複用**與**自動回收**都有，工程級而非 demo 級。 |
| 3 | **skills** | 外部專案知識 | ✅ 有，偏新 | `src/skills.rs`、`~/.agend-terminal/skills/`（5 backend 統一 symlink） | 跨 backend 統一做得好。空間在「知識的結構化/主動檢索」——目前偏存放，未來可更主動餵給 agent。 |
| 4 | **plugins / connectors（MCP）** | 串接既有工具服務 | ✅ 做滿，是核心 | `src/mcp/registry.rs`、`src/mcp/tools.rs`、`src/mcp/handlers/` | MCP 是 agend 的神經系統，agent 間的 send/task/decision 全走這層。比「串接工具」更進一步用於 agent 協調。 |
| 5 | **sub-agents（審查分離）** | 提案者/審查者分離，避免自評 | ✅✅ 領先最多 | `src/claim_verifier.rs`、`src/verify.rs`、verdict 協定（VERIFIED/REJECTED/UNVERIFIED）、issue #1666 | 文章列為「一個零件」，agend 做成**一整套協定**：獨立 reviewer、強制證據、dual-review、cross-team borrow。護城河最深處。 |
| 6 | **外部記憶** | 存磁碟不存 context | ✅ 做滿 | `src/decisions.rs`、`src/inbox/`、memory 系統（markdown） | 文章講「markdown 存磁碟」；agend 有三層——decision log（為什麼這樣決定）、inbox（發生過什麼）、memory（跨 session 事實）。 |

## 總評

六項全部命中。其中四項（automation / worktree / MCP / 記憶）不只做滿、還超出文章描述；reviewer 分離那項領先最多；唯一還有空間的是 skills 的「主動餵知識」。

文章寫的是**規格**，agend 寫的是**產品**——它描述的「理想迴圈」，agend 已是它的完整實作版甚至超集。

## 最該注意的落差：不在零件，在使用紀律

六個零件 agend 都有，但「迴圈工程」原文結尾那段警告**不是零件能解的**：

> 無人看管的迴圈，也會無人看管地犯錯。理解力會在不知不覺間退化，迴圈交付得越快，人與程式碼之間的落差越大。最危險的是放棄判斷、全盤接受迴圈給出的結果。

agend 機制上能防 *agent* 自我欺騙（靠 reviewer 分離、強制證據），但防不了 *人* 自己全盤接受 agent 結果、不再讀 code。這是唯一一個 agend 幫不了、只能使用者自己守的東西。

工具做到頂，判斷力還是得自己留著。

> 原文最後一句：「迴圈本身沒有差別，但迴圈製作者的經驗決定了迴圈的品質與效果。」
> ——這正是護城河所在：人人能寫個 `while` loop，但把六個零件正確組起來、讓它不自我欺騙，靠的是經驗。
