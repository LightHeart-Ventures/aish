# Publishing to the skill mirror

This documents the contract between the nightly publish workflow (**TASK-698**)
and the infrastructure provisioned here (**TASK-696**). The workflow is not
implemented by this module — this is the interface it must code against.

## Object layout

```
index.json                     # the full catalog (SearchResult[])
{owner}/{name}/raw             # verbatim SKILL.md, one object per skill
```

The raw object key is **literally `…/raw`, with no file extension** — that is
what `raw_url_on` in `src/skill_provider.rs` builds. Nothing in the pipeline may
rewrite it to `raw.md` or `raw/index.html`. See the `CRITICAL INVARIANT 1`
comment block in `cloudfront.tf`.

## Staged-then-flipped publish (required)

The live prefix is **never mutated in place**. A publish that overwrites objects
one at a time leaves the mirror internally inconsistent for the duration of the
upload — `index.json` can advertise a skill whose raw object has not landed yet,
and a mid-publish failure leaves it permanently inconsistent.

The required sequence:

1. **Upload to a staged prefix.** Write the complete new catalog under
   `_staging/<run-id>/` — both `index.json` and every `{owner}/{name}/raw`.
2. **Verify the staged tree.** Assert `index.json` parses, that every entry it
   advertises has a corresponding raw object, and that object count/sizes are
   within sane bounds of the previous publish (a catalog that suddenly shrank by
   90% is a failed generator run, not a real change).
3. **Flip atomically.** Server-side copy the verified staged objects onto the
   live keys (`CopyObject`, no re-upload), `index.json` **last** — so the
   catalog never advertises a skill whose raw object is not already live.
4. **Invalidate.** Issue a CloudFront invalidation for `/index.json` (and
   `/*` only if raw objects changed). The distribution id is the
   `distribution_id` output of this module.
5. **Leave staging alone.** The lifecycle rule in `s3.tf` expires
   `_staging/` after 7 days; do not delete it inline, it is the forensic trail
   for a bad publish.

Because the bucket has versioning enabled, step 3 is recoverable: a bad flip can
be rolled back to the previous object version without regenerating anything.

`stale-while-revalidate=604800` on raw objects means a **failed nightly publish
never takes the mirror down** — the edge keeps serving the last good copy for up
to a week while revalidation retries.

## Required IAM permissions

The publish principal is scoped to **this one bucket** — never a wildcard
resource, and never any CloudFront permission beyond creating an invalidation.

| Action | Resource | Why |
|---|---|---|
| `s3:PutObject` | `arn:aws:s3:::<bucket>/_staging/*` | upload the staged tree |
| `s3:PutObject` | `arn:aws:s3:::<bucket>/*` | the atomic flip onto live keys |
| `s3:GetObject` | `arn:aws:s3:::<bucket>/*` | verification + server-side copy source |
| `s3:ListBucket` | `arn:aws:s3:::<bucket>` | enumerate staged vs live |
| `s3:DeleteObject` | `arn:aws:s3:::<bucket>/_staging/*` | optional staging cleanup |
| `cloudfront:CreateInvalidation` | the `distribution_arn` output | post-flip invalidation |

Explicitly **NOT** granted: `s3:PutBucketPolicy`, `s3:PutBucketPublicAccessBlock`,
`s3:DeleteBucket`, or any `cloudfront:Update*`. The publish job must not be able
to make the bucket public or reconfigure the distribution — that is this
Terraform module's job, and splitting it is the point.

Prefer a GitHub Actions OIDC role (no long-lived keys) with a trust policy
pinned to this repository and the publish workflow's ref.

### `content-type` at publish time

Set `--content-type 'text/plain; charset=utf-8'` on raw objects and
`application/json` on `index.json` when uploading. S3 object metadata is the
*primary* owner of `content-type`; the CloudFront response headers policy
overrides it as a safety net. See the `HEADER OWNERSHIP` comment in
`cloudfront.tf` for the full ownership table.
