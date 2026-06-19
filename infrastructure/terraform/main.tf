provider "kubernetes" {
  config_path    = var.kubeconfig_path
  config_context = var.kubeconfig_context
}

provider "helm" {
  kubernetes {
    config_path    = var.kubeconfig_path
    config_context = var.kubeconfig_context
  }
}

resource "kubernetes_namespace" "dice" {
  metadata {
    name   = var.namespace
    labels = { "app.kubernetes.io/part-of" = "dice" }
  }
}

# --- Secrets owned by Terraform (the credential-bearing parts) so they are
# never committed. The stateless workloads (StatefulSets/Services/HPA) are the
# kustomize manifests in ../kubernetes, applied after this (see README). ---
resource "kubernetes_secret" "db" {
  metadata {
    name      = "dice-db"
    namespace = kubernetes_namespace.dice.metadata[0].name
  }
  data = { DATABASE_URL = var.database_url }
}

resource "kubernetes_secret" "jwt" {
  metadata {
    name      = "dice-jwt"
    namespace = kubernetes_namespace.dice.metadata[0].name
  }
  data = {
    "jwt_ed25519.pem"     = var.jwt_private_pem
    "jwt_ed25519.pub.pem" = var.jwt_public_pem
  }
}

resource "kubernetes_secret" "tls" {
  metadata {
    name      = "dice-tls"
    namespace = kubernetes_namespace.dice.metadata[0].name
  }
  type = "kubernetes.io/tls"
  data = {
    "tls.crt" = var.tls_cert_pem
    "tls.key" = var.tls_key_pem
  }
}
