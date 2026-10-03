# Argo CD consumer migration

This runbook describes the consumer-side changes for replacing HermitCrab with
`open-tf-mirror`. It is intentionally kept in this repository as documentation;
the consumer repository is not modified by the chart release.

## Deploy the chart

Create an ApplicationSet entry that sources the immutable release tag from
`https://github.com/choupeanut/open-tf-mirror.git` and uses the path
`charts/open-tf-mirror`. Keep the existing cluster generator and namespace
ownership model. Pin `targetRevision` to a release tag rather than `main`.

For this revision, use chart/image `0.3.0` (tag `v0.3.0`). Before upgrading
from 0.2.x, remove any `--enable-module-mirror` / `--module-registry-base`
arguments (the module mirror was removed). Custom `openTfMirror.env` entries now
render correctly. If many Terraform runners connect at the same time, raise
`--conn-burst`: it now limits plain HTTP connections as well. The chart keeps the
existing client-facing HTTPS Service port at 443 while passing the Service port
to `--https-redirect-port`; do not hard-code the container port 8443 in the
redirect setting. Preserve existing `env`/`envFrom` entries for proxy settings.

Set the image tag to the same release version as the chart `appVersion`. Keep
the existing PVC size and storage class for the target cluster. For TLS, apply a
cert-manager `Certificate` in the same namespace, with `spec.secretName` equal
to `openTfMirror.tls.secretName`, and set `openTfMirror.tls.enabled: true`.
The Secret volume is mounted at `/etc/open-tf-mirror/ssl`; the application
reloads a valid renewed certificate without a pod restart.
If the upstream registry uses a private CA, create a Secret with key `ca.crt`
and set `openTfMirror.upstreamCA.secretName`; the chart mounts it read-only and
configures the startup-validated CA bundle path. Use `openTfMirror.args` or
`env`/`envFrom` for `trusted-proxy` and proxy variables; there is intentionally
no second proxy-specific values interface.

The chart requires Kubernetes 1.21 or newer. Before syncing, render the exact
ApplicationSet values with Helm and check that the Service, headless Service,
StatefulSet, PVC template, PDB, and TLS Secret name are all in the expected
namespace. After syncing, verify `/readyz` and `/livez` through the HTTPS
Service and confirm that the certificate fingerprint changes after a test
Secret rotation.

## Rollback

Keep the previous HermitCrab Application available until a consumer has
completed a locked provider installation through the mirror. Roll back by
restoring the previous ApplicationSet revision and leave the mirror PVC intact
so a later retry can reuse verified archives.
