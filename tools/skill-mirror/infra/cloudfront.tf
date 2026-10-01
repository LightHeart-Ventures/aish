# CloudFront distribution fronting the private S3 origin.
#
# ---------------------------------------------------------------------------
# CRITICAL INVARIANT 1 -- extensionless `.../raw` keys
# ---------------------------------------------------------------------------
# The client builds the fetch URL as `{base}/{owner}/{name}/raw` (see
# `raw_url_on` in src/skill_provider.rs). The S3 object key is LITERALLY
# `{owner}/{name}/raw` with no extension.
#
# Therefore this distribution deliberately has:
#   * NO `default_root_object`      -- would rewrite `/` semantics
#   * NO S3 website endpoint origin -- index-document conventions would try
#                                      `.../raw/index.html`
#   * NO CloudFront Function / rewrite that appends `.md`, `/index.html`, or
#     any suffix to the path
# The origin is the S3 REST endpoint (via OAC), which maps the request path to
# the object key verbatim. Do not "helpfully" add a rewrite here.
#
# ---------------------------------------------------------------------------
# CRITICAL INVARIANT 2 -- never emit `x-vercel-mitigated`
# ---------------------------------------------------------------------------
# `is_vercel_challenge` in src/skill_provider.rs special-cases an HTTP 429 that
# carries `x-vercel-mitigated: challenge` into a "bot challenge" error with
# Vercel-specific remediation advice. If our own mirror ever emitted that
# header, operators would get a deeply misleading message about a platform we
# do not even use.
#
# CloudFront does not emit this header on its own. The rules are:
#   * never add it to a response_headers_policy custom_headers_config
#   * TASK-700's WAF rate-limit rule must return a 429 WITHOUT it
# scripts/smoke.sh asserts its absence on every response.
#
# ---------------------------------------------------------------------------
# `?version=<v>` handling
# ---------------------------------------------------------------------------
# The client appends `?version=<v>` when a skill ref carries a version. For v1,
# versioned pinning is NOT implemented: the cache policies below set
# query_strings_config = "none", so the query string is excluded from the cache
# key and is not forwarded to S3. The effect is that `.../raw?version=2` serves
# the same object as `.../raw` -- a 200, not a 404. That is the intended v1
# behaviour. See README for the follow-up needed to implement real pinning.

resource "aws_cloudfront_origin_access_control" "mirror" {
  name                              = "${var.bucket_name}-oac"
  description                       = "OAC so only this distribution can read the private skill-mirror bucket"
  origin_access_control_origin_type = "s3"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

# --- Cache policies ---------------------------------------------------------
# One per content class. Both exclude cookies/headers/query-strings from the
# cache key: this is a static object store, so a maximally-shared cache key is
# exactly what we want for hit ratio (and egress cost -- see TASK-700).

resource "aws_cloudfront_cache_policy" "raw" {
  name        = "aish-skill-mirror-raw"
  comment     = "Raw SKILL.md objects: long edge TTL, matches s-maxage=86400"
  default_ttl = 86400
  min_ttl     = 0
  max_ttl     = 604800

  parameters_in_cache_key_and_forwarded_to_origin {
    enable_accept_encoding_brotli = true
    enable_accept_encoding_gzip   = true

    cookies_config {
      cookie_behavior = "none"
    }

    headers_config {
      header_behavior = "none"
    }

    # See "?version=<v> handling" above -- intentionally "none".
    query_strings_config {
      query_string_behavior = "none"
    }
  }
}

resource "aws_cloudfront_cache_policy" "catalog" {
  name        = "aish-skill-mirror-catalog"
  comment     = "index.json catalog: short edge TTL, matches s-maxage=300"
  default_ttl = 300
  min_ttl     = 0
  max_ttl     = 300

  parameters_in_cache_key_and_forwarded_to_origin {
    enable_accept_encoding_brotli = true
    enable_accept_encoding_gzip   = true

    cookies_config {
      cookie_behavior = "none"
    }

    headers_config {
      header_behavior = "none"
    }

    query_strings_config {
      query_string_behavior = "none"
    }
  }
}

# --- Response headers policies ---------------------------------------------
# HEADER OWNERSHIP (documented per the card's requirement):
#
#   cache-control  -> OWNED BY CLOUDFRONT (these policies, override = true).
#                     Rationale: the cache policy can be retuned without
#                     republishing every object in the bucket. The nightly
#                     publish does not need to know the cache strategy.
#
#   content-type   -> PRIMARY owner is S3 object metadata, set at publish time
#                     by TASK-698 (it is per-object truth). These policies set
#                     it with override = true as a SAFETY NET, which is sound
#                     because each cache behaviour maps to exactly one content
#                     class (default = raw/text, /index.json = json).
#
#   etag           -> OWNED BY S3, passed through untouched for conditional
#                     GETs. Nothing here overrides it.

resource "aws_cloudfront_response_headers_policy" "raw" {
  name    = "aish-skill-mirror-raw-headers"
  comment = "text/plain + long-lived cache-control with stale-while-revalidate"

  custom_headers_config {
    items {
      header   = "cache-control"
      value    = "public, max-age=3600, s-maxage=86400, stale-while-revalidate=604800"
      override = true
    }

    # stale-while-revalidate is load-bearing: a failed nightly publish serves
    # slightly stale content instead of taking the mirror down.
    items {
      header   = "content-type"
      value    = "text/plain; charset=utf-8"
      override = true
    }
  }

  security_headers_config {
    strict_transport_security {
      access_control_max_age_sec = 31536000
      include_subdomains         = true
      preload                    = false
      override                   = true
    }

    content_type_options {
      override = true
    }

    frame_options {
      frame_option = "DENY"
      override     = true
    }

    referrer_policy {
      referrer_policy = "no-referrer"
      override        = true
    }
  }
}

resource "aws_cloudfront_response_headers_policy" "catalog" {
  name    = "aish-skill-mirror-catalog-headers"
  comment = "application/json + short-lived cache-control for index.json"

  custom_headers_config {
    items {
      header   = "cache-control"
      value    = "public, max-age=300, s-maxage=300"
      override = true
    }

    items {
      header   = "content-type"
      value    = "application/json"
      override = true
    }
  }

  security_headers_config {
    strict_transport_security {
      access_control_max_age_sec = 31536000
      include_subdomains         = true
      preload                    = false
      override                   = true
    }

    content_type_options {
      override = true
    }

    frame_options {
      frame_option = "DENY"
      override     = true
    }

    referrer_policy {
      referrer_policy = "no-referrer"
      override        = true
    }
  }
}

# --- Distribution -----------------------------------------------------------

resource "aws_cloudfront_distribution" "mirror" {
  enabled         = true
  is_ipv6_enabled = true
  comment         = "aish skill mirror -- static catalog + raw SKILL.md endpoint"
  price_class     = "PriceClass_100"
  aliases         = [var.mirror_hostname]

  # Deliberately empty: see CRITICAL INVARIANT 1 at the top of this file.
  default_root_object = ""

  origin {
    domain_name              = aws_s3_bucket.mirror.bucket_regional_domain_name
    origin_id                = "s3-${aws_s3_bucket.mirror.id}"
    origin_access_control_id = aws_cloudfront_origin_access_control.mirror.id
  }

  # Default behaviour serves `/{owner}/{name}/raw`.
  default_cache_behavior {
    target_origin_id = "s3-${aws_s3_bucket.mirror.id}"

    # The client refuses any non-https origin except loopback
    # (skill_provider::check_url), so http must not merely be allowed -- it is
    # redirected.
    viewer_protocol_policy = "redirect-to-https"

    allowed_methods = ["GET", "HEAD"]
    cached_methods  = ["GET", "HEAD"]
    compress        = true

    cache_policy_id            = aws_cloudfront_cache_policy.raw.id
    response_headers_policy_id = aws_cloudfront_response_headers_policy.raw.id
  }

  # The catalog gets its own content-type and a much shorter TTL.
  ordered_cache_behavior {
    path_pattern     = "/index.json"
    target_origin_id = "s3-${aws_s3_bucket.mirror.id}"

    viewer_protocol_policy = "redirect-to-https"

    allowed_methods = ["GET", "HEAD"]
    cached_methods  = ["GET", "HEAD"]
    compress        = true

    cache_policy_id            = aws_cloudfront_cache_policy.catalog.id
    response_headers_policy_id = aws_cloudfront_response_headers_policy.catalog.id
  }

  viewer_certificate {
    acm_certificate_arn = aws_acm_certificate_validation.mirror.certificate_arn
    ssl_support_method  = "sni-only"

    # TLS 1.2 floor.
    minimum_protocol_version = "TLSv1.2_2021"
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  # NOTE: no `custom_error_response` blocks. An unknown skill must surface S3's
  # 403/404 as a real error -- the card's test plan asserts unknown skill -> 404
  # on raw. Rewriting errors to a friendly HTML page would break that and would
  # also mask a broken publish.
}
