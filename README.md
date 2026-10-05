# open-tf-mirror

`open-tf-mirror` is a persistent, on-demand Terraform/OpenTofu provider
[network mirror](https://developer.hashicorp.com/terraform/internals/provider-network-mirror-protocol)
written in Rust. It replaces the [HermitCrab](https://github.com/seal-io/hermitcrab)
deployment, keeps the same URL shape and Helm values, and fixes the HermitCrab

## Why not HermitCrab

| HermitCrab case | open-tf-mirror behaviour |
| --- | --- |
| [PR #28](https://github.com/seal-io/hermitcrab/pull/28): custom TLS certificates are never reloaded | Certificate and key are re-read at most every 5 s on new handshakes. A broken or expired replacement keeps the last valid pair, and the `notAfter` boundary is inclusive (RFC 5280). |
| [#27](https://github.com/seal-io/hermitcrab/issues/27): proxies strip `Range`, so a `200 OK` is treated as an error | Archives are always fetched as a full `200` stream, then SHA-256-verified before they are published. |
| [#26](https://github.com/seal-io/hermitcrab/issues/26): newly published versions are missing | A request for a version that is not in the cached index forces one index refresh, with a 30 s per-provider backoff. |
| [#14](https://github.com/seal-io/hermitcrab/issues/14): cannot be used behind a proxy | `--outbound-mode=trusted-proxy` honours `HTTP(S)_PROXY`/`NO_PROXY`, and `--upstream-ca-file` trusts a corporate CA. |
| [#15](https://github.com/seal-io/hermitcrab/issues/15): Teleport and other third-party registries return 400/404 | Terraform service discovery (`providers.v1`) is followed, path prefixes such as `/registry/` are kept, and both archive naming styles are parsed. |
| [#24](https://github.com/seal-io/hermitcrab/issues/24): provider types containing `-` (`google-beta`) | Archive names are parsed from the right using the known provider type and validated as semver. |
| [#22](https://github.com/seal-io/hermitcrab/issues/22): how to disable TLS | `--enable-tls=false` (the Helm default) serves plain HTTP behind an ingress. |

## Provider behaviour

- Implements the network mirror protocol below `/v1/providers/`: `index.json`,
  `{version}.json` and archive downloads.
- Persists provider metadata for 30 minutes by default. A partial platform
  result is kept and retried after 30 seconds. A failed refresh never replaces
  valid data, and stale metadata is served while an origin is down.
- Archive lookup order is the optional bundled filesystem mirror, then the PVC
  cache, then upstream. Each file's checksum is verified once and remembered by
  file identity (device, inode, size, mtime). A file that is replaced or
  corrupted is hashed again.
- Upstream downloads stream to a temp file and are size-capped, checksum-verified,
  fsynced and atomically renamed into place. Each archive is downloaded once
  even under concurrent requests. A download runs to completion even if the
  requesting client disconnects.
- Responses carry `Content-Length`. Archives are streamed from disk.
- Only `registry.terraform.io` is allowed by default; add OpenTofu with
  `--allowed-registries=registry.terraform.io,registry.opentofu.org`.
- Leftover `.tmp` files from an interrupted run are removed at startup.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/v1/providers/:hostname/:namespace/:type/index.json` | List versions (normalized, de-duplicated). |
| `GET` | `/v1/providers/:hostname/:namespace/:type/:version.json` | List platform archives with `zh:` hashes. |
| `GET` | `/v1/providers/:hostname/:namespace/:type/download/:archive` | Serve or populate an archive. |
| `PUT` | `/v1/providers/sync` | Refresh every known provider index. |
| `GET` | `/readyz` | Confirm the cache directory is writable. |
| `GET` | `/livez` | Confirm the process is alive. |

Provider endpoints are rate-limited by a token bucket (`--conn-qps`,
`--conn-burst`); excess requests get `429` with `Retry-After: 1`.
`PUT /v1/providers/sync` has no application-level authentication. Restrict it
with a NetworkPolicy or ingress ACL.

## Configuration

```shell
open-tf-mirror \
  --tls-cert-file=/etc/open-tf-mirror/ssl/tls.crt \
  --tls-private-key-file=/etc/open-tf-mirror/ssl/tls.key \
  --data-source-dir=/var/run/open-tf-mirror \
  --conn-qps=500 \
  --conn-burst=500
```

| Flag | Environment | Default |
| --- | --- | --- |
| `--bind-address` | `SERVER_BIND_ADDRESS` | `0.0.0.0` |
| `--http-port` | `SERVER_HTTP_PORT` | `8080` |
| `--https-port` | `SERVER_HTTPS_PORT` | `8443` |
| `--https-redirect-port` | `SERVER_HTTPS_REDIRECT_PORT` | HTTPS listener port |
| `--enable-tls` | `SERVER_ENABLE_TLS` | `true` |
| `--tls-cert-file` / `--tls-private-key-file` | `SERVER_TLS_CERT_FILE` / `SERVER_TLS_PRIVATE_KEY_FILE` | unset (required with TLS) |
| `--data-source-dir` | `SERVER_DATA_SOURCE_DIR` | `/var/run/open-tf-mirror` |
| `--allowed-registries` | `SERVER_ALLOWED_REGISTRIES` | `registry.terraform.io` |
| `--metadata-ttl-seconds` | `SERVER_METADATA_TTL_SECONDS` | `1800` (minimum `30`) |
| `--outbound-mode` | `SERVER_OUTBOUND_MODE` | `direct` |
| `--upstream-ca-file` | `SERVER_UPSTREAM_CA_FILE` | unset |
| `--conn-qps` / `--conn-burst` | — | `100` / `200` (provider request rate limit) |
| `--max-connections` | `SERVER_MAX_CONNECTIONS` | `4096` (concurrent connections per listener) |
| `--log-debug`, `--log-verbosity` | `RUST_LOG` overrides both | `info` |
| — | `TF_PLUGIN_MIRROR_DIR` | unset (optional bundled mirror) |

### Listeners

HTTP and HTTPS share one connection loop with these settings:

- At most `--max-connections` concurrent connections per listener. This is
  separate from the `--conn-qps`/`--conn-burst` request rate limit, so idle
  keep-alive connections cannot starve health probes.
- A 60 s HTTP/1 header-read timeout, plus HTTP/2 keep-alive pings every 30 s
  with a 60 s timeout.
- A 10 s TLS handshake timeout.
- Accept errors such as `EMFILE` are retried after 100 ms instead of
  terminating the process.

On `SIGTERM`, both listeners stop accepting and ask live connections to close
gracefully (HTTP/1 `Connection: close`, HTTP/2 `GOAWAY`). Connections still open
after 15 s are cancelled.

With TLS enabled, `/readyz` and `/livez` stay available on HTTP, and other
`GET`/`HEAD` requests redirect to HTTPS on `--https-redirect-port`.

### Outbound policy

- **`direct`** (default): proxy variables are ignored. The upstream TLS client
  is shared and pooled. A custom DNS resolver rejects any name that resolves to
  a private, loopback, link-local, CGNAT or documentation address (including
  IPv4-mapped, NAT64 and 6to4 forms). The client connects only to the addresses
  that resolver vetted, so DNS rebinding cannot bypass the check. IP-literal
  URLs are checked the same way. Every redirect hop (max 5) must be HTTPS and is
  re-validated.
- **`trusted-proxy`**: requires `HTTP_PROXY`/`HTTPS_PROXY` (or lowercase) and
  applies `NO_PROXY`. Egress isolation becomes the proxy's job.
- `--upstream-ca-file` adds a PEM CA bundle. A malformed bundle fails startup,
  and TLS verification cannot be disabled.

### Persistent layout

```text
<data-source-dir>/
├── metadata/<hostname>/<namespace>/<provider>/{index.json,<version>.json}
└── providers/<hostname>/<namespace>/<provider>/<archive>.zip
```

## Helm

The chart lives at `charts/open-tf-mirror` and can be consumed from an immutable
Git tag. It has these defaults:

- The server and provider-copy init container run as UID/GID `10001`.
- The server's root filesystem is read-only, and it writes only to its PVC.
- TLS is off. Enabling it requires an existing Secret in
  `openTfMirror.tls.secretName`.
- `openTfMirror.upstreamCA.secretName` mounts `ca.crt` for private registry
  CAs.
- Extra variables go in `openTfMirror.env`, for example `HTTPS_PROXY` for
  `trusted-proxy` mode.

See [the chart README](charts/open-tf-mirror/README.md).

Each StatefulSet replica has its own PVC; caches are not shared across pods.

## Operational notes

- `--max-connections` caps concurrent connections on **each** listener.
  Connections over the cap wait in the kernel listen backlog. The default of
  4096 needs a matching open-file limit; container runtimes normally allow
  far more.
- A plain-HTTP client must send its first bytes within 10 s. Idle HTTP/1
  keep-alive connections close after 60 s. An idle HTTP/2 connection that keeps
  answering pings stays open.
- A cache hit does not re-hash an unchanged file. Silent on-disk corruption is
  only detected when the file's identity changes or the process restarts.
  Terraform still verifies the `zh:` hash it was given.
- Startup deletes `.*.tmp` under `providers/` and `metadata/`. Do not point two
  processes at the same data directory.

## Verification

```shell
./scripts/verify.sh            # fmt, clippy, tests, cargo-audit, helm, kubeconform, docker build
RUN_E2E=1 ./scripts/verify.sh  # plus a real Terraform online -> offline cache smoke test
```

The E2E run performs one online `terraform init` through the mirror. It then
restarts the mirror on a network-isolated Docker network, deletes Terraform's
plugin cache, and proves a second init succeeds from the PVC cache alone. Last,
it shows that a deleted archive cannot be silently refetched. It needs Docker,
Terraform, `openssl`, `curl`, `jq` and `socat`.

## Documentation

- [Changelog](CHANGELOG.md)
- [0.3.0 architecture review and plan](docs/reviews/2026-10-03-architecture-review.md)
- [HermitCrab upstream comparison](docs/reviews/2026-09-06-hermitcrab-upstream-comparison.md)

## License

Apache-2.0, see [LICENSE](LICENSE).
