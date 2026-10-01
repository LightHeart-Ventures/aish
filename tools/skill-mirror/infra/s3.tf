# Private origin bucket for the skill mirror catalog.
#
# The bucket is NEVER publicly readable. The only reader is the CloudFront
# distribution, authenticated via Origin Access Control (OAC) and constrained
# by the bucket policy below to that one distribution ARN. There is deliberately
# no website-hosting configuration on this bucket -- see cloudfront.tf for why
# a static-website endpoint would break the extensionless `.../raw` keys.

resource "aws_s3_bucket" "mirror" {
  bucket = var.bucket_name
}

# Belt and braces: block every form of public access at the bucket level, so
# even a mistaken future ACL or policy edit cannot make the catalog public.
resource "aws_s3_bucket_public_access_block" "mirror" {
  bucket = aws_s3_bucket.mirror.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "mirror" {
  bucket = aws_s3_bucket.mirror.id

  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "mirror" {
  bucket = aws_s3_bucket.mirror.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
    bucket_key_enabled = true
  }
}

# Versioning is a recoverability requirement, not a nicety: the nightly publish
# (TASK-698) overwrites index.json and every raw object. If a bad generator run
# ships a truncated catalog, versioning is what lets us restore the previous
# good object without regenerating from source.
resource "aws_s3_bucket_versioning" "mirror" {
  bucket = aws_s3_bucket.mirror.id

  versioning_configuration {
    status = "Enabled"
  }
}

# Keep noncurrent versions bounded so versioning does not grow without limit,
# and clean up the staged publish prefix (see publish.md) after it is flipped.
resource "aws_s3_bucket_lifecycle_configuration" "mirror" {
  bucket = aws_s3_bucket.mirror.id

  depends_on = [aws_s3_bucket_versioning.mirror]

  rule {
    id     = "expire-noncurrent-versions"
    status = "Enabled"

    filter {}

    noncurrent_version_expiration {
      noncurrent_days = 30
    }
  }

  rule {
    id     = "expire-abandoned-staging-prefixes"
    status = "Enabled"

    filter {
      prefix = "_staging/"
    }

    expiration {
      days = 7
    }

    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

# Grant read to the CloudFront distribution ONLY. The AWS:SourceArn condition
# pins this to our specific distribution, so another account's distribution
# cannot use the same OAC service principal to read the bucket.
data "aws_iam_policy_document" "mirror_oac_read" {
  statement {
    sid    = "AllowCloudFrontOACReadOnly"
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["cloudfront.amazonaws.com"]
    }

    actions   = ["s3:GetObject"]
    resources = ["${aws_s3_bucket.mirror.arn}/*"]

    condition {
      test     = "StringEquals"
      variable = "AWS:SourceArn"
      values   = [aws_cloudfront_distribution.mirror.arn]
    }
  }
}

resource "aws_s3_bucket_policy" "mirror" {
  bucket = aws_s3_bucket.mirror.id
  policy = data.aws_iam_policy_document.mirror_oac_read.json

  # Without this the policy can be written before the public-access-block is in
  # place, which AWS rejects on a bucket that is mid-configuration.
  depends_on = [aws_s3_bucket_public_access_block.mirror]
}
