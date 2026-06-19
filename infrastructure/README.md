# infrastructure

Deployment + local-ops assets for Dice.

| Dir | What |
|---|---|
| [`docker/`](docker) | local dev stack (`docker-compose.yml`: Postgres/Redis/NATS) + the opt-in `observability.yml` (Prometheus/Grafana/Tempo), and the multi-stage [`Dockerfile`](docker/Dockerfile) that builds the backend bins |
| [`kubernetes/`](kubernetes) | real manifests for the **multi-node split fleet** — gateway `StatefulSet` (per-pod node id + advertised address for cross-node resume), the service bins, LB strategy for QUIC/WSS, HPA. `kubectl apply -k kubernetes/` |
| [`terraform/`](terraform) | provisions the namespace, secrets, and NATS/Redis backing stores; the app is the kustomize manifests above |

See each dir's README. The k8s + terraform assets are the **deployment reference**
(written against the real `DICE_*` env contract), built here and applied on real
infrastructure — they are not cluster-verified in CI.
