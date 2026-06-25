# open-tf-mirror

High-performance Rust implementation of a Terraform/OpenTofu network mirror with
Hermit Crab-compatible deployment defaults.

## Features

- Terraform/OpenTofu provider network mirror endpoints under `/v1/providers/`.
- Local provider archive cache compatible with `terraform providers mirror`.
- Terraform module archive mirror endpoint under `/v1/modules/`; local cache hits
  are served directly and cache misses are fetched from the upstream registry.
- Custom TLS certificate reload for Kubernetes Secret/cert-manager rotation.
- Helm chart under `helm/hermitcrab` preserving the existing Hermit Crab chart
  values and Kubernetes resource naming contract.

## CI

GitHub Actions runs on every push and pull request:

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets`
- Docker image build
- Helm chart lint, compatibility render, and package artifact upload

## Helm Chart Versioning

The chart is versioned in `helm/hermitcrab/Chart.yaml`. CI packages the chart on
every run, so chart changes are tracked and validated with the code they deploy.
