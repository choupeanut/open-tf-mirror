# open-tf-mirror

High-performance Rust implementation of a Terraform/OpenTofu network mirror.

## Features

- Terraform/OpenTofu provider network mirror endpoints under `/v1/providers/`.
- Local provider archive cache compatible with `terraform providers mirror`.
- Terraform module archive mirror endpoint under `/v1/modules/`; local cache hits
  are served directly and cache misses are fetched from the upstream registry.
- Custom TLS certificate reload for Kubernetes Secret/cert-manager rotation.
- Helm chart under `charts/open-tf-mirror`.

## CI

GitHub Actions runs on every push and pull request:

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets`
- Docker image build and Docker Hub publish from `main`
- Helm chart lint, compatibility render, and package artifact upload

## Helm Chart Versioning

The chart is versioned in `charts/open-tf-mirror/Chart.yaml`. CI packages the chart on
every run, so chart changes are tracked and validated with the code they deploy.
