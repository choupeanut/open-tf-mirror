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
upstream HermitCrab themes tracked in PR #28 and the current open issues #26
and #27 without importing HermitCrab's implementation: certificate reload
remains atomic, missing-version refresh handles the provider sync gap, direct
full-body downloads accept a normal `200 OK`, proxy/CA policy is explicit,
Teleport discovery keeps `/registry/`, and archive responses are streamed with
checksum verification.
Issue #22's request for disabling TLS verification is intentionally not
implemented; this release keeps HTTPS and certificate validation mandatory.

## Verification Gate

### Follow-up discovery corrections

The pre-release review found three discovery defects, now covered by regression
tests before publication:

- The discovered `providers.v1` URL is the complete API base. Version and package
  requests append only their protocol-relative endpoint, without an additional
  `v1/providers/` prefix. The fixtures now use the actual protocol paths.
- Relative service URLs resolve against the final discovery response URL after
  redirects, not the original hostname root.
- Cold discovery failures share the same 30-second hostname retry window as
  stale failures. An entry can have no last-good URL; after the retry deadline it
  can recover normally. The existing cache and lock are reused.

The corrected discovery integration tests failed against the previous code
(five failures, including duplicate cold requests) before the implementation
was changed. Coverage also includes package lookup and deterministic retry
expiry without sleeping for 30 seconds.

Local release verification passed with `RUN_E2E=1 ./scripts/verify.sh`: formatting,
Clippy, 81 tests, dependency audit, Helm lint/render, Docker build using Rust
1.88, and Terraform online installation followed by a restarted mirror on an
internal Docker network. Removing the cached archive then correctly failed
installation without leaving a temporary archive. The three independent review
probes also pass. Consumer repositories and deployments were not changed.

This evidence does not cover proxy-only/NO_PROXY and custom-CA handshake fixtures
or HTTP/2 GOAWAY and slow in-flight transfer shutdown tests; those remain test
coverage gaps rather than verified compatibility claims.

Before publishing `v0.2.1`, run `./scripts/verify.sh` (or the equivalent CI
steps) for formatting, Clippy, all deterministic tests, RustSec audit, Helm
render/lint, and Docker build. Run `RUN_E2E=1 ./scripts/verify.sh` for the
online-init, restart, and isolated-cache Terraform smoke test.
