# DNS-validated ACM certificate for the mirror hostname.
#
# MUST be issued in us-east-1: CloudFront reads certificates only from that
# region. Hence the `provider = aws.us_east_1` on both resources below, while
# the validation CNAMEs themselves are written into the (region-agnostic)
# Route53 hosted zone.
#
# TIMING CAVEAT: first issuance plus DNS propagation can take up to ~24h in the
# worst case. Do not put the initial apply on a demo critical path. See README.

resource "aws_acm_certificate" "mirror" {
  provider = aws.us_east_1

  domain_name       = var.mirror_hostname
  validation_method = "DNS"

  lifecycle {
    create_before_destroy = true
  }
}

# One validation record per domain validation option. `for_each` over the set
# keeps this correct if a SAN is ever added to the cert.
resource "aws_route53_record" "acm_validation" {
  for_each = {
    for dvo in aws_acm_certificate.mirror.domain_validation_options : dvo.domain_name => {
      name   = dvo.resource_record_name
      record = dvo.resource_record_value
      type   = dvo.resource_record_type
    }
  }

  zone_id         = data.aws_route53_zone.mirror.zone_id
  name            = each.value.name
  type            = each.value.type
  records         = [each.value.record]
  ttl             = 60
  allow_overwrite = true
}

resource "aws_acm_certificate_validation" "mirror" {
  provider = aws.us_east_1

  certificate_arn         = aws_acm_certificate.mirror.arn
  validation_record_fqdns = [for r in aws_route53_record.acm_validation : r.fqdn]
}
