# ccquota Plugin — Installation Guide

This plugin provides **Claude API consumption quota tracking** in aish, letting you monitor real-time token usage, costs, and rate-limit status across your Anthropic API account.

---

## Prerequisites

### Anthropic API Key

You already have this set up (aish needs it to run). Verify:

```sh
echo $ANTHROPIC_API_KEY
# Should NOT be empty
```

If missing, see the aish config guide:

```
:skill add aish-config-guide
```

---

## Verification

### Step 1: API key is set

```sh
echo $ANTHROPIC_API_KEY | head -c 20
# Should show: sk_ant_... (first 20 chars)
```

### Step 2: aish plugin is discovered

From inside aish:

```
:mcp
```

You should see:

```
ccquota (local)
  Tools: 3
    - get_quota
    - get_usage
    - estimate_cost
```

Or check directly:

```sh
ls ~/.aish/plugins/ccquota/
# Should show: plugin.json, README.md
```

### Step 3: Try a quota query

From inside aish:

```
get_quota {}
```

Expected output:

```json
{
  "quota": {
    "monthly_limit_usd": 100.0,
    "monthly_spent_usd": 24.50,
    "remaining_usd": 75.50,
    "reset_date": "2024-02-01"
  }
}
```

---

## Installation Summary

| Step | Command | Expected Output |
|------|---------|-----------------|
| 1. Verify API key | `echo $ANTHROPIC_API_KEY` | API key is not empty |
| 2. Check aish plugin | `:mcp` from inside aish | `ccquota` appears in MCP server list |
| 3. Test quota query | `get_quota {}` | Quota and usage data returned |

---

## Configuration

### Environment Variables

```sh
# Required (already set)
export ANTHROPIC_API_KEY="sk_ant_..."

# Optional
export CCQUOTA_CACHE_SECS=300      # Cache quota data for 5 minutes (default: 300)
export CCQUOTA_WARN_PERCENT=80     # Alert when usage crosses 80% of limit (default: 80)
```

---

## Usage

### Get your quota and usage

```
get_quota {}
```

Returns:
- Monthly spending limit (USD)
- Current month's spending (USD)
- Remaining budget (USD)
- Next reset date

### Get detailed usage breakdown

```
get_usage { start_date: "2024-01-01", end_date: "2024-01-31" }
```

Returns:
- Tokens used by model (Opus, Sonnet, Haiku)
- Cost by model
- Request count

### Estimate cost of a task

```
estimate_cost { model: "claude-3-5-sonnet-20241022", input_tokens: 50000, output_tokens: 10000 }
```

Returns:
- Estimated cost (USD)
- Breakdown by input/output

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `:mcp` doesn't show `ccquota` | Plugin not discovered | Restart aish: `:restart` |
| "Invalid API key" in query results | `ANTHROPIC_API_KEY` is wrong or expired | Check `echo $ANTHROPIC_API_KEY`, regenerate key on console.anthropic.com |
| "Rate limit exceeded" | Too many quota queries in short time | Wait 1 minute, or increase `CCQUOTA_CACHE_SECS` |
| Quota shows as $0 / empty | API returns no data (new account?) | Check your account on https://console.anthropic.com — ensure billing is set up |

---

## Next Steps

1. **Monitor your spend regularly:**
   ```
   get_quota {}
   ```

2. **Set up an alert** (optional):
   ```
   :alert "usage exceeds 80% of monthly budget" \
     --condition "get_quota { } | .quota.remaining_usd < (.quota.monthly_limit_usd * 0.2)"
   ```

3. **Check usage trends:**
   ```
   get_usage { start_date: "2024-01-01", end_date: "2024-01-31" }
   ```

4. **Estimate before large runs:**
   ```
   estimate_cost { model: "claude-3-5-opus-20250805", input_tokens: 1000000, output_tokens: 100000 }
   ```

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/ccquota/`)
- **Anthropic console:** https://console.anthropic.com
- **API pricing:** https://www.anthropic.com/pricing
- **aish docs:** https://github.com/LightHeart-Ventures/aish
