[English](PULL_REQUEST_TEMPLATE.md)

<!-- 感謝你的貢獻！請完成下列檢查。若 PR 與已合併的變更重複，或在沒有推進
既有決策的情況下重新實作已決定的方案，可能會直接關閉而不再 review。 -->

## 變更內容與原因

<!-- 用一到兩句話說明本 PR 改了什麼，以及它解決的問題。
請連結會被本 PR 關閉的 issue，例如 `Closes #123`。 -->

## 既有討論檢查（必填）

- [ ] 我已查看 [docs/KNOWN_ISSUES.md](https://github.com/suzuke/agend-terminal/blob/main/docs/KNOWN_ISSUES.md)——這不是其中已列為刻意延後處理的項目。
- [ ] 我已將本變更與**目前的 `main`** 分支比較，確認尚未實作。
- [ ] 我已搜尋**已結案**的 issue 與 PR，確認此範圍是否有先前嘗試或決策，並在下方連結找到的內容。
- [ ] 若本變更重新檢視既有決策，我已在下方說明哪些新證據或變更值得推進，而不是重提相同方案。

## 驗證方式

<!-- 新增／更新的測試、手動步驟或實際執行的命令
（例如 `cargo test`、`cargo fmt --check`、`cargo clippy`）。請說明你實際執行了
什麼及其結果——只有「測試通過」而沒有命令，是主張而不是證據。 -->

## 相容性（磁碟格式）

<!-- 若本 PR 變更 daemon 讀寫的任何磁碟格式（$AGEND_HOME 下的檔案，
或寫入 agent 工作目錄的檔案），請依
[docs/COMPATIBILITY.md](https://github.com/suzuke/agend-terminal/blob/main/docs/COMPATIBILITY.md)
檢查。Tier (a) 手動編輯介面（例如 `fleet.yaml`）與 tier (b) 持久化狀態
（inbox、task event、sidecar store）在 migration framework 存在之前只能做
ADDITIVE-ONLY 變更；tier (c)（cache、lock、transcript）則可自由變更。 -->

- [ ] 未變更 tier (a)/(b) 的磁碟格式，**或**變更僅為 additive-only——新增具有 serde default 的 optional field；未重新命名、改型別、改用途或移除既有欄位。
- [ ] 若確實是 breaking format change：已提高相關 `schema_version`、隨附 migration（或明確拒絕並提供操作說明），且已在上方特別說明。

## 給 reviewer 的備註

<!-- Reviewer 應知道的事項：取捨、後續工作、不在範圍內的內容。 -->
