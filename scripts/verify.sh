#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets

if command -v cargo-audit >/dev/null 2>&1; then
  cargo audit
fi

helm lint charts/open-tf-mirror
helm template open-tf-mirror charts/open-tf-mirror \
  --namespace open-tf-mirror >/tmp/open-tf-mirror-default.yaml
helm template open-tf-mirror charts/open-tf-mirror \
  --namespace open-tf-mirror \
  --set fullnameOverride=open-tf-mirror \
  --set openTfMirror.replicas=2 \
  --set openTfMirror.args[0]=--conn-burst=500 \
  --set openTfMirror.args[1]=--conn-qps=500 \
  --set openTfMirror.tls.enabled=true \
  --set openTfMirror.tls.secretName=open-tf-mirror-tls-secret \
  --set openTfMirror.pvc.size=20Gi \
  --set openTfMirror.pvc.storageClass=hyperdisk-balanced \
  >/tmp/open-tf-mirror-pricer.yaml

if command -v kubeconform >/dev/null 2>&1; then
  kubeconform -strict /tmp/open-tf-mirror-default.yaml
  kubeconform -strict /tmp/open-tf-mirror-pricer.yaml
fi

if [[ "${SKIP_DOCKER:-0}" != 1 ]]; then
  docker build --tag open-tf-mirror:verify .
fi

if [[ "${RUN_E2E:-0}" != 1 ]]; then
  echo "Static verification passed. Set RUN_E2E=1 to run the online-then-cached Terraform smoke test."
  exit 0
fi

if [[ "${SKIP_DOCKER:-0}" == 1 ]]; then
  echo "RUN_E2E=1 requires the Docker image for the network-isolated cache phase; unset SKIP_DOCKER" >&2
  exit 1
fi

for command in openssl curl jq socat terraform; do
  command -v "$command" >/dev/null || {
    echo "$command is required for RUN_E2E=1" >&2
    exit 1
  }
done

work=$(mktemp -d)
pid=""
offline_container=""
offline_network=""
offline_forwarder_pid=""
cleanup() {
  if [[ -n "$pid" ]]; then
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  if [[ -n "$offline_container" ]]; then
    docker rm -f "$offline_container" >/dev/null 2>&1 || true
  fi
  if [[ -n "$offline_network" ]]; then
    docker network rm "$offline_network" >/dev/null 2>&1 || true
  fi
  if [[ -n "$offline_forwarder_pid" ]]; then
    kill "$offline_forwarder_pid" 2>/dev/null || true
    wait "$offline_forwarder_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

mkdir -p "$work/data" "$work/terraform"
plugin_cache="$work/plugin-cache"
mkdir -p "$plugin_cache"
cp tests/fixtures/terraform-cache-smoke/main.tf "$work/terraform/main.tf"
openssl req -x509 -nodes -newkey rsa:2048 -days 1 \
  -keyout "$work/tls.key" -out "$work/tls.crt" \
  -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
  >/dev/null 2>&1
# Run the isolated container with the host UID so the bind-mounted cache stays
# writable without changing host ownership or requiring a privileged container.
chmod 644 "$work/tls.crt" "$work/tls.key"

if [[ -n "${OPEN_TF_MIRROR_TEST_HTTP_PORT:-}" ]]; then
  http_port="$OPEN_TF_MIRROR_TEST_HTTP_PORT"
else
  # Avoid colliding with a developer's local mirror or a previous interrupted run.
  http_port=$((20000 + RANDOM % 20000))
fi
if [[ -n "${OPEN_TF_MIRROR_TEST_HTTPS_PORT:-}" ]]; then
  https_port="$OPEN_TF_MIRROR_TEST_HTTPS_PORT"
else
  https_port=$((http_port + 1))
fi
cat >"$work/terraformrc" <<EOF
provider_installation {
  network_mirror {
    url     = "https://localhost:${https_port}/v1/providers/"
    include = ["registry.terraform.io/*/*"]
  }
}
EOF

cargo build --bin open-tf-mirror

start_server() {
  local log=$1
  shift
  env "$@" ./target/debug/open-tf-mirror \
    --bind-address=127.0.0.1 \
    --http-port="$http_port" \
    --https-port="$https_port" \
    --tls-cert-file="$work/tls.crt" \
    --tls-private-key-file="$work/tls.key" \
    --data-source-dir="$work/data" >"$log" 2>&1 &
  pid=$!
  for _ in $(seq 1 100); do
    if curl --cacert "$work/tls.crt" -fsS \
      "https://localhost:${https_port}/readyz" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      cat "$log" >&2
      return 1
    fi
    sleep 0.1
  done
  cat "$log" >&2
  return 1
}

run_init() {
  env \
    TF_CLI_CONFIG_FILE="$work/terraformrc" \
    TF_PLUGIN_CACHE_DIR="$plugin_cache" \
    SSL_CERT_FILE="$work/tls.crt" \
    "$@" \
    terraform -chdir="$work/terraform" init -input=false -no-color
}

start_server "$work/first.log"
run_init
archive=$(find "$work/data/providers" -type f -name '*.zip' -print -quit)
test -n "$archive"
mtime_before=$(stat -c %Y "$archive")
kill -TERM "$pid"
wait "$pid"
pid=""

rm -rf "$work/terraform/.terraform" "$work/terraform/.terraform.lock.hcl"
rm -rf "$plugin_cache"
mkdir -p "$plugin_cache"
docker image inspect open-tf-mirror:verify >/dev/null 2>&1 || {
  echo "open-tf-mirror:verify is required for the network-isolated cache phase" >&2
  exit 1
}
offline_network="open-tf-mirror-$(basename "$work")"
docker network create --internal "$offline_network" >/dev/null
offline_container=$(docker run --detach \
  --name "${offline_network}-server" \
  --network "$offline_network" \
  --user "$(id -u):$(id -g)" \
  --volume "$work/data:/var/run/open-tf-mirror" \
  --volume "$work/tls.crt:/tmp/open-tf-mirror-tls.crt:ro" \
  --volume "$work/tls.key:/tmp/open-tf-mirror-tls.key:ro" \
  open-tf-mirror:verify \
  --bind-address=0.0.0.0 \
  --http-port=8080 \
  --https-port=8443 \
  --tls-cert-file=/tmp/open-tf-mirror-tls.crt \
  --tls-private-key-file=/tmp/open-tf-mirror-tls.key \
  --data-source-dir=/var/run/open-tf-mirror)
offline_ip="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$offline_container")"
test -n "$offline_ip"
socat "TCP-LISTEN:${https_port},bind=127.0.0.1,reuseaddr,fork" "TCP:${offline_ip}:8443" \
  >"$work/offline-forwarder.log" 2>&1 &
offline_forwarder_pid=$!
offline_ready=0
for _ in $(seq 1 100); do
  if curl --cacert "$work/tls.crt" -fsS \
    "https://localhost:${https_port}/readyz" >/dev/null 2>&1; then
    offline_ready=1
    break
  fi
  if [[ "$(docker inspect -f '{{.State.Running}}' "$offline_container" 2>/dev/null || true)" != true ]]; then
    docker logs "$offline_container" >&2 || true
    exit 1
  fi
  sleep 0.1
done
if [[ "$offline_ready" != 1 ]]; then
  docker logs "$offline_container" >&2 || true
  cat "$work/offline-forwarder.log" >&2 || true
  exit 1
fi
run_init \
  HTTPS_PROXY=http://127.0.0.1:1 \
  NO_PROXY=localhost,127.0.0.1
test "$mtime_before" = "$(stat -c %Y "$archive")"

docker rm -f "$offline_container" >/dev/null
offline_container=""
docker network rm "$offline_network" >/dev/null
offline_network=""
kill "$offline_forwarder_pid" 2>/dev/null || true
wait "$offline_forwarder_pid" 2>/dev/null || true
offline_forwarder_pid=""

# Prove the failure mode as well: once the local archive is removed, a cached
# metadata record pointing at an unreachable private address must not produce a
# new archive or silently fall back to an external download.
metadata_file=$(find "$work/data/metadata" -type f -name '3.6.2.json' -print -quit)
test -n "$metadata_file"
rm -f "$archive"
rm -rf "$work/terraform/.terraform" "$work/terraform/.terraform.lock.hcl"
rm -rf "$plugin_cache"
mkdir -p "$plugin_cache"
jq '(.value.platforms[]?.download_url) = "https://127.0.0.1:1/blocked.zip"' \
  "$metadata_file" >"${metadata_file}.tmp"
mv "${metadata_file}.tmp" "$metadata_file"
start_server "$work/third.log" \
  HTTPS_PROXY=http://127.0.0.1:1 \
  NO_PROXY=localhost,127.0.0.1
if run_init \
  HTTPS_PROXY=http://127.0.0.1:1 \
  NO_PROXY=localhost,127.0.0.1; then
  echo "Terraform init unexpectedly succeeded without a cached provider archive" >&2
  exit 1
fi
test ! -e "$archive"
if find "$work/data/providers" -type f -name '*.tmp' -print -quit | grep -q .; then
  echo "failed offline download left a temporary provider archive" >&2
  exit 1
fi

kill -TERM "$pid"
wait "$pid"
pid=""
echo "Terraform online-then-cached network-isolation smoke test passed: $archive"
