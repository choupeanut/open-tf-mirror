# Hermit Crab Chart

This chart installs [seal-io/hermitcrab](https://github.com/seal-io/hermitcrab) with a StatefulSet, a ClusterIP Service, a headless Service, and a StatefulSet volume claim template.

## ArgoCD Compatibility

The existing ArgoCD values shape is preserved:

```yaml
fullnameOverride: hermitcrab
hermitcrab:
  image:
    tag: v0.1.7
  args: []
  tls:
    enabled: true
    domainName: it-will-be-ignored.example.com
    secretName: hermitcrab-tls-secret
  replicas: 1
  resources: {}
  pvc:
    size: 20Gi
    storageClass: standard
```

With `fullnameOverride: hermitcrab` and the default `hermitcrab.name: hermitcrab`, resource names are:

| Resource | Name |
| --- | --- |
| StatefulSet | `hermitcrab-hermitcrab` |
| Service | `hermitcrab-hermitcrab` |
| Headless Service | `hermitcrab-hermitcrab-headless` |
| PVC template | `data` |

The TLS secret from `hermitcrab.tls.secretName` is mounted at `/etc/hermitcrab/ssl`, and persistent data is mounted at `/var/run/hermitcrab`.

## Values

| Key | Default | Description |
| --- | --- | --- |
| `fullnameOverride` | `""` | Fully override the release base name. |
| `hermitcrab.name` | `hermitcrab` | Hermit Crab resource suffix. |
| `hermitcrab.replicas` | `1` | Number of pods. |
| `hermitcrab.image.repository` | `sealio/hermitcrab` | Image repository. |
| `hermitcrab.image.tag` | `v0.1.4` | Image tag. |
| `hermitcrab.args` | `["--log-debug", "--log-verbosity=4"]` | Container args. |
| `hermitcrab.tls.enabled` | `true` | Enable TLS settings. |
| `hermitcrab.tls.domainName` | `""` | Domain for auto-cert argument. |
| `hermitcrab.tls.secretName` | `""` | Existing TLS secret to mount. |
| `hermitcrab.resources` | `{}` | Container requests and limits. |
| `hermitcrab.pvc.size` | `1Gi` | PVC template size. |
| `hermitcrab.pvc.storageClass` | `""` | PVC storage class. |
