# 0.2.1 Reliability Review

This release implements the scoped P1/P2 corrections for `open-tf-mirror`.
The consumer repositories remain unchanged; their rollout direction is in the
migration runbooks.

## Finding Mapping

| Finding | Correction | Evidence |
| --- | --- | --- |
| Partial provider metadata discarded a healthy platform | Merge successful package responses with validated cached `(os, arch)` entries; persist `complete: false` and retry after 30 seconds. | `stale_platform_metadata_survives_partial_refresh`, `persisted_partial_metadata_retries_after_short_ttl` |
| Failed refresh could replace usable metadata | Empty/all-failed refreshes return the upstream error while retaining the previous entry for stale fallback. | Existing `stale_version_packages_survive_restart_and_origin_failure` and metadata suite |
| Newly published versions were hidden by a fresh index | A missing version forces one per-provider index refresh under the existing lock, with a 30-second suppression window. | `missing_version_forces_one_index_refresh` |
| Refresh failure maps could grow without bound | Expired entries are pruned on write and the oldest entry is evicted above 4096 keys. | `metadata::tests::refresh_failure_map_is_bounded` |
| Archive names with prerelease versions were parsed incorrectly | Standard underscore and Teleport dash `-bin.zip` formats are parsed explicitly and validated with `semver`. | `parses_upstream_compatible_provider_archive_names` |
| Registry service discovery was missing | `providers.v1` discovery supports relative and absolute service URLs and preserves path prefixes; per-host discovery is single-flight and stale-capable. | `tests/registry_discovery.rs` |
| Archive and registry outbound behavior diverged | Shared outbound client provides direct DNS/IP policy, five redirects, trusted proxy mode, and startup-validated CA bundles. | `tests/outbound.rs`, provider storage policy suite |
| HTTPS connections accepted new requests after shutdown | Hyper connections call `graceful_shutdown`; HTTP and HTTPS use a 15-second drain deadline. | main listener implementation; TLS socket coverage remains a release gate |
| Helm redirected to container port instead of Service port | Chart sets `--https-redirect-port` from the client-facing HTTPS Service port and gives pods 30 seconds termination grace. | CI Helm render assertions |

## HermitCrab Compatibility

The implementation retains the Terraform provider network-mirror URL shape and
PVC archive layout used by the existing consumers. It also addresses the
upstream HermitCrab themes tracked in PR #28 and issues #14, #15, #22, #24,
#26, and #27 without importing HermitCrab's implementation: certificate
reload remains atomic, proxy/CA policy is explicit, Teleport discovery keeps
`/registry/`, and archive responses are streamed with checksum verification.
Issue #22's request for disabling TLS verification is intentionally not
implemented; this release keeps HTTPS and certificate validation mandatory.

## Verification Gate

Before publishing `v0.2.1`, run `./scripts/verify.sh` (or the equivalent CI
steps) for formatting, Clippy, all deterministic tests, RustSec audit, Helm
render/lint, and Docker build. Run `RUN_E2E=1 ./scripts/verify.sh` for the
online-init, restart, and isolated-cache Terraform smoke test.
