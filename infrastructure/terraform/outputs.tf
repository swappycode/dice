output "namespace" {
  description = "Namespace the fleet is deployed into."
  value       = kubernetes_namespace.dice.metadata[0].name
}

output "nats_url" {
  description = "In-cluster NATS URL to set as DICE_NATS_URL."
  value       = "nats://dice-nats.${kubernetes_namespace.dice.metadata[0].name}.svc.cluster.local:4222"
}

output "redis_url" {
  description = "In-cluster Redis URL to set as DICE_REDIS_URL."
  value       = "redis://dice-redis-master.${kubernetes_namespace.dice.metadata[0].name}.svc.cluster.local:6379"
}

output "next_step" {
  description = "Apply the stateless workloads after Terraform provisions secrets + stores."
  value       = "kubectl apply -k ../kubernetes/"
}
