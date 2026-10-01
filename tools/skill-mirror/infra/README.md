# skill-mirror infrastructure (TASK-696)

Terraform for the public, read-only **aish skill mirror**: an S3 origin behind
CloudFront serving `https://skills.aish.sh/{owner}/{name}/raw` and
`https://skills.aish.sh/index.json`.

This is the hosting layer only. The catalog *generator* lives in
`tools/skill-mirror/` (TASK-694), the nightly *publish* workflow is TASK-698,
the *search* endpoint is TASK-697, and the *WAF + budget guardrails* are
TASK-700. Those last two stack on this module via its outputs.

## Why this exists

`skill_provider::check_url` in the aish client **refuses any non-https origin
except loopback**. There is no negotiating that from the server side, so the
mirror must be https with a valid public certificate. Beyond that, everything
except `/api/v1/search` is a plain object read — no compute on the hot path.

Success condition: `AISH_SKILL_REGISTRY=https://skills.aish.sh` makes
`:skill add acme/git-helper` work unmodified.

## What this provisions

| File | Resources |
|---|---|
| `providers.tf` | `aws` provider (default region + aliased `us_east_1` for ACM), pinned `~> 5.60`, commented remote-backend stub |
| `s3.tf` | private origin bucket, public-access block, SSE-S3 encryption, versioning, lifecycle rules, OAC-scoped bucket policy |
| `cloudfront.tf` | OAC, two cache policies, two response-headers policies, the distribution |
| `acm.tf` | DNS-validated certificate in us-east-1 + its validation records |
| `route53.tf` | hosted-zone lookup, A + AAAA alias records |
| `variables.tf` | `aws_region`, `mirror_hostname`, `hosted_zone_name`, `bucket_name`, `tags` |
| `outputs.tf` | distribution id/arn/domain, bucket name/arn, mirror URL |
| `publish.md` | the staged-then-flipped publish contract + required IAM for TASK-698 |
| `scripts/smoke.sh` | POSIX post-deploy assertions |

The bucket is **never public**. CloudFront reads it through an Origin Access
Control, and the bucket policy pins read access to this one distribution's ARN.

## Header / cache matrix

| Path | `content-type` | `cache-control` |
|---|---|---|
| `/{owner}/{name}/raw` | `text/plain; charset=utf-8` | `public, max-age=3600, s-maxage=86400, stale-while-revalidate=604800` |
| `/index.json` | `application/json` | `public, max-age=300, s-maxage=300` |

**Header ownership** (the full table is commented in `cloudfront.tf`):

- `cache-control` — owned by **CloudFront** (response headers policy,
  `override = true`), so cache strategy can be retuned without republishing
  every object.
- `content-type` — primary owner is **S3 object metadata** set at publish time;
  the CloudFront policy overrides it as a safety net, which is sound because
  each cache behaviour maps to exactly one content class.
- `etag` — owned by **S3**, passed through untouched for conditional GETs.

## Two invariants you must not break

1. **Object keys are extensionless `…/raw`.** The client builds
   `{base}/{owner}/{name}/raw`. There is deliberately no `default_root_object`,
   no S3 website endpoint, and no rewrite function — anything that appends
   `.md`, `/index.html`, or any suffix breaks every fetch.

2. **Never emit `x-vercel-mitigated`.** `is_vercel_challenge` in
   `src/skill_provider.rs` special-cases a 429 carrying
   `x-vercel-mitigated: challenge` into a bot-challenge error with
   Vercel-specific advice. Emitting it from our own mirror would produce a
   deeply misleading message about a platform we do not use. `scripts/smoke.sh`
   asserts its absence; TASK-700's WAF rate-limit response must omit it too.

## `?version=<v>`

The client appends `?version=<v>` when a skill ref carries a version. **v1 does
not implement versioned pinning.** Both cache policies set
`query_string_behavior = "none"`, so the query string is excluded from the cache
key and not forwarded — `…/raw?version=2` serves the same object as `…/raw` and
returns **200, not 404**. That is intentional: 404-ing on a version the client
legitimately asked for would be worse than serving latest.

Implementing real pinning later means publishing versioned object keys and
switching these policies to include the query string in the cache key.

## Provider decision: AWS S3 + CloudFront

**The engineering spec on the card recommended Cloudflare R2 + Workers. That
recommendation was overruled by the operator on 2026-09-30**, closing blocking
open question `oq_4ee37b56ca45`. This module implements **AWS**.

The spec's argument for R2 was sound on its own terms: this endpoint is public
and unauthenticated by operator decision, so the failure mode is not a breach,
it is a bill — and R2's zero-egress pricing removes the unbounded-cost tail
structurally rather than alarming on it after the fact.

The operator chose AWS anyway, and the cost tail is handled **compensatingly**
rather than structurally:

- **TASK-700** adds a WAF rate limit (bounding request volume at the edge) plus
  a **$20/mo AWS Budgets alarm** owned by gregory@hohertz.com.
- `PriceClass_100` limits the distribution to the cheapest edge locations.
- Aggressive `s-maxage` (86400 on raw) keeps origin egress near zero; CDN
  cache-hit egress is the dominant cost term and it is cheap.

The tradeoff being accepted: AWS egress is metered where R2's is not, so an
abuse event costs money until the WAF rule throttles it. That is a deliberate
"detect and bound" posture instead of R2's "cannot happen" posture. The
mitigation is real but it is not equivalent — worth revisiting if the mirror
ever sees serious unauthenticated traffic.

What the operator gets in exchange: one cloud account, one IAM story, one set of
Terraform providers the team already knows, and Route53 already holding `aish.sh`.

## Usage

```sh
cd tools/skill-mirror/infra

# CI / validation only -- needs no credentials and no backend.
terraform init -backend=false
terraform validate
terraform fmt -check -recursive

# Real use: uncomment + fill the backend stub in providers.tf first.
terraform init
terraform plan
terraform apply
```

Then smoke-test:

```sh
./scripts/smoke.sh
# or, before DNS/cert propagation completes:
MIRROR="https://$(terraform output -raw distribution_domain_name)" ./scripts/smoke.sh
```

### Prerequisites

- The Route53 public hosted zone for `aish.sh` must already exist. This module
  looks it up (`data.aws_route53_zone`) and never creates or destroys it.
- `bucket_name` must be globally unique across all of S3.

## ⚠️ Propagation caveat — do not put this on a demo critical path

First `apply` issues a **new ACM certificate** and waits on DNS validation.
Certificate issuance plus DNS propagation can take **up to ~24 hours** in the
worst case, and CloudFront distribution deployment itself takes 10–20 minutes on
top of that. `terraform apply` will sit on
`aws_acm_certificate_validation.mirror` for the duration.

Plan the first apply **days** ahead of anything that depends on it. Until the
custom hostname resolves, test against the `distribution_domain_name` output —
it works immediately and serves the same content over https.
