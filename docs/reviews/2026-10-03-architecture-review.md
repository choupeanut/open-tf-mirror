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
