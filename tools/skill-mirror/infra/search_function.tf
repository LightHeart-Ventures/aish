# Lambda@Edge search endpoint for the skills mirror — TASK-697.
#
# Owns ONLY this file and variables_search.tf. TASK-696 owns main.tf /
# providers.tf / s3.tf / cloudfront.tf / variables.tf / outputs.tf; TASK-700
# owns guardrails.tf / variables_guardrails.tf. Nothing here edits those.
#
# DEPENDENCIES ON TASK-696 (by name — `terraform validate` fails until it lands):
#   * aws_s3_bucket.catalog              the origin bucket holding index.json
#   * aws_cloudfront_distribution.mirror referenced only in an output, for the
#                                        one-line wiring described at the bottom
#
# REGION: Lambda@Edge functions MUST live in us-east-1. CloudFront + its ACM
# certificate already force the root module to us-east-1, so the default
# provider is used deliberately rather than introducing an alias into a
# providers.tf this task does not own.

# ---------------------------------------------------------------------------
# IAM
# ---------------------------------------------------------------------------

data "aws_iam_policy_document" "search_assume_role" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]

    principals {
      type = "Service"
      # Both are required for Lambda@Edge: `lambda` to create/invoke the
      # function, `edgelambda` for CloudFront to replicate and invoke it.
      identifiers = [
        "lambda.amazonaws.com",
        "edgelambda.amazonaws.com",
      ]
    }
  }
}

resource "aws_iam_role" "search" {
  name               = "${var.search_name_prefix}-edge-search"
  assume_role_policy = data.aws_iam_policy_document.search_assume_role.json

  tags = {
    Name      = "${var.search_name_prefix}-edge-search"
    Component = "skill-mirror-search"
    Task      = "TASK-697"
  }
}

data "aws_iam_policy_document" "search" {
  # Least privilege: read the ONE catalog object, nothing else in the bucket.
  statement {
    sid       = "ReadCatalogIndex"
    effect    = "Allow"
    actions   = ["s3:GetObject"]
    resources = ["${aws_s3_bucket.catalog.arn}/${var.search_index_key}"]
  }

  # Lambda@Edge logs land in the REPLICA region's log group, hence the
  # wildcard region in the resource ARN.
  statement {
    sid    = "CloudWatchLogs"
    effect = "Allow"
    actions = [
      "logs:CreateLogGroup",
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    resources = ["arn:aws:logs:*:*:log-group:/aws/lambda/*.${var.search_name_prefix}-edge-search:*"]
  }
}

resource "aws_iam_role_policy" "search" {
  name   = "${var.search_name_prefix}-edge-search"
  role   = aws_iam_role.search.id
  policy = data.aws_iam_policy_document.search.json
}

# ---------------------------------------------------------------------------
# Function package
#
# Zero npm dependencies: the handler imports @aws-sdk/client-s3, which is part
# of the nodejs20.x managed runtime, so there is nothing to install or bundle.
# Tests and docs are excluded from the deployment artifact.
# ---------------------------------------------------------------------------

data "archive_file" "search" {
  type        = "zip"
  source_dir  = "${path.module}/../edge"
  output_path = "${path.module}/.build/edge-search.zip"

  excludes = [
    "README.md",
    "tests",
    "tests/search.test.mjs",
    "tests/handler.test.mjs",
    "tests/fixtures/catalog.json",
  ]
}

resource "aws_lambda_function" "search" {
  function_name = "${var.search_name_prefix}-edge-search"
  description   = "skills.aish.sh /api/v1/search — field-weighted search over the mirror catalog (TASK-697)"

  role    = aws_iam_role.search.arn
  handler = "handler.handler"
  runtime = "nodejs20.x"

  filename         = data.archive_file.search.output_path
  source_code_hash = data.archive_file.search.output_base64sha256

  memory_size = var.search_lambda_memory_mb
  timeout     = var.search_lambda_timeout_s

  # Lambda@Edge requires a published version; $LATEST cannot be associated
  # with a CloudFront behaviour.
  publish = true

  # Lambda@Edge does NOT support environment variables — the bucket/key/region
  # are baked into the handler's defaults (CATALOG_KEY defaults to
  # "index.json"). The bucket is resolved at runtime from the origin-request
  # event, so there is nothing to inject here. Kept as a comment rather than an
  # empty `environment {}` block, which Terraform would reject for Lambda@Edge.

  tags = {
    Name      = "${var.search_name_prefix}-edge-search"
    Component = "skill-mirror-search"
    Task      = "TASK-697"
  }
}

resource "aws_cloudwatch_log_group" "search" {
  name              = "/aws/lambda/${aws_lambda_function.search.function_name}"
  retention_in_days = var.search_log_retention_days

  tags = {
    Component = "skill-mirror-search"
    Task      = "TASK-697"
  }
}

# ---------------------------------------------------------------------------
# Cache policy — key on the FULL query string so distinct `q` values cache
# independently at the edge. Most searches repeat a small vocabulary, so the
# hit rate should be high and the Lambda invocation count low.
# ---------------------------------------------------------------------------

resource "aws_cloudfront_cache_policy" "search" {
  name        = "${var.search_name_prefix}-search"
  comment     = "Cache /api/v1/search on the full query string (TASK-697)"
  default_ttl = var.search_cache_default_ttl_s
  min_ttl     = var.search_cache_min_ttl_s
  max_ttl     = var.search_cache_max_ttl_s

  parameters_in_cache_key_and_forwarded_to_origin {
    enable_accept_encoding_brotli = true
    enable_accept_encoding_gzip   = true

    query_strings_config {
      # ALL — every distinct ?q=&limit= combination is its own cache entry.
      query_string_behavior = "all"
    }

    headers_config {
      header_behavior = "none"
    }

    cookies_config {
      cookie_behavior = "none"
    }
  }
}

# ---------------------------------------------------------------------------
# The CloudFront behaviour association.
#
# WIRING (one line in TASK-696's cloudfront.tf — this task does not edit that
# file). Inside `resource "aws_cloudfront_distribution" "mirror"`, add:
#
#   dynamic "ordered_cache_behavior" {
#     for_each = [local.search_ordered_cache_behavior]
#     content {
#       path_pattern           = ordered_cache_behavior.value.path_pattern
#       target_origin_id       = ordered_cache_behavior.value.target_origin_id
#       viewer_protocol_policy = ordered_cache_behavior.value.viewer_protocol_policy
#       allowed_methods        = ordered_cache_behavior.value.allowed_methods
#       cached_methods         = ordered_cache_behavior.value.cached_methods
#       compress               = ordered_cache_behavior.value.compress
#       cache_policy_id        = ordered_cache_behavior.value.cache_policy_id
#
#       lambda_function_association {
#         event_type   = ordered_cache_behavior.value.lambda.event_type
#         lambda_arn   = ordered_cache_behavior.value.lambda.lambda_arn
#         include_body = ordered_cache_behavior.value.lambda.include_body
#       }
#     }
#   }
#
# Terraform cannot inject a nested block into a resource declared in another
# file, so the shape is published here as a local + an output and consumed
# there. Keeping it this way is what makes TASK-697 and TASK-696 conflict-free.
# ---------------------------------------------------------------------------

locals {
  search_ordered_cache_behavior = {
    path_pattern           = "/api/v1/search*"
    target_origin_id       = var.search_target_origin_id
    viewer_protocol_policy = "redirect-to-https"
    allowed_methods        = ["GET", "HEAD"]
    cached_methods         = ["GET", "HEAD"]
    compress               = true
    cache_policy_id        = aws_cloudfront_cache_policy.search.id

    lambda = {
      # origin-request: runs only on a cache MISS, so a warm edge cache costs
      # zero Lambda invocations. The handler short-circuits the origin fetch
      # and returns the JSON response directly.
      event_type = "origin-request"
      lambda_arn = aws_lambda_function.search.qualified_arn
      # The request has no body — GET only.
      include_body = false
    }
  }
}

output "search_ordered_cache_behavior" {
  description = "Shape for the /api/v1/search* ordered_cache_behavior — consume from cloudfront.tf (see WIRING comment in search_function.tf)."
  value       = local.search_ordered_cache_behavior
}

output "search_lambda_qualified_arn" {
  description = "Published Lambda@Edge version ARN for the search endpoint."
  value       = aws_lambda_function.search.qualified_arn
}

output "search_cache_policy_id" {
  description = "CloudFront cache policy id keying /api/v1/search* on the full query string."
  value       = aws_cloudfront_cache_policy.search.id
}
