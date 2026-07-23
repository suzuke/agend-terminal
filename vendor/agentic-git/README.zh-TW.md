# agentic-git

> 要把 agentic-git 嵌入你自己的 orchestrator?從 [Embedder Contract v1](docs/embedder-contract-v1.md) 開始。

`agentic-git` 現在以獨立 Cargo workspace 的形式維護於
[AgEnD-Terminal monorepo](https://github.com/suzuke/agend-terminal/tree/main/vendor/agentic-git)；
完成一次 monorepo 發布驗證後，原本的獨立 repository 才會封存為唯讀歷史。

**給 AI coding agent 的透明防護版 `git`。**

`agentic-git` 是一個偽裝成 `git`、放在 agent PATH 上的 Rust binary。agent
繼續講它熟悉的 git;shim 平時隱形,出事的瞬間才出手:

- 把每個變更操作**路由**進該 agent 綁定的 worktree(HMAC 簽章的 binding);
- **擋下**會毀掉多 agent 環境的操作——`git worktree *`、切到 main/別人的
  branch、未綁定時的任何變更、在你本人的 canonical checkout 裡動手、
  push 範圍夾帶信任根檔案——並附上 LLM 讀得懂的「該怎麼做」;
- 透過 hook 為每個 commit 附上 `Agentic-Agent` 等 **provenance trailer**;
- operator 可**刻意 bypass**(一次性 / 逐 agent / 限時),且留稽核紀錄。

從 [agend-terminal](https://github.com/suzuke/agend-terminal) 抽取而來
(原名 `agend-git`),在真實多 agent fleet 上運行;shim 檔案的完整 commit
歷史一併保留。所有環境變數都接受舊 `AGEND_*` 名稱作為 fallback,既有
agend fleet **零改動**即可採用本 binary。

> **誠實定位**:這是安全帶,不是牢籠。它針對「半信任、會出包」的 agent
> (手滑、被 prompt injection),擋不住直接呼叫 `/usr/bin/git` 的蓄意
> 攻擊者——要硬邊界請在底下加 kernel 級隔離。

詳細機制、環境變數契約、roadmap 見英文版 [README.md](README.md)。

License: Apache-2.0(見 [NOTICE](NOTICE))。
