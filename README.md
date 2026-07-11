# open-tf-mirror

`open-tf-mirror` is a persistent, on-demand Terraform/OpenTofu provider network
mirror. It is intended as a compatible replacement for the HermitCrab usage in
Pricer's infrastructure workflows while fixing custom TLS certificate rotation.

## Provider behavior

- Implements `index.json`, version metadata, and archive download endpoints
  below `/v1/providers/`.
- Fetches provider metadata from allowed origin registries and persists it for
  30 minutes by default.
- Serves stale persisted metadata if an origin is temporarily unavailable.
- Checks an optional bundled filesystem mirror, then the PVC cache, before
  downloading an archive.
- Streams archive downloads, verifies the registry SHA-256 checksum, and
  publishes them atomically.
- Deduplicates concurrent metadata refreshes and archive downloads.
- Restricts provider origins to `registry.terraform.io` by default and rejects
  non-HTTPS or non-public archive targets.

The optional module proxy is not part of the production compatibility contract
and is disabled by default. Enable it explicitly with
`--enable-module-mirror=true` only after reviewing its separate trust model.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/v1/providers/:hostname/:namespace/:type/index.json` | List versions. |
| `GET` | `/v1/providers/:hostname/:namespace/:type/:version.json` | List platform archives. |
| `GET` | `/v1/providers/:hostname/:namespace/:type/download/:archive` | Serve or populate an archive. |
| `PUT` | `/v1/providers/sync` | Refresh known provider indices. |
| `GET` | `/readyz` | Confirm the cache directory is writable. |
| `GET` | `/livez` | Confirm the process is alive. |

## Configuration

The current cert-manager deployment supplies a certificate and private key:

```shell
open-tf-mirror \
  --tls-cert-file=/etc/open-tf-mirror/ssl/tls.crt \
  --tls-private-key-file=/etc/open-tf-mirror/ssl/tls.key \
  --data-source-dir=/var/run/open-tf-mirror \
  --conn-qps=500 \
  --conn-burst=500
```

The key flags and environment variables are:

| Flag | Environment | Default |
| --- | --- | --- |
| `--bind-address` | `SERVER_BIND_ADDRESS` | `0.0.0.0` |
| `--http-port` | `SERVER_HTTP_PORT` | `8080` |
| `--https-port` | `SERVER_HTTPS_PORT` | `8443` |
| `--enable-tls` | `SERVER_ENABLE_TLS` | `true` |
| `--data-source-dir` | `SERVER_DATA_SOURCE_DIR` | `/var/run/open-tf-mirror` |
| `--allowed-registries` | `SERVER_ALLOWED_REGISTRIES` | `registry.terraform.io` |
| `--enable-module-mirror` | `SERVER_ENABLE_MODULE_MIRROR` | `false` |

Custom certificate files are re-read at most once every five seconds. A failed
reload keeps the last valid matching certificate and private key. With TLS
enabled, HTTP health probes remain available and other HTTP requests redirect to
HTTPS.

Persistent data uses this layout:

```text
<data-source-dir>/
├── metadata/<hostname>/<namespace>/<provider>/
└── providers/<hostname>/<namespace>/<provider>/
```

## Helm

The chart lives at `charts/open-tf-mirror` and can be consumed directly from an
immutable Git tag. The runtime and provider-copy init container use UID/GID
`10001`; the server has a read-only root filesystem and writes only to its PVC.
TLS is disabled by default. Enabling it requires an existing Secret through
`openTfMirror.tls.secretName`.

See [the chart README](charts/open-tf-mirror/README.md) for values and rendered
resource names.

## Verification

Run all static checks, RustSec audit, Helm renders, and the container build:

```shell
./scripts/verify.sh
```

Run the real Terraform TLS/cache smoke test as well:

```shell
RUN_E2E=1 ./scripts/verify.sh
```

The smoke test performs one online `terraform init`, restarts the mirror with an
unreachable upstream proxy, deletes Terraform's local plugin cache, and proves a
second init succeeds without changing the cached archive.

## Consumer migration

The consumer repositories have **not** been changed. Future agents must follow
these reviewed instructions:

- [`cloud-infra-argocd-apps`](docs/migration/cloud-infra-argocd-apps.md)
- [`cloud-infra-terragrunt-terraform`](docs/migration/cloud-infra-terragrunt-terraform.md)
