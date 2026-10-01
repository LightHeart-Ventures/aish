# Cost guardrails for the public skill mirror

**Card:** TASK-700 (SPR-100) · **Status:** implemented, pending the TASK-696 module landing

The aish skill mirror at `https://skills.aish.sh` is **public by explicit
operator decision**. That reframes the threat model entirely: the failure mode
is not unauthorized access, it is an **unbounded bill**.

This document describes the three controls that address that, why they are
layered in this specific order, and what each one deliberately does *not* do.

## Explicitly not in scope

No OAuth, no JWT, no API keys, no client attestation.

A public HTTP endpoint cannot be restricted to "only aish clients". Any secret
shipped in a distributed binary is extractable, and a scheme that pretends
otherwise buys a false sense of control while adding a real maintenance burden
and a real outage mode. We are not going to pretend otherwise. The controls
below are the ones that actually bound the cost.

## Control matrix

| Control | Where | Setting | Protects against |
|---|---|---|---|
| Edge cache on `raw` | CloudFront | `public, max-age=3600, s-maxage=86400, stale-while-revalidate=604800` | repeated skill downloads |
| Edge cache on `index.json` | CloudFront | `public, max-age=300, s-maxage=300` | catalog polling |
| Edge cache on search | CloudFront | `public, max-age=60`, keyed on full query string | repeated identical queries |
| Per-IP rate limit (search) | WAFv2 rate-based rule | 300 req / 5 min = **60 req/min**, `429` + `retry-after: 60` | runaway scripts |
| Per-IP rate limit (catch-all) | WAFv2 rate-based rule | 3000 req / 5 min = 600 req/min | absurd random-path scanning |
| `limit` clamp | search function (TASK-697) | `[1,100]` | response-size amplification |
| Response size cap | generator (TASK-694) | 256 KiB per `SKILL.md` | oversized objects |
| Budget alarm | AWS Budgets | 50 / 80 / 100% of $20/mo | everything else |
| Egress alarm | CloudWatch `BytesDownloaded` | 50 GB / 6h | same-day signal, faster than billing |

The `limit` clamp and the response size cap are listed for completeness — they
are owned by TASK-697 and TASK-694 respectively, not by this module.

## Layering rationale

The ordering of these controls is a deliberate design decision, not an
accident of implementation.

**The cache is the real control.** With `s-maxage=86400` on raw objects, a
million requests for the same skill is *one* origin read. Essentially all of
the cost exposure of a public mirror is eliminated at this layer, and it is
eliminated without touching a single legitimate request.

**The rate limit is the backstop.** It exists for the pathological case that
misses cache by construction — random-path scanning, a tight loop against
distinct queries. It is not the primary control and should not be tuned as
though it were.

**The budget alarm exists because no control is perfect.** It is the detective
layer that catches the thing we did not anticipate. The CloudWatch egress alarm
sits alongside it because AWS Budgets can lag real spend by up to ~24h, and a
day is a long time for a runaway egress bill.

### Why cache-first matters, concretely

This ordering has a direct consequence worth stating plainly:

> **Cache-first means legitimate heavy users are never throttled.**

A CI fleet pulling the same twenty skills on every build is, in request-count
terms, one of the heaviest consumers this mirror will ever have. It is also
almost free to serve, because every one of those requests is a cache hit. Under
a cache-first design that fleet never trips anything.

The rate limit, by contrast, **only bites traffic that by definition reuses
nothing**. If you are being throttled, you are issuing requests that each miss
cache — which is the signature of scanning, not of use. That is the property
that lets us set a rate limit at all without worrying about punishing the
users we most want to serve.

### Tiering: the expensive path is search

The two rate tiers follow directly from the above. `/index.json` and
`/{owner}/{name}/raw` are cache-dominated, so throttling them buys nothing and
risks exactly the CI-fleet false positive described above; they get the loose
catch-all tier (600 req/min). `/api/v1/search*` invokes a Lambda@Edge function
per cache miss and is therefore the only path with real marginal cost; it gets
the tight tier (60 req/min), expressed as a WAFv2 scope-down statement on the
URI path.

## The 60/min arithmetic — read this before changing the limit

The product decision is **60 requests per minute sustained**. The Terraform
default is **300**. Those are the same number:

```
WAFv2 rate-based rules count requests over an EVALUATION WINDOW.
This module pins that window to 300 seconds (var.waf_evaluation_window_sec).

    60 req/min  x  5 min  =  300 requests per 5-minute window
```

Setting `search_rate_limit_per_5min = 60` would mean **12 req/min** and would
throttle ordinary interactive use. The window is set explicitly in the
Terraform rather than relying on the provider default, precisely so this
arithmetic is visible in the code and not folded into an implicit default.

**Burst behaviour.** Because WAF counts over the whole window rather than
running a per-second token bucket, a client can spend its entire 300-request
budget in the first seconds of a window and then be blocked for the remainder.
That is acceptable and arguably desirable: the caller this rule exists for is a
tight loop, and a human running `:skill search` a dozen times never approaches
300.

## The `x-vercel-mitigated` constraint

**Our 429 must never carry an `x-vercel-mitigated` header.** This is not
cosmetic.

`is_vercel_challenge` in `src/skill_provider.rs` matches on HTTP 429 **and**
`x-vercel-mitigated: challenge` together, and converts that pairing into aish's
bot-challenge error message — the one that tells the user skill.fish is behind
Vercel's bot protection and suggests workarounds, *including pointing them at
this mirror*.

If our own rate limit emitted that header, a throttled user would be told that
the mirror is blocked by a bot challenge and advised to switch to the mirror
they are already using. A plain HTTP 429 is already reported cleanly by the
client, so the correct behaviour is simply not to set the header.

This is enforced in three places:

1. The WAF custom responses in `guardrails.tf` set only `retry-after`, with a
   comment explaining why nothing else may be added.
2. All three response headers policies carry a `remove_headers_config` that
   strips `x-vercel-mitigated`, so no future origin change can reintroduce it.
3. `scripts/guardrail-smoke.sh` check 2 asserts its absence on an actual 429.

## Known risks and accepted trade-offs

### NAT'd corporate networks

A per-IP rate limit is, by construction, a per-*egress-IP* limit. Everyone
behind a single corporate NAT shares one budget. This is the standard and
well-known downside of IP-based rate limiting and it is accepted here.

The reason it is acceptable: 60 req/min is generous for the actual workload.
Interactive `:skill search` is a human-paced operation, and the bulk-fetch path
(`:skill add`) hits the cache-dominated raw objects governed by the loose
catch-all tier at 600 req/min. For a shared office IP to trip the search tier,
dozens of people would have to be searching simultaneously and continuously.

**Revisit only with evidence.** If a real user reports a 429 from a shared
network, raise `search_rate_limit_per_5min` — do not pre-emptively loosen it on
the strength of the theoretical concern alone, because every increase directly
widens the window the control exists to close.

### Publish visibility is bounded by the index TTL

`s-maxage=300` on `/index.json` means a newly published skill can take **up to
5 minutes** to become visible to clients, and CloudFront edges may disagree with
each other during that window.

This is accepted and deliberate: the catalog-polling cost it prevents is real
and continuous, whereas the staleness it introduces is bounded and only matters
immediately after a publish. If a publish must be visible instantly, issue a
CloudFront invalidation for `/index.json` as part of the publish workflow
(TASK-698) — the raw objects do not need invalidating because their keys change
with content.

### The budget is scoped by service, not by tag

`aws_budgets_budget` *can* filter on `TagKeyValue`, but only for tags activated
as **cost allocation tags** in the Billing console — an account-level, manual,
eventually-consistent step that Terraform cannot reliably drive for a new tag
and which does not backfill historical cost. A budget wired to a not-yet-active
tag silently matches $0 of spend, which is the worst failure mode available to
a cost alarm: **it looks healthy precisely because it is measuring nothing.**

So the budget is scoped to the service set (S3 + CloudFront + Lambda) instead.
The trade-off is explicit: if this AWS account hosts other S3/CloudFront/Lambda
workloads, their spend counts against the $20 ceiling and the alarm
over-reports. Since the ceiling is a trip-wire rather than a forecast,
over-reporting is the correct direction to fail.

To tighten later: activate the `Project` cost allocation tag, wait for it to
backfill, then swap the `cost_filter` block in `budget.tf`.

## Budget decision summary

| Parameter | Value | Variable |
|---|---|---|
| Monthly ceiling | **$20 USD** | `var.monthly_budget_usd` |
| ACTUAL thresholds | 50% / 80% / 100% | — |
| FORECASTED thresholds | 80% / 100% | — |
| Recipient | **gregory@hohertz.com** | `var.budget_notification_email` |

Both the ceiling and the recipient are **parameterized**: handing the mirror to
a different owner, or raising the ceiling, is a `tfvars` change rather than a
code edit.

$20 is a deliberately low trip-wire, not a forecast. The expected steady-state
cost of this design is a couple of dollars a month, so *any* material spend is
itself the signal — which is the whole reason a 50% ACTUAL notification is
useful here rather than merely noisy.

FORECASTED at 50% is deliberately **omitted**: early in a month AWS's forecast
is noisy enough that it would cry wolf most months, and an alarm nobody trusts
is worse than no alarm at all.

## Follow-up wiring

This file's resources are deliberately free of references to resources declared
in TASK-696's or TASK-697's files, so the three branches can merge in any order.
Three one-line seams must be closed **after the TASK-696 PR lands**:

1. **Attach the web ACL** — in TASK-696's `cloudfront.tf`, on the
   `aws_cloudfront_distribution` resource:

   ```hcl
   web_acl_id = aws_wafv2_web_acl.mirror.arn
   ```

   Note the ARN goes into an argument named `web_acl_id`. For a
   `CLOUDFRONT`-scoped web ACL that is the documented AWS behaviour, and
   passing the id instead fails at apply time.

2. **Attach the response headers policies** — on each cache behaviour in
   TASK-696's `cloudfront.tf`:

   ```hcl
   response_headers_policy_id = aws_cloudfront_response_headers_policy.guardrail_raw.id    # /{owner}/{name}/raw
   response_headers_policy_id = aws_cloudfront_response_headers_policy.guardrail_index.id  # /index.json
   response_headers_policy_id = aws_cloudfront_response_headers_policy.guardrail_search.id # /api/v1/search*
   ```

3. **Feed the distribution id to the egress alarm** — via tfvars, or by setting
   the variable's value from TASK-696's resource once both are in one module:

   ```hcl
   cloudfront_distribution_id = aws_cloudfront_distribution.<name>.id
   ```

   Leave it empty and the CloudWatch alarm is simply not created; the budget
   and the rate limits are unaffected.

## Verification

`scripts/guardrail-smoke.sh` implements the card's test plan. Checks are
labelled by whether they need a deployed stack:

| Check | Needs deploy? | Asserts |
|---|---|---|
| 1 — cache headers | yes | `cache-control` present on all three path classes |
| 2 — rate limit | yes | >60 req/min from one IP ⇒ `429` + `retry-after`, and **no** `x-vercel-mitigated` |
| 3 — `limit` clamp | yes (HTTP) | `limit=10000` returns at most 100 rows |
| 4 — synthetic budget alert | yes + AWS creds | the notification path delivers end to end |

Check 4 is the one most often skipped and most important to run. An SNS email
subscription starts as `pending confirmation` and a human must click the link
AWS mails them; until that happens the alarm fires into the void. Proving the
alert path **before** it is needed is the entire point.

```sh
# all non-destructive HTTP checks
sh scripts/guardrail-smoke.sh

# include the rate-limit check (sends real traffic; see --help)
sh scripts/guardrail-smoke.sh --include-rate-limit

# fire a synthetic budget/egress alert through SNS
sh scripts/guardrail-smoke.sh --fire-synthetic-alert
```
