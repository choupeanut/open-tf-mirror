# open-tf-mirror Chart

This chart installs [choupeanut/open-tf-mirror](https://github.com/choupeanut/open-tf-mirror) with a StatefulSet, a ClusterIP Service, a headless Service, and a StatefulSet volume claim template.

## ArgoCD Values

```yaml
fullnameOverride: open-tf-mirror
openTfMirror:
  image:
    tag: v0.1.7
  args: []
  tls:
    enabled: true
    domainName: it-will-be-ignored.example.com
    secretName: open-tf-mirror-tls-secret
  replicas: 1
  resources: {}
  pvc:
    size: 20Gi
    storageClass: standard
```

With `fullnameOverride: open-tf-mirror`, resource names are:

| Resource | Name |
| --- | --- |
| StatefulSet | `open-tf-mirror` |
| Service | `open-tf-mirror` |
| Headless Service | `open-tf-mirror-headless` |
| PVC template | `data` |

The TLS secret from `openTfMirror.tls.secretName` is mounted at `/etc/open-tf-mirror/ssl`, and persistent data is mounted at `/var/run/open-tf-mirror`.

## Values

| Key | Default | Description |
| --- | --- | --- |
| `fullnameOverride` | `""` | Fully override the release base name. |
| `openTfMirror.replicas` | `1` | Number of pods. |
| `openTfMirror.image.repository` | `choupeanut/open-tf-mirror` | Image repository. |
| `openTfMirror.image.tag` | `0.1.0` | Image tag. |
| `openTfMirror.args` | `["--log-debug", "--log-verbosity=4"]` | Container args. |
| `openTfMirror.tls.enabled` | `true` | Enable TLS settings. |
| `openTfMirror.tls.domainName` | `""` | Domain for auto-cert argument. |
| `openTfMirror.tls.secretName` | `""` | Existing TLS secret to mount. |
| `openTfMirror.resources` | `{}` | Container requests and limits. |
| `openTfMirror.pvc.size` | `1Gi` | PVC template size. |
| `openTfMirror.pvc.storageClass` | `""` | PVC storage class. |
