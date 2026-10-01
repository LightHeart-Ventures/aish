# DNS for the mirror hostname -> CloudFront distribution.
#
# The hosted zone must already exist (it owns `hosted_zone_name`); this module
# looks it up rather than creating it, so it can never accidentally take
# ownership of, or destroy, the apex zone.

data "aws_route53_zone" "mirror" {
  name         = var.hosted_zone_name
  private_zone = false
}

# Alias A record. Alias (not CNAME) so the hostname can be an apex if the
# operator ever points the zone root at the mirror, and so there is no extra
# DNS lookup hop.
resource "aws_route53_record" "mirror_a" {
  zone_id = data.aws_route53_zone.mirror.zone_id
  name    = var.mirror_hostname
  type    = "A"

  alias {
    name                   = aws_cloudfront_distribution.mirror.domain_name
    zone_id                = aws_cloudfront_distribution.mirror.hosted_zone_id
    evaluate_target_health = false
  }
}

# IPv6. The distribution has is_ipv6_enabled = true, so publish the AAAA too --
# otherwise IPv6-only clients silently fall back or fail.
resource "aws_route53_record" "mirror_aaaa" {
  zone_id = data.aws_route53_zone.mirror.zone_id
  name    = var.mirror_hostname
  type    = "AAAA"

  alias {
    name                   = aws_cloudfront_distribution.mirror.domain_name
    zone_id                = aws_cloudfront_distribution.mirror.hosted_zone_id
    evaluate_target_health = false
  }
}
