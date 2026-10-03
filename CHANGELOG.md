# Changelog

## 0.3.0 — 2026-10-03

Reliability and performance release driven by the
[0.3.0 architecture review](docs/reviews/2026-10-03-architecture-review.md).

### Breaking

- The optional module mirror (`--enable-module-mirror`,
  `--module-registry-base`, `/v1/modules/...`) is removed. It was disabled by
  default, bypassed the outbound policy and did not implement the Terraform
  module protocol. Remove those flags from any deployment before upgrading.

### Fixed

- A transient `accept()` error (for example `EMFILE`) no longer terminates the
  server; the listener logs, backs off 100 ms and keeps serving.
- Plain HTTP now has the same 60 s header-read timeout, connection limit
  (`--conn-burst`) and graceful drain as HTTPS (one shared connection loop).
- Helm: `openTfMirror.env` no longer breaks rendering, so proxy variables can be
  set through the chart. The default startup probe timeout is now 5 s.
- TLS: certificate `notAfter` is inclusive (RFC 5280). If the cached
  certificate expired and the replacement is also broken, the files are
  re-read at most once per reload interval instead of on every handshake.
- A client disconnecting mid-download no longer aborts the upstream download
  that other requests are waiting for.
- `index.json` versions are normalized (no `v` prefix), de-duplicated and sorted
  by semver, consistent with `{version}.json` lookups.
- Stale `.tmp` files left by a crash are removed at startup.

### Performance

- One pooled upstream HTTP client is shared instead of building a new client
  (root store, TLS handshake) for every request and redirect. Direct mode keeps
  DNS-rebinding protection through a public-only DNS resolver; NAT64 and 6to4
  addresses with an embedded private IPv4 are rejected too.
- Archive cache hits no longer re-hash the whole file on every request. A
  verified checksum is remembered by file identity, and hashing runs on the
  blocking pool with a 256 KiB buffer.
- Cached metadata is shared through `Arc`, so a cache hit no longer clones the
  whole provider index.
- Archive responses include `Content-Length`.

## 0.2.1 — 2026-09-26

See [the 0.2.1 reliability review](docs/reviews/2026-09-26-reliability-fix-review.md).

## 0.2.0 — 2026-07-11

Initial persistent, verified provider mirror.
