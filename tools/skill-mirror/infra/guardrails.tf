###############################################################################
# TASK-700 — Cost guardrails for the PUBLIC skill mirror
#
# The mirror is public by explicit operator decision. That reframes the threat
# model: the failure mode is not unauthorized access, it is an unbounded bill.
# This file provides two of the three layers:
#
#   1. CACHE  (the real control)    — response headers policies, below
#   2. RATE LIMIT (the backstop)    — WAFv2 rate-based rules, below
#   3. BUDGET ALARM (last resort)   — budget.tf
#
# Explicitly NOT in scope and deliberately absent: OAuth, JWT, API keys, client
# attestation. A public HTTP endpoint cannot be restricted to "only aish
# clients" — any secret shipped in the binary is extractable — and we are not
# going to pretend otherwise.
#
# OWNERSHIP: this file is owned by TASK-700. TASK-696 owns main.tf,
# providers.tf, s3.tf, cloudfront.tf, route53.tf, acm.tf, variables.tf and
# outputs.tf. TASK-697 owns search_function.tf and variables_search.tf. Nothing
# in this file references a resource declared in those files; the two seams that
# must cross files are passed as variables or documented as one-line follow-ups
# (see GUARDRAILS.md "Follow-up wiring").
#
# PROVIDER: WAFv2 resources scoped to CLOUDFRONT must be created in us-east-1,
# so every WAF resource here uses the `aws.us_east_1` provider alias declared by
# TASK-696 in providers.tf.
###############################################################################

###############################################################################
# 1. CACHE — the control that actually does the work
#
# With s-maxage=86400 on raw objects, a million requests for the same skill is
# ONE origin read. The cache is not a performance nicety here, it is the cost
# control; the rate limit below is only the backstop for traffic that misses
# cache by construction (random-path scanning).
#
# These are response headers policies, which set cache-control on the way OUT.
# CloudFront's own retention is governed by the cache policies in TASK-696's
# cloudfront.tf; these headers govern what the CLIENT and any intermediary
# proxy do, and make the contract legible in a plain `curl -I`.
#
# ASSOCIATION IS A FOLLOW-UP: attaching a response headers policy to a cache
# behaviour is an argument on aws_cloudfront_distribution, which lives in
# TASK-696's cloudfront.tf. This file must not edit that file, so the policies
# are declared here and the one-line association per behaviour is documented in
# GUARDRAILS.md and called out in the PR body.
###############################################################################

# Path class: /{owner}/{name}/raw  — protects against repeated skill downloads.
resource "aws_cloudfront_response_headers_policy" "guardrail_raw" {
  name    = "${var.guardrails_name_prefix}-guardrail-raw"
  comment = "TASK-700: long-lived edge cache for immutable-ish raw SKILL.md objects"

  custom_headers_config {
    items {
      header   = "cache-control"
      value    = "public, max-age=3600, s-maxage=86400, stale-while-revalidate=604800"
      override = true
    }
  }

  # Defensive: a 429 carrying `x-vercel-mitigated: challenge` is special-cased
  # by the aish client (is_vercel_challenge, src/skill_provider.rs) into a
  # bot-challenge error message. Nothing in this stack sets that header, but
  # stripping it at the edge means no future origin change can accidentally
  # make our own rate limit masquerade as skill.fish's Vercel challenge.
  remove_headers_config {
    items {
      header = "x-vercel-mitigated"
    }
  }
}

# Path class: /index.json — protects against catalog polling.
#
# TTL/freshness trade: 300s bounds how quickly a publish becomes visible to
# clients. Accepted and documented (see GUARDRAILS.md).
resource "aws_cloudfront_response_headers_policy" "guardrail_index" {
  name    = "${var.guardrails_name_prefix}-guardrail-index"
  comment = "TASK-700: 300s edge cache for the catalog index"

  custom_headers_config {
    items {
      header   = "cache-control"
      value    = "public, max-age=300, s-maxage=300"
      override = true
    }
  }

  remove_headers_config {
    items {
      header = "x-vercel-mitigated"
    }
  }
}

# Path class: /api/v1/search* — protects against repeated identical queries.
#
# Short TTL because search results change with every publish, but even 60s
# collapses the tight-loop case. The cache POLICY (TASK-697) must key on the
# FULL query string so distinct `q` values cache independently; this policy only
# sets the outbound header.
resource "aws_cloudfront_response_headers_policy" "guardrail_search" {
  name    = "${var.guardrails_name_prefix}-guardrail-search"
  comment = "TASK-700: 60s edge cache for search responses, keyed on full query string"

  custom_headers_config {
    items {
      header   = "cache-control"
      value    = "public, max-age=60"
      override = true
    }
  }

  remove_headers_config {
    items {
      header = "x-vercel-mitigated"
    }
  }
}

###############################################################################
# 2. RATE LIMIT — the backstop
#
# Two tiers, because the expensive path is search. /index.json and the raw
# objects are cache-dominated (see above), so they get a deliberately loose
# catch-all limit; search gets the tight tier.
#
# Rule ordering matters: the search rule is evaluated FIRST (lower priority
# number). Its scope-down statement narrows it to the search URI prefix, so a
# search request is counted by the tight rule and — if it passes — then also
# counted by the loose catch-all. A raw/index request only ever hits the
# catch-all.
#
# THE 429 CONTRACT — read before editing the custom response:
#   Breach returns HTTP 429 plus `retry-after: 60`, and MUST NOT set
#   `x-vercel-mitigated`. `is_vercel_challenge` in src/skill_provider.rs matches
#   on 429 AND `x-vercel-mitigated: challenge` together, and turns that pairing
#   into aish's "bot challenge" error message with skill.fish-specific
#   guidance. Our own rate limit must surface as a PLAIN HTTP 429, which the
#   client already reports cleanly. Adding that header here would make our
#   mirror impersonate the exact failure it exists to work around.
###############################################################################

resource "aws_wafv2_web_acl" "mirror" {
  provider = aws.us_east_1

  name        = "${var.guardrails_name_prefix}-guardrails"
  description = "TASK-700: per-IP rate limiting for the public aish skill mirror"
  scope       = "CLOUDFRONT"

  # Public mirror: allow by default, block only what trips a rate rule.
  default_action {
    allow {}
  }

  # Shared 429 body for both rate tiers. JSON rather than HTML because every
  # consumer of this endpoint is a programmatic client.
  custom_response_body {
    key          = "rate_limited"
    content_type = "APPLICATION_JSON"
    content = jsonencode({
      error       = "rate_limited"
      message     = "Too many requests from this IP. Retry after ${var.rate_limit_retry_after_sec}s."
      retry_after = var.rate_limit_retry_after_sec
    })
  }

  ###########################################################################
  # Tight tier — SEARCH ONLY.
  #
  # limit is requests per EVALUATION WINDOW, which is pinned to 300s below.
  #   60 req/min * 5 min = 300 requests per 5-minute window  <-- the default.
  # It is NOT 60. Setting it to 60 would mean 12 req/min and would throttle
  # ordinary interactive `:skill search` use. See the variable description.
  ###########################################################################
  rule {
    name     = "search-rate-limit"
    priority = 10

    action {
      block {
        custom_response {
          response_code            = 429
          custom_response_body_key = "rate_limited"

          response_header {
            name  = "retry-after"
            value = tostring(var.rate_limit_retry_after_sec)
          }
          # NO x-vercel-mitigated header here. See "THE 429 CONTRACT" above.
        }
      }
    }

    statement {
      rate_based_statement {
        limit                 = var.search_rate_limit_per_5min
        evaluation_window_sec = var.waf_evaluation_window_sec
        aggregate_key_type    = "IP"

        # Scope-down: only count requests whose URI path begins with the search
        # prefix. This is what makes the tier tight-on-search and silent on the
        # cache-dominated paths.
        scope_down_statement {
          byte_match_statement {
            positional_constraint = "STARTS_WITH"
            search_string         = "/api/v1/search"

            field_to_match {
              uri_path {}
            }

            text_transformation {
              priority = 0
              type     = "LOWERCASE"
            }
          }
        }
      }
    }

    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "${var.guardrails_name_prefix}-search-rate-limit"
      sampled_requests_enabled   = true
    }
  }

  ###########################################################################
  # Loose catch-all tier — everything else (/index.json, /{owner}/{name}/raw).
  #
  # Deliberately an order of magnitude above the search tier. Per the card's
  # layering rationale, cache-first means legitimate heavy users (a CI fleet
  # pulling the same skills) are NEVER throttled; this rule exists only to cap
  # absurd random-path scanning that misses cache by construction.
  ###########################################################################
  rule {
    name     = "global-rate-limit"
    priority = 20

    action {
      block {
        custom_response {
          response_code            = 429
          custom_response_body_key = "rate_limited"

          response_header {
            name  = "retry-after"
            value = tostring(var.rate_limit_retry_after_sec)
          }
          # NO x-vercel-mitigated header here either.
        }
      }
    }

    statement {
      rate_based_statement {
        limit                 = var.global_rate_limit_per_5min
        evaluation_window_sec = var.waf_evaluation_window_sec
        aggregate_key_type    = "IP"
      }
    }

    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "${var.guardrails_name_prefix}-global-rate-limit"
      sampled_requests_enabled   = true
    }
  }

  visibility_config {
    cloudwatch_metrics_enabled = true
    metric_name                = "${var.guardrails_name_prefix}-guardrails"
    sampled_requests_enabled   = true
  }

  tags = var.guardrails_tags
}

###############################################################################
# Outputs for the follow-up wiring.
#
# These live here (not in TASK-696's outputs.tf) so this file stays
# self-contained. TASK-696's distribution needs:
#     web_acl_id = aws_wafv2_web_acl.mirror.arn
# A CLOUDFRONT-scoped web ACL is attached by ARN, not by id, despite the
# argument being named web_acl_id — that mismatch has bitten people before.
###############################################################################

output "guardrails_web_acl_arn" {
  description = "ARN of the CLOUDFRONT-scoped WAFv2 web ACL. Assign to the distribution's web_acl_id argument (yes, ARN into an argument named *_id — that is the documented AWS behaviour for CLOUDFRONT scope)."
  value       = aws_wafv2_web_acl.mirror.arn
}

output "guardrails_response_headers_policy_ids" {
  description = "Response headers policy ids to attach per cache behaviour in TASK-696's cloudfront.tf: raw -> the /{owner}/{name}/raw behaviour, index -> the /index.json behaviour, search -> the /api/v1/search* behaviour."
  value = {
    raw    = aws_cloudfront_response_headers_policy.guardrail_raw.id
    index  = aws_cloudfront_response_headers_policy.guardrail_index.id
    search = aws_cloudfront_response_headers_policy.guardrail_search.id
  }
}
