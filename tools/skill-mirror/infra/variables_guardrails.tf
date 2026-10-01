###############################################################################
# TASK-700 — Cost guardrails: input variables
#
# OWNERSHIP NOTE: this file is owned by TASK-700 only. TASK-696 owns
# variables.tf and TASK-697 owns variables_search.tf. Every variable here is
# deliberately prefixed or guardrail-specific so the three files can never
# declare the same name twice (Terraform errors on duplicate variable blocks).
###############################################################################

variable "guardrails_name_prefix" {
  description = <<-EOT
    Name prefix for every guardrail resource (WAF web ACL, budget, SNS topic,
    CloudWatch alarm). Kept separate from TASK-696's naming variables so the two
    files stay independently mergeable.
  EOT
  type        = string
  default     = "aish-skill-mirror"
}

variable "guardrails_tags" {
  description = <<-EOT
    Tags applied to guardrail resources that support tagging. Note that
    aws_budgets_budget and aws_wafv2_web_acl tagging support is partial; see the
    comments in budget.tf about why the budget is scoped by SERVICE rather than
    by tag.
  EOT
  type        = map(string)
  default = {
    Project   = "aish-skill-mirror"
    ManagedBy = "terraform"
    Card      = "TASK-700"
  }
}

###############################################################################
# Rate limiting
###############################################################################

variable "search_rate_limit_per_5min" {
  description = <<-EOT
    WAFv2 rate-based limit for the SEARCH path (/api/v1/search*), expressed as
    requests per EVALUATION WINDOW — not per minute.

    ARITHMETIC (read this before changing the number):
      The product decision is "60 requests per minute sustained".
      WAFv2 rate-based rules count over an evaluation window, which this module
      pins to 300 seconds (5 minutes) via waf_evaluation_window_sec.
      60 req/min * 5 min = 300 requests per 5-minute window.
    So the default of 300 IS the 60/min decision. Dividing it by 5 would give
    12/min and throttle ordinary interactive use.

    Burst tolerance: because WAF counts over the whole window rather than a
    per-second token bucket, a client may spend its entire 300-request budget in
    the first few seconds of a window and then be blocked for the remainder.
    That is acceptable and in fact desirable here — the pathological caller this
    rule exists for is a tight loop, and a short-lived burst from a human
    running `:skill search` a dozen times never approaches 300.
  EOT
  type        = number
  default     = 300

  validation {
    condition     = var.search_rate_limit_per_5min >= 10
    error_message = "WAFv2 rate-based rules require a limit of at least 10."
  }
}

variable "global_rate_limit_per_5min" {
  description = <<-EOT
    A deliberately LOOSE catch-all rate limit applied to every path that is not
    the search endpoint (i.e. /index.json and /{owner}/{name}/raw).

    Per the card's layering rationale, these paths are cache-dominated: with
    s-maxage=86400 on raw objects a million requests for the same skill is one
    origin read, so throttling them buys nothing and would punish exactly the
    legitimate heavy user we want to serve (a CI fleet pulling the same skills).
    This limit therefore exists only to cap absurd random-path scanning, and is
    set an order of magnitude above the search tier.

    Default 3000 per 5-minute window = 600 req/min.
  EOT
  type        = number
  default     = 3000

  validation {
    condition     = var.global_rate_limit_per_5min >= 10
    error_message = "WAFv2 rate-based rules require a limit of at least 10."
  }
}

variable "waf_evaluation_window_sec" {
  description = <<-EOT
    WAFv2 rate-based rule evaluation window in seconds. AWS accepts exactly
    60, 120, 300 or 600. Pinned explicitly to 300 so the "requests per 5 min"
    arithmetic in search_rate_limit_per_5min is stated in code rather than
    relying on the provider default.
  EOT
  type        = number
  default     = 300

  validation {
    condition     = contains([60, 120, 300, 600], var.waf_evaluation_window_sec)
    error_message = "WAFv2 evaluation_window_sec must be one of 60, 120, 300, 600."
  }
}

variable "rate_limit_retry_after_sec" {
  description = <<-EOT
    Value of the `retry-after` response header sent with the 429. Matches the
    one-minute cadence the client-facing contract advertises.
  EOT
  type        = number
  default     = 60
}

###############################################################################
# Budget alarm
###############################################################################

variable "monthly_budget_usd" {
  description = <<-EOT
    Monthly spend ceiling for the skill mirror, in USD. Operator-ratified at $20
    (2026-09-30) as a deliberately LOW trip-wire: the expected steady-state cost
    of this design is a couple of dollars, so any material spend is itself the
    signal. This is not a budget we expect to approach.
  EOT
  type        = number
  default     = 20

  validation {
    condition     = var.monthly_budget_usd > 0
    error_message = "monthly_budget_usd must be greater than zero."
  }
}

variable "budget_notification_email" {
  description = <<-EOT
    Recipient for budget threshold notifications and CloudWatch alarm notices.
    Operator-ratified as gregory@hohertz.com (2026-09-30). Parameterized so
    handing the mirror to a different owner is a tfvars change, not a code edit.
  EOT
  type        = string
  default     = "gregory@hohertz.com"
}

variable "budget_cost_filter_services" {
  description = <<-EOT
    AWS Budgets SERVICE dimension values the budget is scoped to. See the long
    comment in budget.tf: a tag-based filter is NOT reliably expressible here
    because it requires the cost allocation tag to be activated in Billing and
    to have backfilled, so the budget is scoped to the mirror's service set
    instead. These strings are the billing-console display names and must match
    exactly.
  EOT
  type        = list(string)
  default = [
    "Amazon Simple Storage Service",
    "Amazon CloudFront",
    "AWS Lambda",
  ]
}

###############################################################################
# CloudWatch egress alarm (faster-than-billing signal)
###############################################################################

variable "enable_bytes_downloaded_alarm" {
  description = <<-EOT
    Whether to create the CloudWatch alarm on CloudFront BytesDownloaded.
    AWS Budgets notifications can lag real spend by up to ~24h; this alarm is
    the same-day signal. Requires cloudfront_distribution_id to be set.
  EOT
  type        = bool
  default     = true
}

variable "cloudfront_distribution_id" {
  description = <<-EOT
    CloudFront distribution id for the BytesDownloaded alarm.

    WHY A VARIABLE AND NOT A DIRECT RESOURCE REFERENCE: TASK-696 owns
    cloudfront.tf and is being authored in parallel. Referencing its resource
    address directly from this file would couple two in-flight branches and
    break whichever merges second if the resource name differs. Passing the id
    in keeps guardrails.tf / budget.tf / variables_guardrails.tf free of ANY
    cross-file references, which is also what lets them be parsed and validated
    standalone.

    WIRING (one line, see GUARDRAILS.md "Follow-up wiring"):
      cloudfront_distribution_id = aws_cloudfront_distribution.<name>.id
    Leave empty to skip the alarm.
  EOT
  type        = string
  default     = ""
}

variable "bytes_downloaded_alarm_gb_per_6h" {
  description = <<-EOT
    Threshold for the CloudFront BytesDownloaded alarm, in gigabytes summed over
    a 6-hour period. At CloudFront's ~$0.085/GB first-tier egress price, 50 GB
    per 6h is roughly $4.25 per 6h, i.e. a pace that would blow the $20 monthly
    ceiling inside about 30 hours. Alarming there gives same-day warning.
  EOT
  type        = number
  default     = 50
}
