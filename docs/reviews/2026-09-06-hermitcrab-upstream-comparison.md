# HermitCrab 官方 issues／PRs 與 open-tf-mirror 對照

檢查日期：2026-09-06。官方 repository：[seal-io/hermitcrab](https://github.com/seal-io/hermitcrab)。本地審查基準：`f8c5ed1e7d076353e8ff0a45836ea59c5d7456d5`，即 open-tf-mirror PR #1。

## 結論

**不能宣稱官方提出的問題在本專案都不會發生。** 換證、完整 200 下載與官方兩種 archive 命名案例已有對應證據；企業代理、其他 registry 的 service discovery、新版本可見性仍有相同使用情境下的缺口。OpenTofu 必須另外配置 allowlist，也尚未完成真實 OpenTofu E2E。

透過 GitHub API 列舉所有狀態，共 **6 issues + 23 PRs = 29 筆**。讀取全部 issue 內容，並讀取 #14、#15、#23、#24 的討論及相關功能 PR patches。依賴／CI／文件 PR 按技術適用性分類，不把「Closed」等同「已合併」，也不把無關的 Go 依賴更新視為 Rust 版已修復的證據。完整編號清單見附錄。

這次新增本地對照報告，未修改產品程式碼、發布 GitHub 留言、推送或部署。

## 核心對照

| 官方案例 | 官方目前狀態 | 本專案判定 | 證據／限制 |
| --- | --- | --- | --- |
| [PR #28：自訂 TLS 換證](https://github.com/seal-io/hermitcrab/pull/28) | Open；作者 choupeanut；無 issue comments、review comments 或 reviews | 核心問題已避免 | `ReloadingCertResolver` 按新握手重新取得憑證，正常 reload window 5 秒；前一輪真實 TLS + 原子 `..data` symlink rotation 通過，malformed replacement 保留 last-good |
| [Issue #27：代理移除 Range，200 被當錯誤](https://github.com/seal-io/hermitcrab/issues/27) | Open | 原始 Range 問題已避免；代理環境仍未相容 | downloader 不發 Range，接受 200、校驗 SHA-256 後才發布；本輪 mock 測試確認，但 `no_proxy()` 使企業代理需求仍失敗 |
| [Issue #26：AWS 新版本缺漏](https://github.com/seal-io/hermitcrab/issues/26) | Open；無後續診斷留言 | 仍可有相似症狀，不能宣稱相同根因 | 本地 fresh index 30 分鐘內不刷新；指定新版本未命中也不強制更新；本輪已重現，手動 sync 可解除 |
| [Issue #24](https://github.com/seal-io/hermitcrab/issues/24)／[PR #23：google-beta](https://github.com/seal-io/hermitcrab/pull/23) | Issue Closed；PR merged；維護者指向 v0.1.7 | 該精確案例已避免 | 本輪完整核對 provider type、version、OS、arch 均正確；不代表所有 prerelease 解析正確 |
| [Issue #22：Ingress 終止 TLS](https://github.com/seal-io/hermitcrab/issues/22) | Closed；無留言 | Pod TLS 可關閉 | `--enable-tls=false` 不需 cert/key；本輪啟動後 HTTP `/readyz` 為 200；Helm 預設關 TLS。Ingress 本身的 HTTPS/憑證仍由部署方提供 |
| [Issue #15](https://github.com/seal-io/hermitcrab/issues/15)／[PR #16](https://github.com/seal-io/hermitcrab/pull/16)／[PR #17：Teleport 非標準檔名與 v 前綴](https://github.com/seal-io/hermitcrab/pull/17) | Issue Closed；兩 PR merged；使用者在 v0.1.6 回報成功 | 檔名案例已避免，但整體 registry 流程不相容 | 檔名測試通過；實際 discovery 為 `/registry/`，本地固定 `/v1/providers/`，實際前者 200、後者 404 |
| [Issue #14：企業 proxy](https://github.com/seal-io/hermitcrab/issues/14) | Closed；討論表示同設 HTTP_PROXY 與 HTTPS_PROXY 可用 | 本專案仍有缺口 | provider archive client 明確 `.no_proxy()`；只設定兩個 env 不足以修復 |
| [PR #4：OpenTofu](https://github.com/seal-io/hermitcrab/pull/4) | Merged | 需設定，尚未完整驗收 | 預設僅允許 registry.terraform.io；本輪本地 OpenTofu index 請求為 400；擴大 allowlist 後仍需真實 tofu init 驗收 |
| [PR #6：health／metrics](https://github.com/seal-io/hermitcrab/pull/6) | Merged | health 有對應；metrics 未等價實作 | 本地 `/readyz` 實測可用，具寫入測試；無 Prometheus metrics endpoint／collector，原有監控不能直接移植 |
| [PR #13：datasource configuration](https://github.com/seal-io/hermitcrab/pull/13) | Merged | 部分不適用、部分功能不同 | 官方 patch 加 BoltDB mlock 選項；本地使用 JSON + files，沒有 BoltDB。可配置 data-source-dir，但不能直接讀官方 metadata.db |

## 仍需處理的問題與建議

### U1 — P1（限必須經代理連外的部署）：企業 proxy 與私有 CA 不相容

相關官方案例：[#14](https://github.com/seal-io/hermitcrab/issues/14)、[#27](https://github.com/seal-io/hermitcrab/issues/27)。

[src/storage.rs](../../src/storage.rs) `client_for_url` 行 384–390 明確關閉 proxy。RegistryClient 使用一般 reqwest client，而 archive client 強制直連，可能出現 metadata 可取得、archive 卻失敗的現象；沒有直連網路的環境中，cache miss 無法完成。

此外 [Cargo.toml](../../Cargo.toml) 使用 reqwest `rustls-tls`，目前鎖定 reqwest 0.12.28；本地 dependency source 顯示此 feature 導向 webpki roots。程式沒有額外 CA bundle 設定或 `add_root_certificate` 路徑。即使未來加入 proxy，企業 TLS interception 的 CA 信任仍需獨立處理；不能直接假設掛入 OS CA 或設定 SSL_CERT_FILE 就會被現在的 client 採用。此點是 feature／程式碼核對，未連接企業代理測試。

建議新增明確的 direct／trusted-proxy 模式與 outbound CA 設定，統一 metadata 和 archive 的 outbound policy。不能只是刪除 `no_proxy()`：現有 DNS pinning 是以 client 直連為前提，交由 proxy 解析目的位址時，要重新定義防 SSRF 的責任與 trusted-proxy／egress policy。

驗收：proxy-only 網路下 index、package、redirect、archive 全流程成功；企業 CA 正確時成功、錯誤時失敗；Range 不存在／被移除時仍處理完整 200；proxy 模式的內部位址限制有明確測試。

參考：[reqwest 0.12.28 ClientBuilder::no_proxy](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html#method.no_proxy)。

### U2 — P2：第三方 registry 缺少 service discovery，Teleport 目前確定不通

相關官方案例：[#15](https://github.com/seal-io/hermitcrab/issues/15)、[#16](https://github.com/seal-io/hermitcrab/pull/16)、[#17](https://github.com/seal-io/hermitcrab/pull/17)。

本輪讀取官方公開 metadata：

- [Teleport discovery](https://terraform.releases.teleport.dev/.well-known/terraform.json)：HTTP 200，`providers.v1 = "/registry/"`。
- [正確 versions URL](https://terraform.releases.teleport.dev/registry/gravitational/teleport/versions)：HTTP 200，本次 316 versions。
- [本專案目前會使用的 URL](https://terraform.releases.teleport.dev/v1/providers/gravitational/teleport/versions)：HTTP 404。

[src/registry.rs](../../src/registry.rs) `versions` 行 88–92 及 `package` 使用固定的根路徑，沒有依 `providers.v1` 解析。預設 allowlist 會先拒絕 Teleport；即使加進 allowlist，仍會遇到上述 404。

本輪 WireMock 測試也確認：提供 discovery 和正確 `/registry/` 回應時，RegistryClient 仍只請求錯誤的 `/v1/providers/...`，完全沒讀 discovery。因此只能說 archive 名稱與 v 前綴的原始 bug 已避免，不能說 Teleport 整體已相容。

建議實作並快取 discovery、解析相對或絕對 providers.v1 URL，或提供明確 origin-base 設定。API URL join 必須保留 discovery base path。所有 discovery/redirect/跨 host 行為須套用下載信任政策；目前 `with_origin` 為測試可用的 API，並不是可透過 CLI 配置 base path 的替代方案。

驗收：以官方精確 archive 檔名測 parser，再使用非 `/v1/providers/` base 的 registry 跑 index → version → archive 完整流程。

標準：[Terraform Remote Service Discovery](https://developer.hashicorp.com/terraform/internals/remote-service-discovery)。

### U3 — P2：新版本可見性仍會受到 stale/fresh cache 影響

相關官方案例：[#26](https://github.com/seal-io/hermitcrab/issues/26)。官方只有錯誤症狀，沒有證實根因，本報告不將下列本地行為推定成官方根因。

[src/metadata.rs](../../src/metadata.rs) `load_index` 在 TTL 內直接回傳 cache；`refresh_version` 在 index 找不到版本就回 NotFound，沒有對「指定版本 miss」重新抓 index。

本輪模擬：先載入 aws 6.21.0；上游新增 6.22.0 後，再 list 或 get 6.22.0 都未送出任何上游請求，結果仍只有 6.21.0／None。執行 `sync_known` 後才出現 6.22.0。這個測試使用合成版本列表，沒有聲稱是官方當時的實際環境。

30 分鐘快取屬既有設計；但仍會呈現「上游已有版本，mirror 說找不到」的同類症狀。上游持續失敗時，stale fallback 可能讓舊列表延續更久；目前沒有 stale 年齡上限或足夠 freshness 指標。若只在一個 replica 上手動 sync，其他 replica 的記憶體／各自 PVC index 不會一起更新。

建議：明確設定 freshness SLO、可配置 TTL、背景刷新或針對版本 miss 的受限刷新；保留 single-flight/backoff，避免錯誤版本造成 refresh storm。多 replica 的 sync 行為須設計。純粹 get_version miss 刷新也不足以解決 Terraform 在 index 階段就因 constraint 找不到版本而停止的情境。

同時修正前一份 [PR 複查 R1](2026-09-06-pr1-follow-up-review.md)：partial platform refresh 仍可能縮減完整 cache，造成另一種「cache 中的 provider 無法使用」。

### U4 — P2（OpenTofu 使用情境）：預設設定與驗收不足

相關官方案例：[PR #4](https://github.com/seal-io/hermitcrab/pull/4)、[#26](https://github.com/seal-io/hermitcrab/issues/26)、[#27](https://github.com/seal-io/hermitcrab/issues/27)。

預設 `SERVER_ALLOWED_REGISTRIES=registry.terraform.io`，本輪本地請求 `/v1/providers/registry.opentofu.org/hashicorp/random/index.json` 得到 400。這是已文件化的安全預設，不是隨機解析錯誤。

若需要原生 OpenTofu registry，至少明確加入：

```text
SERVER_ALLOWED_REGISTRIES=registry.terraform.io,registry.opentofu.org
```

CLI 的 network_mirror include 規則也必須涵蓋實際 provider hostname。本次從所在環境讀取 OpenTofu discovery 得到 403；未判定是官方服務政策或中間網路限制，也未完成 live OpenTofu archive／tofu init，因此不能保證僅修改 allowlist 就全部正常。

建議增加 Terraform 與 OpenTofu 雙 CLI matrix、代表性 provider、redirect、checksum、離線 cache、lockfile-readonly 驗收。README 的 Terraform/OpenTofu 描述應附必要設定及已驗證範圍。

### U5 — P2/P3：原 audit 的其他缺口仍成立

官方 google-beta 案例測試通過，但前輪確認的 `1.0.0-beta` prerelease 歧義仍存在；兩者分別是 provider 名稱與 version 字串，不能混為一談。前輪 HTTPS graceful shutdown、failure-map 淘汰及 X.509 邊界問題也尚未修正，詳見 [PR #1 複查](2026-09-06-pr1-follow-up-review.md)。

官方 metrics／BoltDB／Go security dependency PRs 需以不同架構判讀：本專案的 Rust server 沒有使用 Go runtime 或 BoltDB，對應套件更新不直接適用；但也沒有承接官方完整 metrics 能力。data-source-dir 路徑可配置，不代表 metadata.db 或原有 PVC layout 可以無轉換共用。

## 驗證摘要與邊界

- 本輪 5 個臨時 probes 全通過，其中 **2 個是刻意確認缺口的測試**（缺少 discovery、新版本在 fresh index 期間不可見），不是五個問題全解決。
- 另外三個 probes 確認 google-beta 精確欄位、Teleport 精確欄位、HTTP 200 完整下載且沒有 Range header。
- 臨時 probe 檔已移出 repo，未混入正式 CI。
- 本輪重跑 core_behavior、provider_storage、tls_reload 既有測試，共 25 個。
- 本輪 `cargo audit`：0 vulnerabilities、warnings 為空。這僅涵蓋 Rust dependency advisory，沒有掃描 Docker OS／bundled provider binaries。
- 本輪 HTTP-only 子程序 readyz 200，預設 OpenTofu registry 請求 400。
- TLS `..data` rotation 與 malformed replacement 的真實握手證據來自前一輪、同一 commit 的測試；本輪重跑 resolver tests，未再操作 Kubernetes／cert-manager。
- 沒有執行企業 proxy E2E、Teleport ZIP 下載或 live tofu init；不得用 parser test／metadata 200 取代這些訊號。

## 官方全部項目清單

下表為本次 GitHub API snapshot；Open/Closed/Merged 都是查詢時狀態。官方後續更新需重新查詢。

| # | 類型 | 狀態 | 標題 | 本地判定 |
| --- | --- | --- | --- | --- |
| [29](https://github.com/seal-io/hermitcrab/pull/29) | PR | Open | fix: correct tofo typo to tofu in README | README 拼字；本地無相同 tofo 指令 |
| [28](https://github.com/seal-io/hermitcrab/pull/28) | PR | Open | feat(tls): support dynamic certificate hot-reload for customized TLS mode | 換證核心已避免，見核心對照 |
| [27](https://github.com/seal-io/hermitcrab/issues/27) | Issue | Open | Feature Request: Support full file downloads (200 OK) when Range headers are stripped by proxies | 200/Range 已避免；proxy 仍缺，見 U1 |
| [26](https://github.com/seal-io/hermitcrab/issues/26) | Issue | Open | [Provider Sync] Missing hashicorp/aws provider versions | 類似版本不可見風險，見 U3 |
| [25](https://github.com/seal-io/hermitcrab/pull/25) | PR | Open | chore(deps): bump golang.org/x/crypto from 0.26.0 to 0.45.0 | Go 套件更新不直接適用；本輪 Rust audit 另行驗證 |
| [24](https://github.com/seal-io/hermitcrab/issues/24) | Issue | Closed | Error installing Terraform google-beta provider due to regex mismatch | google-beta 精確案例通過 |
| [23](https://github.com/seal-io/hermitcrab/pull/23) | PR | Merged | Add support for provider name contains "-", e.g. google-beta | google-beta 精確案例通過 |
| [22](https://github.com/seal-io/hermitcrab/issues/22) | Issue | Closed | How to Disable TLS | TLS 可停用；HTTP-only 實測通過 |
| [21](https://github.com/seal-io/hermitcrab/pull/21) | PR | Closed | chore(deps): bump github.com/getkin/kin-openapi from 0.122.0 to 0.131.0 | Go 套件更新不直接適用；本輪 Rust audit 另行驗證 |
| [20](https://github.com/seal-io/hermitcrab/pull/20) | PR | Closed | chore(deps): bump golang.org/x/net from 0.24.0 to 0.38.0 | Go 套件更新不直接適用；本輪 Rust audit 另行驗證 |
| [19](https://github.com/seal-io/hermitcrab/pull/19) | PR | Closed | chore(deps): bump golang.org/x/crypto from 0.26.0 to 0.35.0 | Go 套件更新不直接適用；本輪 Rust audit 另行驗證 |
| [18](https://github.com/seal-io/hermitcrab/pull/18) | PR | Closed | chore(deps): bump golang.org/x/net from 0.24.0 to 0.36.0 | Go 套件更新不直接適用；本輪 Rust audit 另行驗證 |
| [17](https://github.com/seal-io/hermitcrab/pull/17) | PR | Merged | refactor: ignore version prefix with v | v 前綴通過，registry discovery 缺口見 U2 |
| [16](https://github.com/seal-io/hermitcrab/pull/16) | PR | Merged | refactor: support datasource configuration | Teleport 檔名通過，discovery 缺口見 U2 |
| [15](https://github.com/seal-io/hermitcrab/issues/15) | Issue | Closed | 400 when downloading provider teleport on another registry | 檔名相容；整體 registry 流程仍不相容 |
| [14](https://github.com/seal-io/hermitcrab/issues/14) | Issue | Closed | Usage behind proxy | proxy 缺口仍存在，見 U1 |
| [13](https://github.com/seal-io/hermitcrab/pull/13) | PR | Merged | refactor: support datasource configuration | BoltDB mlock 不適用；cache migration 非等價 |
| [12](https://github.com/seal-io/hermitcrab/pull/12) | PR | Closed | chore(deps): bump google.golang.org/protobuf from 1.31.0 to 1.33.0 | Go 套件更新不直接適用；本輪 Rust audit 另行驗證 |
| [11](https://github.com/seal-io/hermitcrab/pull/11) | PR | Merged | chore(deps): bump golang.org/x/crypto from 0.14.0 to 0.17.0 | Go 套件更新不直接適用；本輪 Rust audit 另行驗證 |
| [10](https://github.com/seal-io/hermitcrab/pull/10) | PR | Merged | ci: adjust lint | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
| [9](https://github.com/seal-io/hermitcrab/pull/9) | PR | Merged | ci: support deps udpate | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
| [8](https://github.com/seal-io/hermitcrab/pull/8) | PR | Merged | chore: bump go version | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
| [7](https://github.com/seal-io/hermitcrab/pull/7) | PR | Merged | chore: remove useless lib | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
| [6](https://github.com/seal-io/hermitcrab/pull/6) | PR | Merged | refactor: implement measure | health 部分對應；metrics 缺口 |
| [5](https://github.com/seal-io/hermitcrab/pull/5) | PR | Merged | chore: fix golangci | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
| [4](https://github.com/seal-io/hermitcrab/pull/4) | PR | Merged | feat: support opentofu registry | OpenTofu 需設定且缺 E2E，見 U4 |
| [3](https://github.com/seal-io/hermitcrab/pull/3) | PR | Merged | ci: bump version | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
| [2](https://github.com/seal-io/hermitcrab/pull/2) | PR | Closed | ci: test | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
| [1](https://github.com/seal-io/hermitcrab/pull/1) | PR | Merged | docs: clarify | Go/官方 CI、release、文件維護；非同一實作，無直接移植結論 |
