###############################################################################
# TASK-700 — Budget alarm (layer 3 of 3)
#
# The budget alarm exists because no control is perfect. The cache makes the
# steady-state cost trivial and the WAF rate limit caps the pathological
# caller, but both are preventive; this is the detective control that fires
# when something we did not anticipate starts spending money.
#
# Ceiling: $20/month (var.monthly_budget_usd), operator-ratified 2026-09-30.
# That is a deliberately LOW trip-wire, not a forecast — the expected
# steady-state cost of this design is a couple of dollars, so any material
# spend is itself the signal.
#
# Thresholds: 50 / 80 / 100% ACTUAL, plus 80 / 100% FORECASTED.
# FORECASTED at 50% is deliberately omitted: early in a month AWS's forecast is
# noisy enough that a 50% forecast alert would cry wolf most months, and an
# alarm nobody trusts is worse than no alarm.
#
# OWNERSHIP: owned by TASK-700. No references to resources declared in
# TASK-696's or TASK-697's files — the one cross-file value (the CloudFront
# distribution id) arrives via var.cloudfront_distribution_id.
###############################################################################

resource "aws_budgets_budget" "mirror" {
  name         = "${var.guardrails_name_prefix}-monthly"
  budget_type  = "COST"
  time_unit    = "MONTHLY"
  limit_amount = tostring(var.monthly_budget_usd)
  limit_unit   = "USD"

  #############################################################################
  # SCOPING — why SERVICE and not TAG.
  #
  # The honest answer: a tag filter is not dependably expressible here. AWS
  # Budgets *can* filter on TagKeyValue, but only for tags that have been
  # explicitly activated as COST ALLOCATION TAGS in the Billing console — an
  # account-level, manual, eventually-consistent step that Terraform cannot
  # reliably drive for a brand-new tag, and which does not backfill historical
  # cost. A budget wired to a not-yet-active tag silently matches $0 of spend,
  # which is the worst possible failure mode for a cost alarm: it looks healthy
  # precisely because it is measuring nothing.
  #
  # So the budget is scoped to the mirror's SERVICE SET instead
  # (var.budget_cost_filter_services: S3 + CloudFront + Lambda). The trade-off
  # is explicit: if this AWS account ever hosts OTHER S3/CloudFront/Lambda
  # workloads, their spend counts against this $20 ceiling and the alarm will
  # over-report. Given the ceiling is a trip-wire rather than a forecast, an
  # over-reporting alarm is the correct direction to fail.
  #
  # To tighten later: activate the `Project` cost allocation tag in Billing,
  # wait for it to backfill, then swap this block for
  #   cost_filter { name = "TagKeyValue", values = ["user:Project$aish-skill-mirror"] }
  #############################################################################
  cost_filter {
    name   = "Service"
    values = var.budget_cost_filter_services
  }

  # --- ACTUAL spend thresholds -------------------------------------------- #

  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 50
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.budget_notification_email]
  }

  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.budget_notification_email]
  }

  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.budget_notification_email]
  }

  # --- FORECASTED spend thresholds ---------------------------------------- #
  # Forecast gives earlier warning than actual, at the cost of noise. 80% and
  # 100% only; see the header comment on why 50% FORECASTED is omitted.

  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = [var.budget_notification_email]
  }

  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = [var.budget_notification_email]
  }

  tags = var.guardrails_tags
}

###############################################################################
# SNS topic for the CloudWatch egress alarm.
#
# AWS Budgets can email subscribers directly, but CloudWatch alarms cannot —
# they need an SNS topic. This topic is also the hook the smoke test uses to
# fire a SYNTHETIC alert and prove the notification path works before it is
# needed (see scripts/guardrail-smoke.sh, check 4).
#
# MANUAL STEP: an email subscription starts as `pending confirmation`. AWS
# sends a confirmation link to var.budget_notification_email that a human must
# click. Until then the alarm fires into the void. This is the single most
# commonly skipped step in a cost-alarm setup and is exactly what check 4 of
# the smoke test exists to catch.
###############################################################################

resource "aws_sns_topic" "guardrail_alerts" {
  provider = aws.us_east_1

  name         = "${var.guardrails_name_prefix}-guardrail-alerts"
  display_name = "aish skill mirror cost guardrails"

  tags = var.guardrails_tags
}

resource "aws_sns_topic_subscription" "guardrail_alerts_email" {
  provider = aws.us_east_1

  topic_arn = aws_sns_topic.guardrail_alerts.arn
  protocol  = "email"
  endpoint  = var.budget_notification_email
}

###############################################################################
# CloudWatch alarm on CloudFront BytesDownloaded — the faster-than-billing
# signal.
#
# AWS Budgets notifications can lag real spend by up to ~24h, which is a long
# time for a runaway egress bill. CloudFront publishes BytesDownloaded at
# 5-minute granularity, so this alarm turns a cost problem into an operational
# signal on the same day.
#
# PROVIDER + DIMENSIONS: CloudFront metrics are published ONLY to us-east-1 and
# require the literal dimension Region = "Global". Both are easy to get wrong
# and produce an alarm permanently stuck in INSUFFICIENT_DATA — which, like a
# mis-scoped budget, looks healthy while measuring nothing.
###############################################################################

resource "aws_cloudwatch_metric_alarm" "bytes_downloaded" {
  provider = aws.us_east_1

  # Gated so this file remains applyable before TASK-696's distribution exists,
  # and so the alarm is never created pointing at an empty distribution id.
  count = var.enable_bytes_downloaded_alarm && var.cloudfront_distribution_id != "" ? 1 : 0

  alarm_name = "${var.guardrails_name_prefix}-bytes-downloaded"
  alarm_description = join(" ", [
    "CloudFront egress for the public aish skill mirror exceeded",
    "${var.bytes_downloaded_alarm_gb_per_6h} GB in 6 hours.",
    "At first-tier CloudFront pricing that pace would exhaust the",
    "${var.monthly_budget_usd} USD/month ceiling in well under two days.",
    "Check the WAF search-rate-limit metric and the CloudFront cache hit rate:",
    "a spike here with a healthy hit rate means a genuine traffic increase,",
    "whereas a spike with a collapsed hit rate means cache-missing abuse.",
  ])

  namespace   = "AWS/CloudFront"
  metric_name = "BytesDownloaded"
  statistic   = "Sum"

  # 6h of 5-minute datapoints, evaluated as one period.
  period             = 21600
  evaluation_periods = 1

  comparison_operator = "GreaterThanThreshold"
  threshold           = var.bytes_downloaded_alarm_gb_per_6h * 1024 * 1024 * 1024

  # Absence of traffic is not a problem — do not alarm on a quiet mirror.
  treat_missing_data = "notBreaching"

  dimensions = {
    DistributionId = var.cloudfront_distribution_id
    Region         = "Global"
  }

  alarm_actions = [aws_sns_topic.guardrail_alerts.arn]
  ok_actions    = [aws_sns_topic.guardrail_alerts.arn]

  tags = var.guardrails_tags
}

###############################################################################
# Outputs
###############################################################################

output "guardrails_budget_name" {
  description = "Name of the monthly cost budget for the skill mirror."
  value       = aws_budgets_budget.mirror.name
}

output "guardrails_alert_topic_arn" {
  description = "SNS topic ARN used by the egress alarm. Publish to it to fire a synthetic alert (see scripts/guardrail-smoke.sh check 4)."
  value       = aws_sns_topic.guardrail_alerts.arn
}
