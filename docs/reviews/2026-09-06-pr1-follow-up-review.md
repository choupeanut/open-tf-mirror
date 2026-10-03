# PR #1 修正複查

日期：2026-09-06

審查 commit：`f8c5ed1e7d076353e8ff0a45836ea59c5d7456d5`；base：`8758ce58922cb5190706244c7c81779ae57b5115`。

本次重新檢查 TLS、metadata/provider storage、module mirror、HTTP handlers、Helm、CI、驗證腳本與既有測試。結論：**建議先修正 R1–R3 再合併**。目前 CI 全綠，但新增的兩個針對性 regression probes 都失敗，另有一個實際 HTTPS 停機測試確認未啟動 connection-level graceful shutdown。沒有發現可確認的 P0。

本報告補充並更正前次 audit 的「已完成」判斷；前次的 partial-platform、prerelease 與 graceful-drain 驗收不足。這次僅新增複查文件，沒有修改或推送產品程式碼。

## 已確認問題

### R1 — P1：部分平台更新失敗會刪掉仍可用的 stale metadata

位置：[src/metadata.rs](../../src/metadata.rs)，`refresh_version`，行 517–554。

- 條件：已有 linux/amd64 與 darwin/amd64 的完整 metadata，超過 freshness 後刷新；linux 成功、darwin 回傳 503。
- 實際：程式只將成功結果加入 `platforms`，以新的 `fetched_at` 覆寫記憶體與磁碟。`load_version` 將此次視為成功，因此不會使用完整 stale fallback 或設定 failure backoff。
- 影響：darwin 從 version JSON 消失；即使其 ZIP 已存在，archive handler 仍必須先取得平台 metadata，因而無法使用。缺失結果被視為新鮮，預設可持續 30 分鐘；重啟也會讀到縮減後的資料。
- 重現：WireMock index 包含兩平台，磁碟預放完整且過期的 version metadata；只有 darwin download metadata 回傳 503。期待 `platforms.len() == 2`，實際為 `1`。
- 修正：以 `(os, arch)` 合併成功刷新與通過驗證的既有平台資料；對暫時失敗的平台保留 stale。對不完整結果採較短重試時間或逐平台 freshness，避免套用完整 30 分鐘 TTL。不要把已被 index 正式移除的平台無条件保留。
- 驗收：完整快取 → 一平台 503 → 該平台 archive 仍可下載；上游恢復後可在短 backoff 後補齊；cold partial 與 warm partial 各有測試，並涵蓋重啟。

### R2 — P2：prerelease 檔名仍可能被解析成錯誤的平台

位置：[src/provider.rs](../../src/provider.rs)，`ArchiveName::parse`，行 14–16。

- 已重現輸入：`terraform-provider-random_1.0.0-beta_linux_amd64.zip`。
- 預期：version=`1.0.0-beta`、os=`linux`、arch=`amd64`。
- 實際：version=`1.0.0`、os=`beta`、arch=`linux`；末尾 `_amd64` 被 optional suffix 吞掉。
- 原因：version 使用 lazy matching，同時允許 `-` 作為 version 字元與欄位分隔符，且尾端容許任意 suffix，產生多義解析。
- 影響：合法 package 被 `validate_package` 判定不符合版本／平台而拒絕；archive request 也可能查錯版本。現有包含數字或句點的 prerelease 測試不足以涵蓋純字母 `-beta`。
- 修正：明確定義標準檔名格式，利用已知 provider type 與由右向左的平台欄位切割解析；若保留 legacy 格式，應使用獨立且無歧義的分支。
- 驗收：stable、`-beta`、`-rc`、`-rc.1`、build metadata、含連字號的 provider type，以及刻意混淆的平台／suffix 負面案例。

### R3 — P2：HTTPS 所謂 graceful drain 沒有啟動 Hyper connection shutdown

位置：[src/main.rs](../../src/main.rs)，行 323–362。

- `connection_shutdown` 只傳給 TLS handshake；成功握手後直接 await `serve_connection`，沒有監聽 shutdown 或呼叫 `graceful_shutdown()`。
- listener 停止接受連線後，程式只是等待 15 秒；既有 HTTP/1 keep-alive／HTTP/2 connection 仍能開始新請求，最後可能被 `abort_all()` 中斷。
- 實際重現：建立 HTTPS keep-alive 連線，先 GET `/livez` 得到 200；對測試子程序送 SIGTERM，等待 0.5 秒後在同一連線再送 GET，仍得到 `HTTP/1.1 200 OK`。
- 影響：rolling restart 時客戶端不會及早切換連線，新下載可在 grace period 尾端開始後被截斷。握手取消與 semaphore 等待取消的修正有效，但不能據此宣稱 established connection 已正常 drain。
- 修正：保留 shutdown receiver，pin Hyper connection，在訊號到達時呼叫 `connection.as_mut().graceful_shutdown()`，持續 poll 讓已開始的請求完成，再以 deadline 作最後保護；也應檢查 HTTP listener 的整體停機上限。
- 驗收：HTTP/1 keep-alive、HTTP/2 GOAWAY、正在傳輸的 archive、閒置連線、到期強制中止。測試不能只呼叫 `accept_tls_with_timeout`。
- 官方 API：[Hyper-util Connection::graceful_shutdown](https://docs.rs/hyper-util/latest/hyper_util/server/conn/auto/struct.Connection.html#method.graceful_shutdown)。

### R4 — P2：failure backoff maps 沒有淘汰到期 key

位置：[src/metadata.rs](../../src/metadata.rs)，`index_failures`／`version_failures` 與行 821–837。

- `refresh_allowed` 只讀取到期時間；`record_refresh_failure` 只 insert。只有同一 key 日後成功刷新，才會走 clear。
- 條件：長期執行，遇到大量不同 provider/version key 的 transient upstream error；這些 key 不再被成功請求。單一 key 的反覆失敗不會造成此增長。
- 影響：每個失敗 key 永久保留，即使 30 秒 backoff 早已結束。request rate limit 只限制速度，不能限制程序生命週期內的 map 大小。
- 證據：靜態控制流程確認；未進行大量請求或 OOM 壓力測試，不能據此估計實際記憶體耗盡時間。
- 修正：使用有容量上限與 TTL 的 cache，或以有界方式定期清理已到期項目。僅在相同 key 被查詢時清理，仍無法處理不再被請求的 key。
- 驗收：注入可控時間，產生多個不同失敗 key，時間前進後確認淘汰與最大容量；同 key 在 backoff 期間仍不可重送上游。

### R5 — P3：X.509 notAfter 邊界與標準不一致，且前次文件敘述錯誤

位置：[src/tls_reload.rs](../../src/tls_reload.rs)，行 167–169，以及 resolver 的 `now_epoch < not_after` 判斷；前次 audit 的 P1-2。

- RFC 5280 §4.1.2.5 定義有效期包含 notBefore 與 notAfter 兩端；程式註解卻稱 notAfter 為 exclusive，並以 `now >= not_after` 拒絕。
- 影響：以目前整秒時間表示，在 notAfter 相等那一秒提早拒絕；這不是原始自動換證失效的主因，也不應列為 P1。
- 修正：統一採標準的邊界判斷，或明確將提前截止定義為額外的操作政策，不可稱為 X.509 規定；同步更正原報告。建議避免自行擴張 DER/time parser，改用成熟 parser 並配合注入時鐘測試。
- 驗收：notBefore 前／相等、notAfter 相等／後、UTCTime／GeneralizedTime。
- 標準：[RFC 5280 §4.1.2.5](https://www.rfc-editor.org/rfc/rfc5280#section-4.1.2.5)。

## 其他建議與既有邊界

1. **P2：限制 cache-hit 驗證成本。** `ProviderStorage::load_or_fetch` 在取得 per-key lock 前便完整讀檔計算 SHA-256；download semaphore 不限制此路徑。大量客戶端命中同一大型 archive 時，會重複讀取與雜湊，之後 handler 再開檔串流一次。建議先量測，增加獨立驗證併發上限，並考慮以檔案 identity/size/mtime 綁定驗證結果；需要保留對檔案被修改的偵測能力。
2. **P2：module mirror 啟用前補足信任與離線路徑。** URL 只驗證 HTTP(S) scheme，archive redirect 未採 provider 的 public-IP/DNS pinning policy；handler 每次都先查 registry，已有 archive 時仍依賴 registry 可用。這些是預設停用功能的既有缺口，不能說本 PR 已把 module cache 完整強化。啟用前應加入同等下載政策、全域併發上限、cache-first 行為及真正 Terraform module protocol 驗收。
3. **P2：TLS reload 改為單次更新工作。** 現在在 rustls 同步 resolver 內讀檔與 parse；多執行緒同時跨過 reload interval 可重複執行。cached cert 已過期時會繞過 interval 的 fast path，replacement 仍錯誤就會每次握手重新讀檔與記錄錯誤。建議背景／single-flight reload、獨立的下次重試時間及原子快照發布。這是程式碼推導，未做負載量測。
4. **P3：把驗證變成可重複的 CI gate。** 目前 container job 做 build，沒有執行 `RUN_E2E=1`；完整 E2E 的成功來自本地執行。建議在排程或受控 CI 加入真實 TLS rotation、offline cache 與 lockfile-readonly 情境；固定必要工具並明確區分 skip 與 pass。

## 本次驗證結果

- PR #1 的 push 與 pull_request 共六個 jobs 均通過：Rust checks、Helm chart、Docker build and publish。job 名稱包含 publish，但此 branch/PR 的 tag-only publish steps 不會執行。
- `cargo build --locked --bin open-tf-mirror` 通過。
- 兩個臨時 Rust regression probes：**0 passed / 2 failed**，分別確認 R1 與 R2；測試檔已移出 repo，避免把已知失敗測試混入目前 PR。
- 真實 TLS socket 測試確認 R3：SIGTERM 後舊連線仍接受新 GET。
- 以本地檔案模擬 Kubernetes projected Secret 的 `..data` 原子 symlink 切換，透過全新 TLS handshake 觀察：5.2 秒後送出新 certificate；再切換為 malformed pair，等待 5.2 秒後仍送出 last-good certificate。**換證核心路徑通過**。此測試不是在 Kubernetes cluster 或 cert-manager 上執行，也不涵蓋 TLS session resumption。
- R4 為靜態確認；R5 以官方標準核對。此次沒有重跑整套 Docker E2E 或部署到 cluster。

## 建議處理順序

先修 R1、R2、R3 並將重現案例納入正式測試；R4 一併補容量／TTL 管理；R5 同步更正程式與 audit。原報告的「已修正並驗證」應在相應測試通過後再恢復，不應僅依目前 CI 全綠直接合併。
