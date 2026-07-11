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

for command in openssl curl terraform; do
  command -v "$command" >/dev/null || {
    echo "$command is required for RUN_E2E=1" >&2
    exit 1
  }
done

work=$(mktemp -d)
pid=""
cleanup() {
  if [[ -n "$pid" ]]; then
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

mkdir -p "$work/data" "$work/terraform"
cp tests/fixtures/terraform-cache-smoke/main.tf "$work/terraform/main.tf"
openssl req -x509 -nodes -newkey rsa:2048 -days 1 \
  -keyout "$work/tls.key" -out "$work/tls.crt" \
  -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
  >/dev/null 2>&1

http_port=${OPEN_TF_MIRROR_TEST_HTTP_PORT:-18080}
https_port=${OPEN_TF_MIRROR_TEST_HTTPS_PORT:-18443}
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
start_server "$work/second.log" \
  HTTPS_PROXY=http://127.0.0.1:1 \
  NO_PROXY=localhost,127.0.0.1
run_init \
  HTTPS_PROXY=http://127.0.0.1:1 \
  NO_PROXY=localhost,127.0.0.1
test "$mtime_before" = "$(stat -c %Y "$archive")"

kill -TERM "$pid"
wait "$pid"
pid=""
echo "Terraform online-then-cached smoke test passed: $archive"
