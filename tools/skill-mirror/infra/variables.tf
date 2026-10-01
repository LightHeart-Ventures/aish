# Base variables for the skill-mirror static hosting module (TASK-696).
#
# NAMING NOTE: peers are adding `variables_search.tf` (TASK-697, the search
# function) and `variables_guardrails.tf` (TASK-700, WAF + budget alarm) in
# this SAME directory. Keep new variables for those concerns in THOSE files --
# this file owns only the hosting/DNS/bucket primitives below.

variable "aws_region" {
  description = "AWS region for the origin bucket and regional resources. The ACM certificate is always issued in us-east-1 via the aliased provider, independent of this value."
  type        = string
  default     = "us-east-2"
}

variable "mirror_hostname" {
  description = "Public hostname the mirror is served on. This is the value operators put in AISH_SKILL_REGISTRY."
  type        = string
  default     = "skills.aish.sh"
}

variable "hosted_zone_name" {
  description = "Route53 public hosted zone that owns `mirror_hostname`. Must already exist; this module looks it up rather than creating it."
  type        = string
  default     = "aish.sh"
}

variable "bucket_name" {
  description = "Globally-unique name for the private S3 origin bucket holding the catalog (index.json) and the per-skill raw objects."
  type        = string
  default     = "aish-skill-mirror-origin"
}

variable "tags" {
  description = "Tags applied to every resource in this module via provider default_tags."
  type        = map(string)
  default = {
    Project   = "aish-skill-mirror"
    ManagedBy = "terraform"
    Component = "static-hosting"
    Task      = "TASK-696"
  }
}
