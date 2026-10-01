terraform {
  required_version = ">= 1.5.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.60"
    }
  }

  # REMOTE BACKEND STUB -- intentionally commented out.
  #
  # The team uses a remote backend, but the bucket/table names and credentials
  # are environment-specific and MUST NOT be hardcoded in the repo. Uncomment
  # and fill in from your environment (or pass -backend-config=... at init
  # time) before the first real `terraform apply`.
  #
  # CI runs `terraform init -backend=false` + `terraform validate`, which does
  # not need a backend at all -- that is why this can stay commented without
  # breaking the validation gate.
  #
  # backend "s3" {
  #   bucket         = "<REPLACE-with-your-tfstate-bucket>"
  #   key            = "aish/skill-mirror/terraform.tfstate"
  #   region         = "<REPLACE-with-your-tfstate-region>"
  #   dynamodb_table = "<REPLACE-with-your-tf-lock-table>"
  #   encrypt        = true
  # }
}

provider "aws" {
  region = var.aws_region

  default_tags {
    tags = var.tags
  }
}

# CloudFront only accepts ACM certificates issued in us-east-1, regardless of
# where the rest of the stack lives. This aliased provider exists solely so
# acm.tf can target that region.
provider "aws" {
  alias  = "us_east_1"
  region = "us-east-1"

  default_tags {
    tags = var.tags
  }
}
