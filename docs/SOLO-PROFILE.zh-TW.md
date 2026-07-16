[English](SOLO-PROFILE.md)

# Solo Profile — 單人（operator + 一個 agent）情境指引

**性質：** 說明性，非規範性。本文件與 `FLEET-DEV-PROTOCOL.md` 衝突時，一律以
protocol 為準。本文件只協助在沒有 peer 可協調時套用 §3.21 的 proportional
ceremony 判斷；不會豁免 task tracking、CI、worktree、review 或 merge-authority gate。

## 為什麼需要這份文件

quickstart 的預設路徑——一個 `general` instance 對 operator 講話、無 team、無其
他 fleet peer——是 AgEnD 的官方入門路徑。但 `FLEET-DEV-PROTOCOL.md` 是為**多
agent**情境寫的：dispatch contract、雙 reviewer、其他 agent 會讀的 decision
board、對「久未回應的 peer」的 timeout staircase。單人若逐字照辦，會把整套
「避免另一個 agent 被搞混」的 ceremony 全跑一遍——但根本沒有另一個 agent。

#2524 的 workflow gap 盤點（三視角：多 agent／單人／非 Claude backend）直接點名
這個缺口：*「單人：quickstart 單 instance 是官方入門路徑，但 protocol ceremony
全是 fleet 導向，輕量化只靠 §3.21 lead judgment」*——單人的輕量化過去只能個案判
斷，沒有寫下來的指引。本文件就是那份指引，不是新開一個閘門。

## 什麼情況算「單人」

沒有 team（身分區塊裡沒有 `team`）且沒有列出其他 fleet peer。只要兩者之一存
在，就是 fleet 情境——照 `FLEET-DEV-PROTOCOL.md` 字面執行。

## 單人時仍然適用的規則

以下這些保護的是**你自己**、operator 的 repo，或 merge 管線本身——不是某個
peer agent。人數多寡不改變它們存在的理由：

- **Task board（§1）。** 單人 agent 處理 operator 的直接請求時，要自行建立並
  claim task，最後記錄有證據的結果。board 仍是跨 restart／handoff 的 durable
  source of truth。
- **Worktree 紀律（§10／§12.4）。** 仍然要用 daemon-managed worktree + branch，
  絕不直接 commit main，也不以 raw git 建立 worktree。這是為了把你的改動跟
  operator 的 canonical working tree 隔開，這件事跟有沒有隊友無關。
- **Test-first（§3.10）。** 仍然要先寫會失敗的測試再修。這是為了抓**你自己**
  的迴歸——價值不是「讓 reviewer 能驗證」，是「讓你自己不會交出一個其實沒修好
  的修法」。
- **CI fail-closed merge。** CI 不知道也不在乎 repo 上有幾個 agent。綠燈就是
  綠燈。
- **證據閘（§3.3「comments are claims, not evidence」）。** 就算只有你自己會
  回頭看自己的宣稱，這條依然成立。

## 單人時可以縮減的部分

- **Review coordination（§3.2–3.5）。** 不要捏造假的 reviewer；但仍要依 §3.21
  選擇 review tier，merge authority 也仍照 §3.5。implementer self-merge 需要兩個
  獨立 VERIFIED；operator 只有在 §3.6 等適用 protocol 路徑下才能直接 review／
  merge。若所需 reviewer 不存在，把 merge 決定交還 operator，或加入真正的
  reviewer instance。
- **Decision board 討論。** 可以沒有 peer discussion thread，但 scope decision、
  correction 或尚未解決的 operator fork 仍要記在 `decision`。只有在 default 安全
  且有明確聲明時，才使用下面的 timeout/default 路徑。
- **`send`／`inbox`／team 通訊工具。** 沒有 peer recipient 時，不必做 peer
  dispatch。跟 operator 的溝通還是走對應 channel 的 `reply` 路徑——這跟 fleet
  大小無關。
- **對「久未回應 peer」的 timeout staircase（§9）。** 沒有 peer 可以升級處理。

## 已經解掉的那個硬卡點

在 #2531 之前，`decision(needs_answer: true)` 若 operator 離線，完全沒有解法
——只能無限期等。這是真實的單人／overnight 失效模式：沒有 peer 能代答，也沒
有預設值可退。

**已修復**：`decision(action: post, needs_answer: true, timeout_secs: N,
timeout_default: "...")`——超過 `timeout_secs` 未答，daemon 自動採
`timeout_default` 並通知該 decision 的 `author`。單人與 overnight 的 decision
現在有真正的出口，不再是無限期等待。這是 #2524 在多 agent／單人／非 Claude
backend 三視角盤點裡**唯一**的硬卡點——本文件其他部分都是「輕量化」，不是補
缺失機制。

## 什麼時候該從單人升級成 fleet

直接套用 §3.21 軸 A——它本來就不是 fleet 專屬：

> FLEET iff *「錯了代價很高」* 且 *「只有想惡意破壞它的人才抓得到這個缺陷——你自
> 己會寫的測試抓不到」*。否則 SINGLE。

lead 的 5 秒問題，改寫成單人 agent 自問自答的版本：*「如果我這裡悄悄錯了，有
多嚴重，我自己寫的測試抓得到，還是只有主動想搞破壞的人才抓得到？」*如果誠實
的答案是「只有惡意的人才抓得到」，別因為慣性留在單人模式——在出手前拉進第二
個視角（一個 reviewer instance，或 operator 本人）。

## 另見

- `FLEET-DEV-PROTOCOL.md` §3.6（LOW docs-only 例外）是 protocol 本身已經開出的
  一條輕量路徑範例——本文件是把同一種直覺推廣開來。
- `FLEET-DEV-PROTOCOL.md` §3.21（Proportional Ceremony）——本文件的升級判準取
  自這裡的三個獨立軸（fleet-vs-single、spike-vs-skip、review tier）。
