# open-tf-mirror Chart

This chart installs [choupeanut/open-tf-mirror](https://github.com/choupeanut/open-tf-mirror) with a StatefulSet, a ClusterIP Service, a headless Service, and a StatefulSet volume claim template. It requires Kubernetes 1.21 or newer because it renders `policy/v1` PodDisruptionBudgets.

## ArgoCD Values

```yaml
fullnameOverride: open-tf-mirror
openTfMirror:
  image:
    tag: "release-version"
  args: []
  tls:
    enabled: true
    domainName: ""
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

The TLS secret from `openTfMirror.tls.secretName` is mounted at `/etc/open-tf-mirror/ssl`, and persistent data is mounted at `/var/run/open-tf-mirror`. Enabling TLS requires a non-empty existing Secret name; the chart does not issue ACME certificates. `domainName` is retained only for compatibility with older values and does not perform certificate issuance.

The server and provider-copy init container run without privilege under UID/GID `10001`. The server root filesystem is read-only; the PVC is its only writable runtime mount. When `openTfMirror.providersMirror.enabled` is true, the init container copies bundled providers into an `emptyDir` that is mounted read-only into the server and exposed through `TF_PLUGIN_MIRROR_DIR`.

`openTfMirror.service.targetPorts.http` and `.https` are the ports on which the
server listens. The chart adds matching `--http-port` and `--https-port` flags
unless the corresponding flag is already present in `openTfMirror.args`; an
explicit flag is retained for compatibility and must match the container port.
The Service's `ports` values remain the client-facing ports.

`global.imagePullSecrets` accepts either secret names or Kubernetes-style
`{name: ...}` entries. The chart normalizes both forms to
`LocalObjectReference` objects in the Pod spec.

Configure exactly one of `openTfMirror.pdb.minAvailable` and
`openTfMirror.pdb.maxUnavailable`. If neither is set, the chart keeps the
backward-compatible default of `minAvailable: 1`; numeric zero is preserved.

## Values

| Key | Default | Description |
| --- | --- | --- |
| `fullnameOverride` | `""` | Fully override the release base name. |
| `openTfMirror.replicas` | `1` | Number of pods. |
| `openTfMirror.image.repository` | `peanutchou/open-tf-mirror` | Image repository. |
| `openTfMirror.image.tag` | Chart `appVersion` | Image tag. |
| `openTfMirror.args` | `["--log-debug", "--log-verbosity=4"]` | Container args. |
| `global.imagePullSecrets` | `[]` | Registry secret names or `{name: ...}` objects. |
| `openTfMirror.service.ports` | `{http: 80, https: 443}` | Client-facing Service ports. |
| `openTfMirror.service.targetPorts` | `{http: 8080, https: 8443}` | Server listening/container ports. |
| `openTfMirror.pdb.minAvailable` | `1` when neither PDB field is set | Minimum available pods; mutually exclusive with `maxUnavailable`. |
| `openTfMirror.pdb.maxUnavailable` | unset | Maximum unavailable pods; mutually exclusive with `minAvailable`. |
| `openTfMirror.tls.enabled` | `false` | Enable TLS using an existing Secret. |
| `openTfMirror.tls.domainName` | `""` | Legacy compatibility value; certificate issuance is not implemented. |
| `openTfMirror.tls.secretName` | `""` | Existing TLS secret to mount. |
| `openTfMirror.resources` | `{}` | Container requests and limits. |
| `openTfMirror.pvc.size` | `1Gi` | PVC template size. |
| `openTfMirror.pvc.storageClass` | `""` | PVC storage class. |

The chart declares Apache-2.0 licensing in `Chart.yaml`; the repository-level
[`LICENSE`](../../LICENSE) file is the authoritative license text.
