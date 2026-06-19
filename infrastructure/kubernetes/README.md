# Kubernetes — Dice split fleet (M4)

Real manifests for running Dice as a horizontally-scaled, multi-node deployment:
the **gateway** (QUIC + WSS) as a `StatefulSet`, the **auth / chat / presence**
service bins as `StatefulSet`s answering over NATS RPC, plus the Services, LB
strategy, autoscaling, and config/secret wiring.

> These manifests are written against the running system's real env contract
> (`DICE_*`) but are **not cluster-verified in this repo** (no cluster/tooling in
> CI). Treat them as the deployment reference — like the 100k benchmark harness,
> built here and run on real infrastructure.

## What's here

| File | What |
|---|---|
| `namespace.yaml` | the `dice` namespace |
| `config.yaml` | `dice-config` ConfigMap (non-secret env) + **Secret templates** (`dice-tls`, `dice-jwt`, `dice-db`) |
| `gateway.yaml` | gateway `StatefulSet` + headless Service + WSS/QUIC LoadBalancers + HPA + PodDisruptionBudget |
| `services.yaml` | auth / chat / presence `StatefulSet`s + headless Services |
| `kustomization.yaml` | ties it together; pins the image tag |

## Prerequisites

1. **A Kubernetes cluster** (1.26+ for mixed-protocol LB support, if you merge the
   WSS/QUIC Services).
2. **The image**, built + pushed:
   ```sh
   docker build -f infrastructure/docker/Dockerfile -t ghcr.io/swappycode/dice:latest .
   docker push ghcr.io/swappycode/dice:latest
   ```
3. **Backing stores** reachable from the cluster — **Postgres**, **NATS**
   (JetStream), **Redis** — as managed services or in-cluster operators
   (CloudNativePG, the NATS Helm chart, the Redis/Bitnami chart). Point the URLs
   in `config.yaml` + `dice-db` at them. Run the DB migrations once before first
   boot (the dev monolith auto-migrates; in prod run them as a `Job`).
4. **Real secrets** — replace the templates in `config.yaml`:
   - `dice-tls` — the server cert/key (provision via **cert-manager**).
   - `dice-jwt` — the Ed25519 signing pair (the gateway signs, auth verifies the
     **same** pair). Store via **sealed-secrets** / **external-secrets**.
   - `dice-db` — `DATABASE_URL`.

## Deploy

```sh
kubectl apply -k infrastructure/kubernetes/
kubectl -n dice rollout status statefulset/dice-gateway
```

Point clients at the `dice-gateway-wss` LoadBalancer (TCP 8443) + `dice-gateway-quic`
(UDP 8444) external addresses.

## Per-pod identity (why StatefulSets)

Every bin generates snowflake ids and so needs a **distinct `DICE_NODE_ID`**
(0–1023). A `StatefulSet` gives each pod a stable ordinal (`dice-gateway-0`, …),
and the container entrypoint computes `DICE_NODE_ID = NODE_ID_BASE + ordinal`:

| Tier | `NODE_ID_BASE` | id range |
|---|---|---|
| gateway | 0 | 0–255 |
| auth | 256 | 256–511 |
| chat | 512 | 512–767 |
| presence | 768 | 768–1023 |

The gateway additionally derives **`DICE_ADVERTISED_ADDR`** =
`<pod>.dice-gateway-headless.dice.svc.cluster.local:8443` — its stable, reachable
address. **Cross-node resume** (ADR-0007) records this in the shared session
directory so a reconnect landing on another node is redirected to the owner
(phase 0b), and a node's *death* is recovered by re-hosting the session from its
durable Redis snapshot on another node (phase 2b).

## LB strategy: QUIC (UDP) + WSS (TCP)

The gateway terminates TLS itself, so the client-facing Services are **L4
pass-through LoadBalancers**, not an HTTP Ingress:

- **`dice-gateway-wss`** — TCP 8443. `sessionAffinity: ClientIP` routes a
  reconnect back to its owning node within the resume window (phase-0 affinity).
  With phase 0b/2b this is an optimization, not a correctness requirement.
- **`dice-gateway-quic`** — UDP 8444. QUIC connection-ids survive a UDP-LB rehash;
  a reshuffle to a different node is absorbed by cross-node resume re-host. Kept a
  separate Service so it works on every LB implementation (merge with the WSS
  Service only on 1.26+ with the `MixedProtocolLBService` feature).

`externalTrafficPolicy: Local` on both preserves the real client source IP — the
per-IP rate limiter (`{scope}:{ip}`) and QUIC path validation depend on it.

## Scaling

`kubectl -n dice scale statefulset/dice-gateway --replicas=N`, or let the HPA do
it (CPU target 70%, 3–32 pods, a 5-min scale-down stabilization so nodes holding
detached sessions aren't shed hastily). One pod sustains ~40k connections at the
2 Gi limit (~44 KB/conn measured), so 32 pods ≈ 1.3M connections.

## Observability

Each pod serves `/metrics` on `:9600`. Scrape via a Prometheus `PodMonitor`
selecting `app.kubernetes.io/part-of: dice`; traces go to the OTLP collector in
`DICE_OTLP_ENDPOINT`. (The `infrastructure/docker/observability.yml` stack is the
local equivalent.)
