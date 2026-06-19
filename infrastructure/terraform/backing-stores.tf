# In-cluster NATS (JetStream) + Redis via Helm. Postgres is intentionally NOT run
# in-cluster here — use a managed Postgres (RDS/Cloud SQL/Crunchy) or the
# CloudNativePG operator and pass its DSN via var.database_url. Swap these for
# managed equivalents in production where you want them.

resource "helm_release" "nats" {
  name       = "dice-nats"
  namespace  = kubernetes_namespace.dice.metadata[0].name
  repository = "https://nats-io.github.io/k8s/helm/charts/"
  chart      = "nats"
  version    = "1.2.2"

  # JetStream on (the durable DICE_EVT stream / outbox backstop needs it).
  set {
    name  = "config.jetstream.enabled"
    value = "true"
  }
  set {
    name  = "config.cluster.enabled"
    value = "true"
  }
  set {
    name  = "config.cluster.replicas"
    value = "3"
  }
}

resource "helm_release" "redis" {
  name       = "dice-redis"
  namespace  = kubernetes_namespace.dice.metadata[0].name
  repository = "https://charts.bitnami.com/bitnami"
  chart      = "redis"
  version    = "20.0.3"

  # Presence + the cross-node resume session directory/snapshot live here; a
  # replicated setup keeps them available across a node loss.
  set {
    name  = "architecture"
    value = "replication"
  }
  set {
    name  = "auth.enabled"
    value = "false" # cluster-internal; enable + wire DICE_REDIS_URL creds for prod
  }
}
