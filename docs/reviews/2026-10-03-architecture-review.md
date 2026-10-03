# 0.3.0 架構 Review、Bug 檢視與實作計畫

日期：2026-10-03
審查基準：`c464078`（v0.2.1，`codex/project-hardening`）
範圍：`src/` 全部模組、`tests/`、Helm chart、CI、Dockerfile、`scripts/verify.sh`。
原則：**不做過度設計**。只修正會影響可用性、可靠性或效能的問題；不新增 metrics、
cluster 共享快取、ACME 等新功能。

## 架構評估

整體分層合理，保留：

| 模組 | 職責 | 評估 |
| --- | --- | --- |
| `main.rs` | CLI、listener、TLS、graceful shutdown | HTTP 與 HTTPS 走兩套不同的 serve 路徑，行為不一致（見 B1、B5） |
| `http_api.rs` | Terraform network mirror protocol、health、rate limit | 簡潔；需補 `Content-Length` |
| `metadata.rs` | index/version metadata 快取、single-flight、backoff、stale fallback | 正確性良好；每次請求整份 clone index（見 B8） |
| `registry.rs` | service discovery、registry API | 正確 |
| `outbound.rs` | direct / trusted-proxy 出站政策、DNS pinning | 每次請求重建 reqwest `Client`（見 B3） |
| `storage.rs` | archive 快取、SHA-256 驗證、原子發布 | 每次 cache hit 全檔重算 SHA-256（見 B4）；client 斷線會中止下載（見 B6） |
| `tls_reload.rs` | 憑證熱更新 | 正確；邊界與錯誤重試細節（見 B9） |
| `module_mirror.rs` | 選用的 module 代理 | **移除**（見 B7） |

## 發現的問題

| ID | 優先級 | 問題 | 影響 |
| --- | --- | --- | --- |
| B1 | P1 | HTTPS accept loop 遇到暫時性 `accept()` 錯誤（`EMFILE`、`ECONNABORTED`）時以 `?` 結束整個 server | 單一 fd 耗盡事件即可讓 pod 停止服務 HTTPS 並退出 |
| B2 | P1 | Helm `openTfMirror.env` 在 `with` 區塊內引用 `.Values`，任何自訂 env 都讓 chart render 失敗 | 無法透過 chart 設定 `HTTP_PROXY` 等 env，trusted-proxy 模式在 chart 中不可用 |
| B3 | P2 | `OutboundClient` 每次請求與每次 redirect 都建立新的 reqwest `Client`（載入 root store、無連線池） | metadata refresh 每個 platform 都要重新 TLS handshake；高延遲與 CPU 浪費 |
| B4 | P2 | archive cache hit 每次都以 64 KiB `tokio::fs` 讀取重算整個檔案 SHA-256 | 大型 provider（如 aws）多 runner 並行 init 時造成大量 CPU 與 I/O |
| B5 | P2 | 純 HTTP listener 使用 `axum::serve`，沒有 header read timeout 與連線上限 | TLS 關閉（chart 預設）時易受 slowloris 影響 |
| B6 | P2 | archive 下載在 request future 內執行，client 斷線即取消；等待中的請求會重新下載 | 大型 archive 在 client timeout 時反覆重抓上游 |
| B7 | P2 | module mirror 未走 outbound 政策（無 public-IP/DNS pinning、redirect 不驗證）、每次請求建 client、不是 Terraform module protocol | 預設關閉但增加攻擊面與維護成本，且無實際用途 |
| B8 | P3 | `load_index`/`load_version` 每次請求 clone 整個 `CacheEntry`（aws 約上千版本 × 平台） | 每個 `index.json` 請求數萬次配置 |
| B9 | P3 | TLS：`notAfter` 以 exclusive 判斷（RFC 5280 為 inclusive）；cached 憑證過期且檔案錯誤時每次 handshake 都重讀檔案並記錄錯誤 | 邊界秒提早拒絕；錯誤情境下 log 洪水 |
| B10 | P3 | 程序被 SIGKILL 後 `.tmp` 檔永久殘留在 PVC | PVC 空間緩慢流失 |
| B11 | P3 | `index.json` 直接輸出上游原始版本字串（可能含 `v` 前綴、重複） | 與 `{version}.json` 正規化不一致 |
| B12 | P3 | archive 回應沒有 `Content-Length` | client 無法顯示進度、無法偵測截斷 |

## 實作計畫

先完成基礎 commit（移除 module mirror、啟動時清理 `.tmp`：B7、B10），再依檔案切分平行實作：

| 工作 | 檔案 | 內容 |
| --- | --- | --- |
| A — server | `src/main.rs` | HTTP/HTTPS 共用單一 hyper-util 連線迴圈：accept 錯誤不致命、header timeout、連線上限、graceful drain（B1、B5） |
| B — outbound | `src/outbound.rs`、`tests/outbound.rs` | 建構時建立一次共用 `Client`；direct 模式以自訂 DNS resolver 過濾非公開位址（同時保留 DNS pinning），IP literal 在 URL 驗證時檢查（B3） |
| C — storage/API | `src/storage.rs`、`src/http_api.rs`、相關 tests | 以檔案 identity（dev/inode/len/mtime）記住已驗證 checksum；hash 移到 `spawn_blocking`；下載改為 detached task；`Content-Length`（B4、B6、B12） |
| D — metadata/TLS/chart | `src/metadata.rs`、`src/tls_reload.rs`、chart、CI | `Arc` 快取項目、版本正規化去重、`notAfter` inclusive、過期時重試節流、chart env 修正與 CI 斷言（B2、B8、B9、B11） |

驗收：`cargo fmt`、`clippy -D warnings`、`cargo test`、`cargo audit`、`helm lint`、
kubeconform、Docker build，以及 `RUN_E2E=1` 的 Terraform online → offline cache 煙霧測試。

## 不做的事項（刻意保留）

- Prometheus metrics、跨 replica 共享快取、PVC 自動淘汰、ACME：都是新功能，不在可靠性修正範圍。
- 以 `x509-parser` 取代自寫 DER validity parser：現有 parser 範圍小且有測試，新依賴的成本較高。
- `PUT /v1/providers/sync` 驗證：仍以 NetworkPolicy 限制，維持 HermitCrab 相容行為。

## 實作與驗收結果

實作由 4 個 Sonnet 5.5 sub-agent 依上表平行完成（各自 worktree），再由 Opus 5.5 agent 審查與驗收。

| 項目 | Commit | 結果 |
| --- | --- | --- |
| B7、B10 | `3b0ac99` | module mirror 移除；啟動時清理 `.tmp`（失敗只記 warning） |
| B3 | `e2b7cd9` | 單一 pooled client + `PublicOnlyResolver`；IP literal、NAT64、6to4 檢查 |
| B1、B5 | `34dad47` | HTTP/HTTPS 共用 `serve_listener`；accept 錯誤退避 100 ms |
| B4、B6、B12 | `a50e558` | 以檔案 identity 記住驗證結果；detached download；`Content-Length` |
| B2 | `1a565c2` | chart env 修正；startup probe timeout 5 s；CI render 斷言 |
| B8、B9、B11 | `68c53dc` | `Arc` 快取；`index.json` 版本正規化；TLS `notAfter` inclusive 與重試節流 |

### Opus 驗收發現

| 嚴重度 | 問題 | 處理 |
| --- | --- | --- |
| High（本次回歸） | hyper-util `auto::Builder` 判斷 HTTP 版本時的初次讀取沒有 timeout；統一 serve loop 後純 HTTP 也有連線上限，200 條不送資料的連線即可鎖住 listener | `de16044`：交給 hyper 前，在 handshake timeout（10 s）內 `peek` 出協定；加回歸測試 |
| Low | CI 中段的 `! grep` 不會觸發 errexit，等於沒有斷言 | 改為 `if grep ...; then exit 1; fi`，並在本機逐步執行全部通過 |
| Low | 啟動清理 `.tmp` 失敗會阻止啟動 | 改為 warning |
| Low | detached download 失敗而所有 client 都已斷線時沒有 log | 在 task 內記錄 |
| Low（既有） | TLS 握手完成後靜默、閒置 HTTP/2 連線會持續佔用 permit | 記錄於 README Operational notes |

### 驗收紀錄

- `cargo fmt --check`、`cargo clippy -D warnings`（stable 與 MSRV 1.88）：通過
- `cargo test --all-targets`：92 passed / 0 failed
- `cargo audit`：0 vulnerabilities
- CI helm job 逐步本機執行、kubeconform `-strict`：通過
- `./scripts/verify.sh`（含 Docker build）與 `RUN_E2E=1`：通過。online `terraform init` → 隔離網路下 cache hit → 刪除 archive 後預期 502 且沒有殘留 `.tmp`
- 手動 smoke：`hashicorp/random` 3.6.2 archive 的 `Content-Length` 正確，sha256 與 `zh:` hash 相符；第二次請求為 cache hit；SIGTERM 約 0.01 s 正常結束

## PR #1 合併前複審（2026-10-04）

複審整個 PR（`main..codex/project-hardening`，39 個檔案）。0.3.0 的內容已併入 PR 分支；
另外重跑測試 5 次，沒有 flaky test。

| 嚴重度 | 問題 | 處理 |
| --- | --- | --- |
| P1（0.3.0 回歸） | 統一 serve loop 後，`--conn-burst`（預設 200，HermitCrab 的請求速率 burst）同時成為純 HTTP 的連線上限。chart 預設走純 HTTP，Terraform client、ingress keep-alive pool 與 kubelet probe 共用一個 listener；200 條閒置 keep-alive 連線即可讓 `/livez` 卡在 backlog，pod 在負載下被重啟 | 新增 `--max-connections`（`SERVER_MAX_CONNECTIONS`，預設每個 listener 4096），`--conn-burst` 恢復為純速率限制；實測 `--conn-burst=2` 加 10 條閒置連線時 probe 立即回 200 |

其餘檔案（chart template、schema、PDB、CI、verify.sh、storage memo、metadata、TLS）未發現需修正的問題。
