# Outputs consumed by peers stacking on this module:
#   * TASK-697 (search function) needs the distribution id + bucket name
#   * TASK-700 (WAF + budget alarm) needs the distribution id/arn
#   * TASK-698 (nightly publish) needs the bucket name and the distribution id
#     to issue a targeted invalidation after the staged-prefix flip

output "distribution_id" {
  description = "CloudFront distribution ID. Use for cache invalidations and as the WAF association target (TASK-700)."
  value       = aws_cloudfront_distribution.mirror.id
}

output "distribution_arn" {
  description = "CloudFront distribution ARN."
  value       = aws_cloudfront_distribution.mirror.arn
}

output "distribution_domain_name" {
  description = "CloudFront-assigned domain (d111111abcdef8.cloudfront.net). Useful for testing before DNS/cert propagation completes."
  value       = aws_cloudfront_distribution.mirror.domain_name
}

output "bucket_name" {
  description = "Name of the private S3 origin bucket. The nightly publish IAM policy (TASK-698) scopes to this bucket only."
  value       = aws_s3_bucket.mirror.id
}

output "bucket_arn" {
  description = "ARN of the private S3 origin bucket."
  value       = aws_s3_bucket.mirror.arn
}

output "mirror_url" {
  description = "Base URL of the mirror. This is the value for AISH_SKILL_REGISTRY."
  value       = "https://${var.mirror_hostname}"
}

output "raw_url_template" {
  description = "Shape of a raw SKILL.md URL, for documentation and smoke tests."
  value       = "https://${var.mirror_hostname}/{owner}/{name}/raw"
}
