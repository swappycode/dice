# Terraform — Dice infra (M4)

Provisions the **credential-bearing + stateful** layer of a Dice deployment so it
lives in state, not in git: the `dice` namespace, the TLS / JWT / DB **secrets**,
and the in-cluster **NATS** (JetStream) + **Redis** backing stores via Helm. The
stateless workloads (gateway + service `StatefulSet`s, Services, HPA) are the
kustomize manifests in [`../kubernetes`](../kubernetes), applied right after.

> A real skeleton, not cluster-applied in this repo. The division is deliberate:
> Terraform owns secrets + backing stores; kustomize owns the app. The cluster
> itself (EKS/GKE/AKS) is assumed provisioned out-of-band — add a cloud module
> and feed its kubeconfig in via the variables.

## Files

| File | What |
|---|---|
| `versions.tf` | provider + Terraform version constraints (+ a backend stub) |
| `variables.tf` | cluster/image config + the sensitive secret inputs |
| `main.tf` | providers, the `dice` namespace, the `dice-db` / `dice-jwt` / `dice-tls` Secrets |
| `backing-stores.tf` | NATS (JetStream, clustered) + Redis (replication) Helm releases |
| `outputs.tf` | the NATS/Redis URLs to wire into `DICE_*`, and the kustomize next step |
| `terraform.tfvars.example` | copy to `terraform.tfvars` (gitignored) and fill in |

## Use

```sh
cd infrastructure/terraform
cp terraform.tfvars.example terraform.tfvars   # fill in (keep out of VCS)
terraform init
terraform apply

# then the stateless app, with the secrets + stores now in place:
kubectl apply -k ../kubernetes/
```

Set `DICE_NATS_URL` / `DICE_REDIS_URL` in `../kubernetes/config.yaml` to the
`terraform output` values (the defaults there already match these chart names).

## Production notes

- **Postgres** is not run by this module — point `database_url` at a managed
  Postgres (RDS / Cloud SQL / Crunchy) or the CloudNativePG operator. Run the
  migrations as a one-shot `Job` before the first gateway boot.
- **Secrets** — inline PEMs in `tfvars` are fine for a throwaway cluster; for real
  use, source them from Vault / SSM / Secrets Manager data sources, and prefer
  **cert-manager** for TLS over a static cert.
- **State** — wire a remote backend with locking (the `backend` stub in
  `versions.tf`) before any shared use.
- **Multi-region** is a future spike: replicate the stack per region, use a NATS
  supercluster (leaf nodes) for cross-region fan-out, and a geo-aware LB. Out of
  scope here.
