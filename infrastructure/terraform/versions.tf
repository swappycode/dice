terraform {
  required_version = ">= 1.5"
  required_providers {
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 2.30"
    }
    helm = {
      source  = "hashicorp/helm"
      version = "~> 2.13"
    }
  }
  # Wire a real backend before any shared use (S3/GCS/azurerm + state locking).
  # backend "s3" { bucket = "dice-tfstate" key = "dice/terraform.tfstate" region = "us-east-1" }
}
