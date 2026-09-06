# Terraform consumer migration

This runbook describes the consumer-side change from HermitCrab to the
`open-tf-mirror` network mirror. It does not edit the Terraform consumer
repository.

## Configure the provider mirror

In the runner or job that executes Terraform, set `TF_CLI_CONFIG_FILE` to a
configuration containing a `network_mirror` entry for the mirror's HTTPS
Service URL:

```hcl
provider_installation {
  network_mirror {
    url     = "https://open-tf-mirror.open-tf-mirror.svc.cluster.local/v1/providers/"
    include = ["registry.terraform.io/*/*"]
  }
}
```

Use the cluster-local DNS name and port exposed by the deployed Service. The
runner must trust the CA that issued the mirror's cert-manager certificate. Do
not disable TLS verification or point the mirror at an unapproved registry.

Run `terraform init -lockfile=readonly` (or the equivalent Terragrunt command)
against a representative module. Confirm that the request URL contains the
mirror host, the selected provider versions match the lock file, and the mirror
logs show the expected provider metadata and archive requests. Repeat the
command after deleting only the runner's local `.terraform` directory to prove
that the mirror PVC supplies the cached archive.

## Cutover and rollback

Keep the HermitCrab configuration available during the validation window. Cut
over only after a locked initialization succeeds for each required provider and
the mirror readiness and TLS checks pass. To roll back, restore the previous
`TF_CLI_CONFIG_FILE` and leave the mirror cache untouched for investigation.
