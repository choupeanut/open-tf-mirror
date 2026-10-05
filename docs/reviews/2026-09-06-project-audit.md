# open-tf-mirror 專案完整檢查與修正報告

日期：2026-09-06
檢查基準：`main`，原始基準 commit `8758ce58922cb5190706244c7c81779ae57b5115`
威脅模型：公司內部共用服務；上游 registry 與 provider archive 仍視為不完全可信，PVC 內容視為可能損壞，TLS Secret 由外部 cert-manager/Secret 流程維護。

這次工作同時檢查 Rust server、TLS reload、metadata/cache、provider archive policy、選用的 module mirror、Docker image、Helm chart、CI、E2E 驗證腳本與部署文件。沒有修改任何 consumer repository、GKE cluster 或 production 資源。

## 優先級結論

| 優先級 | 結論 | 狀態 |
| --- | --- | --- |
| P0 | 未發現會立即造成跨租戶資料外洩、任意檔案寫入或無法回滾的 release blocker。 | 無 P0 |
| P1 | TLS handshake/idle/shutdown、憑證有效期、stale metadata 併發重試會直接影響共用服務可用性。 | 已修正並驗證 |
| P2 | 依賴漏洞、損壞 cache、版本解析、partial platform、module mirror、Helm/CI 及 E2E 假陽性會在升級或異常情境造成錯誤。 | 主要項目已修正；剩餘項目列於下方 |
| P3 | 可觀測性、容量治理、多 replica cache 一致性與完整 Terraform module protocol 支援。 | 建議後續迭代 |

## P1：已完成的修正

### P1-1 TLS handshake 飽和會拖住服務退出

原本未完成的 TLS handshake 會佔住 connection semaphore，且 SIGTERM 不能立即取消等待中的 handshake。現在每條 handshake 有 10 秒上限，semaphore 等待與 handshake 都監聽 shutdown；已建立連線以 15 秒 graceful drain，超時後 `abort_all` 並 join。HTTP/1 header read idle timeout 為 60 秒，HTTP/2 keep-alive interval 為 30 秒、timeout 為 60 秒。實作在 [src/main.rs](../../src/main.rs) 的 `serve_https_with_timeouts`、`accept_tls_with_timeout`。

實際壓力測試以 `--conn-burst 2` 開兩條不送 TLS bytes 的 TCP 連線後送 SIGTERM，程序以約 0.008 秒、exit code 0 結束；不再重現原先超過 termination window 的 hang。

### P1-2 憑證 reload 可能接受過期或尚未生效的 replacement

reload 現在解析 certificate chain 的 X.509 validity，拒絕 `not_before` 尚未到或 `not_after` 已到的 certificate，並要求 certificate/private key match。reload 失敗時保留 last-good pair；若 last-good 本身也已過期，resolver 回傳 `None`，不會繼續提供過期 certificate。`notAfter` 以 exclusive upper bound 處理。實作在 [src/tls_reload.rs](../../src/tls_reload.rs)。

已加入 startup expired/not-yet-valid、replacement expired fallback、pair mismatch 與 symlink/file replacement 測試。

### P1-3 stale metadata 造成同一個 key 重複打上游

index 與 version metadata 都使用 per-key async lock；同一個 stale key 只有一個 refresh flight。短暫上游失敗會保留 stale value，並以 30 秒 failure backoff 避免每個請求重試。損壞的 persisted JSON 會 warning 後忽略並重新抓取。`NotFound`、invalid request 與 registry allowlist 錯誤不會被誤當成 transient backoff。實作在 [src/metadata.rs](../../src/metadata.rs)。

四個並發 stale index request 的 wiremock 驗證只產生一個上游 request，且全部回傳 stale 版本。

## P2：已完成的修正

### P2-1 依賴與供應鏈

- `h2` 從 `0.4.15` 升到 `0.4.16`，解除 RustSec `RUSTSEC-2026-0258`/GHSA-q83h-524g-xf6h 風險。
- lockfile 保持最小差異；`cargo audit` 報告 `vulnerabilities=false`、`count=0`。
- provider registry JSON、package JSON、module metadata 與 archive body 都有大小上限。

### P2-2 provider metadata 與 archive cache

- persisted index/version 的 malformed 或結構不合法內容不再阻止啟動；重新抓取前會驗證 version、platform、archive filename、checksum、HTTPS URL 與 metadata identity。
- provider metadata 多平台 fetch 以最多 8 個並行 request 進行；部分 platform 失敗時保留成功 platform，全部失敗才回傳錯誤。
- provider archive cache hit（包含 bundled mirror 與 PVC）會重新計算 SHA-256；checksum 不符、symlink、非 regular file 或超過大小上限都視為 cache miss。
- archive download 使用 `create_new` temporary file、streaming size guard、checksum verification、sync 與 atomic rename；失敗不留下 `.tmp`。
- `1.0.0-beta.1`、`1.0.0+build.7` 與 provider archive optional `v` prefix 現在可解析，不會因 prerelease 直接 404。
- provider download URL 維持 HTTPS、public-address DNS policy、redirect 上限與 DNS rebinding 防護。

主要實作在 [src/metadata.rs](../../src/metadata.rs)、[src/storage.rs](../../src/storage.rs)、[src/provider.rs](../../src/provider.rs)；測試在 `tests/provider_metadata.rs`、`tests/provider_storage.rs`。

### P2-3 optional module mirror 的明確邊界與資源上限

module mirror 預設仍關閉。啟用時現在有 module id/path traversal validation、metadata 256 KiB 上限、archive 1 GiB 上限、streaming download、per-module single-flight、unique temporary file 與 regular-file cache hit 檢查；`git::` source 會明確失敗，不會被誤當成 HTTP archive。實作在 [src/module_mirror.rs](../../src/module_mirror.rs)。

### P2-4 Helm chart correctness

- container listening ports 會由 `service.targetPorts` 產生 `--http-port`/`--https-port`，避免 Service/container port 漂移；使用者明確提供的 flag 會保留。
- `--enable-tls` 不會重複渲染。
- PDB 的 `minAvailable` 與 `maxUnavailable` 互斥，數值 `0` 不會因 Helm truthiness 消失；預設僅在兩者都未設定時使用 `minAvailable: 1`。
- `global.imagePullSecrets` 支援 string 與 `{name: ...}`，schema 會拒絕空或不完整 object。
- headless Service 與 provider-copy init container name 都保留 suffix 並限制在 Kubernetes 63 字元內。
- chart 宣告 `policy/v1` 所需的 Kubernetes `>=1.21`，補上 repository-level Apache-2.0 [LICENSE](../../LICENSE)。

主要檔案為 `charts/open-tf-mirror/templates/`、`values.schema.json`、`Chart.yaml`。

### P2-5 CI 與 E2E false-positive

- CI 不再硬編碼 image tag `0.2.0`，而是由 chart values 讀取；release 版本仍會比對 Cargo、Chart、appVersion 與 image tag。
- CI 增加 PDB zero/互斥、imagePullSecrets、long name、custom target port、duplicate TLS argument 的 render assertions。
- [scripts/verify.sh](../../scripts/verify.sh) 的 E2E 現在隔離 `TF_PLUGIN_CACHE_DIR`，第一次 online init 後清空 Terraform cache，再以 Docker `--internal` network、host-to-container forwarder 驗證第二次 cache hit；刪除 archive 並把 persisted metadata URL 改成 `127.0.0.1:1` 時，第三次 init 必須失敗且不得留下 partial archive。
- README、chart README 與 license 連結已補齊。

## 尚未關閉的風險與建議

這些項目目前沒有被誤標成已完成；它們是部署前需要接受或另開工作的邊界。

### P2-6 啟用 module mirror 前仍需獨立 trust policy

目前 HTTP module cache 只保證 direct HTTP(S) URL、大小與 path 安全，未實作 Terraform module protocol 的 `git::`、完整 redirect allowlist、public-IP DNS policy 或 module checksum verification。它預設關閉，並在 README 明確標示不屬於 production compatibility contract。若要啟用，應先決定允許的 module origin/host allowlist，再沿用 provider storage 的 DNS rebinding、redirect 與 public-IP policy。

### P2-7 PVC 沒有 aggregate quota、eviction 或 resource defaults

目前有單一 archive 1 GiB guard，但沒有整個 PVC 的容量配額、LRU/TTL eviction 或 `resources.requests/limits` 預設值；chart `resources` 仍為 `{}`。共享服務上線前應以實際 provider 數量與 archive 大小設定 PVC、CPU/memory request/limit、告警與清理策略，避免磁碟滿造成 readiness 或新下載失敗。

### P2-8 多 replica 是各自 PVC，沒有跨 pod cache 一致性

StatefulSet 每個 replica 使用自己的 PVC；PDB 只改善 voluntary disruption，不會把 cache 共享。若把 replicas 提高到 2 以上，必須接受 cold cache/重複下載，或改用 RWX/object storage/集中 cache。現行 `ReadWriteOnce` 預設值不適合假設跨 pod 共用同一份 archive。

### P2-9 管理操作與網路邊界

`PUT /v1/providers/sync` 沒有應用層 authentication；HTTP port 仍保留 health probe 與 redirect。此服務應只在 cluster-internal network 可達，並以 NetworkPolicy/ingress ACL 限制 Terraform runner、health checker 與管理來源；不要直接暴露到 Internet。

### P3-1 可觀測性與容量運維

建議加入 request outcome、upstream latency、metadata refresh failure/backoff、archive cache hit/miss/checksum failure、download bytes、PVC usage 與 TLS reload result 的 metrics；log 中維持 provider/version/platform 索引，避免把完整 URL query 或 Secret 內容寫入 log。

### P3-2 protocol 與部署驗證

建議補一個真實 Kubernetes Secret symlink rotation 測試，驗證 cert-manager renewal 在 mounted volume 下於五秒 reload window 內被採用；再以 Terraform lockfile-readonly、代表性 provider matrix 與 rollback revision 做 canary。完整 module protocol 支援與 cross-replica cache 應另開設計，不要在本次 provider mirror 修正中偷偷擴大。

## 驗證紀錄

以下檢查在本次修改後成功：

```text
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo test --locked --all-targets       # 62 tests passed
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo audit                              # vulnerabilities=false, count=0
helm lint charts/open-tf-mirror
kubeconform -strict <rendered chart manifests>
SKIP_DOCKER=1 ./scripts/verify.sh
OPEN_TF_MIRROR_TEST_HTTP_PORT=28080 OPEN_TF_MIRROR_TEST_HTTPS_PORT=28443 RUN_E2E=1 ./scripts/verify.sh
```

此外，以兩條未完成 TLS handshake 的實際 TCP connection 驗證 SIGTERM 約 0.008 秒退出。E2E 的第三次 Terraform init 以預期的 `502 Bad Gateway` 失敗，證明刪除 archive 後沒有靜默繞過 mirror 或建立未驗證檔案。

## 上線前 acceptance checklist

1. 以 immutable image tag 與同版本 Helm chart 部署一個 canary；確認 `/readyz`、`/livez`、HTTP redirect 與 HTTPS certificate fingerprint。
2. 讓 cert-manager 產生/更新 Secret，確認新 fingerprint 被採用，並故意放入 expired/mismatched pair，確認服務保留 last-good 或拒絕 handshake。
3. 以 `terraform init -lockfile=readonly` 驗證代表性 provider，刪除 runner 的 `.terraform` 與 plugin cache 後再次驗證 PVC cache hit。
4. 觀察 PVC usage、metadata refresh error、archive checksum failure、pod restart 與上游 latency；確認 NetworkPolicy 限制 `/v1/providers/sync` 與 HTTP port 的來源。
5. 保留舊版本 chart/image 與 PVC，完成 rollback rehearsal 後才切換。
