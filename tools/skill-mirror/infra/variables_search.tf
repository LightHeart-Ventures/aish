# Variables owned by TASK-697 (edge search endpoint).
#
# Deliberately `search_`-prefixed and kept in their OWN file so this task never
# touches TASK-696's variables.tf or TASK-700's variables_guardrails.tf.

variable "search_name_prefix" {
  description = "Name prefix for the search endpoint's Lambda, IAM role and cache policy."
  type        = string
  default     = "aish-skill-mirror"
}

variable "search_index_key" {
  description = "S3 key of the catalog index the search endpoint reads. Must match the generator's output key (TASK-694)."
  type        = string
  default     = "index.json"
}

variable "search_target_origin_id" {
  description = "CloudFront origin id the /api/v1/search* behaviour targets. Must match the catalog-bucket origin declared in TASK-696's cloudfront.tf."
  type        = string
  default     = "catalog-s3"
}

variable "search_lambda_memory_mb" {
  description = "Lambda@Edge memory. 256 MB is ample for a ~1 MB index and a linear scan; Lambda@Edge origin-request caps at 10240 MB."
  type        = number
  default     = 256

  validation {
    condition     = var.search_lambda_memory_mb >= 128 && var.search_lambda_memory_mb <= 10240
    error_message = "search_lambda_memory_mb must be between 128 and 10240."
  }
}

variable "search_lambda_timeout_s" {
  description = "Lambda@Edge timeout in seconds. Origin-request triggers cap at 30s; 5s is far beyond a cold index fetch plus scan."
  type        = number
  default     = 5

  validation {
    condition     = var.search_lambda_timeout_s >= 1 && var.search_lambda_timeout_s <= 30
    error_message = "search_lambda_timeout_s must be between 1 and 30 (the Lambda@Edge origin-request ceiling)."
  }
}

variable "search_log_retention_days" {
  description = "CloudWatch Logs retention for the search function."
  type        = number
  default     = 14
}

variable "search_cache_min_ttl_s" {
  description = "CloudFront minimum TTL for /api/v1/search* responses."
  type        = number
  default     = 0
}

variable "search_cache_default_ttl_s" {
  description = "CloudFront default TTL for /api/v1/search* responses. Matches the handler's cache-control: max-age=60."
  type        = number
  default     = 60
}

variable "search_cache_max_ttl_s" {
  description = "CloudFront maximum TTL for /api/v1/search* responses."
  type        = number
  default     = 300
}
