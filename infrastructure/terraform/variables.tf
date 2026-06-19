variable "kubeconfig_path" {
  description = "Path to the kubeconfig for the target cluster (provisioned out-of-band: EKS/GKE/AKS module or an existing cluster)."
  type        = string
  default     = "~/.kube/config"
}

variable "kubeconfig_context" {
  description = "kubeconfig context to use."
  type        = string
  default     = null
}

variable "namespace" {
  description = "Namespace for the Dice fleet."
  type        = string
  default     = "dice"
}

variable "image" {
  description = "Fully-qualified Dice backend image (built from infrastructure/docker/Dockerfile)."
  type        = string
  default     = "ghcr.io/swappycode/dice:latest"
}

# --- secrets (do NOT hard-code; pass via a tfvars file kept out of VCS, or an
# external secrets manager data source) ---
variable "database_url" {
  description = "Postgres DSN reachable from the cluster."
  type        = string
  sensitive   = true
}

variable "jwt_private_pem" {
  description = "Ed25519 JWT signing key (PEM). The gateway signs; auth verifies the same pair."
  type        = string
  sensitive   = true
}

variable "jwt_public_pem" {
  description = "Ed25519 JWT public key (PEM)."
  type        = string
  sensitive   = true
}

variable "tls_cert_pem" {
  description = "Server TLS cert chain (PEM). In a real cluster, prefer cert-manager over a static value."
  type        = string
  sensitive   = true
}

variable "tls_key_pem" {
  description = "Server TLS private key (PEM, PKCS#8)."
  type        = string
  sensitive   = true
}
