# cua-repl Plugin — Installation Guide

Drives the [trycua/cua](https://github.com/trycua/cua) computer-use agent REPL
(`python -m cua_agent.cli`) from aish: interactive GUI sessions, one-shot
headless prompts, click prediction, and a preflight doctor.

---

## Prerequisites

| Requirement | Why | Check |
|---|---|---|
| Python **>=3.11,<3.14** | `cua-agent` requires-python | `python3 -V` |
| `cua-agent[cli,computer]` | REPL (`yaspin`) + `cua-sandbox` computer control | `python3 -c 'import cua_agent, yaspin, cua_sandbox'` |
| A model key | `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` | per model string |
| A desktop | cloud container **or** docker / lume / winsandbox | see table below |

Install:

```bash
python3 -m pip install 'cua-agent[cli,computer]'
```

> `omniparser+…` model strings additionally need `pip install cua-som`, which is
> AGPL-3.0 and deliberately **not** a `cua-agent` extra.

### Secrets

Put keys in `~/.aish/credentials` and reference them as `${profile:KEY}` — never
inline a secret in `plugin.json`, a script, or the session env:

```ini
[profile:cua]
CUA_API_KEY = ...
ANTHROPIC_API_KEY = ...
```

---

## Verification

```bash
cd plugins/cua-repl
./bin/cua-doctor.sh
```

Expected (cloud provider, keys exported):

```
OK     python                  Python 3.12.7 at /usr/bin/python3
OK     cua-agent               0.9.1
OK     cli-extra               yaspin present
OK     cua-sandbox             present (computer control available)
OK     cua-api-key             set
OK     container               cli-sandbox
OK     model-key               ANTHROPIC_API_KEY set
OK     budget                  --budget $5
```

Exit code 1 means at least one `FAIL` — fix those before launching, or the REPL
will block on an interactive stdin prompt.

Smoke-test a one-shot run (no GUI side effects):

```bash
./bin/cua-repl.sh --provider docker -p "take a screenshot and describe the desktop"
```

---

## Configuration

`plugin.json → config_schema`:

| Key | Default | Notes |
|---|---|---|
| `python` | `python3` | interpreter holding `cua-agent` (→ `CUA_PYTHON`) |
| `model` | `anthropic/claude-sonnet-4-5-20250929` | default model string |
| `provider` | `cloud` | `cloud` \| `lume` \| `winsandbox` \| `docker` |
| `container_name` | `cli-sandbox` | → `CUA_CONTAINER_NAME`, stops stdin prompts |
| `credential_profile` | `cua` | credentials section holding the keys |
| `budget_usd` | `5` | → `--budget`, hard per-session spend cap |
| `images` | `3` | → `--images`, screenshots kept in context |
| `show_usage` | `true` | → `-u`, per-turn cost |
| `trajectory` | `false` | → `--trajectory`, writes `./trajectories` |
| `extra_args` | `""` | raw flags appended (→ `CUA_REPL_ARGS`) |

### Providers

| `provider` | Desktop | Needs |
|---|---|---|
| `cloud` | Cua-hosted container | `CUA_API_KEY`, `CUA_CONTAINER_NAME` (https://cua.ai) |
| `docker` | local Linux container | docker daemon |
| `lume` | local macOS VM | `lume` CLI (Apple silicon) |
| `winsandbox` | local Windows VM | Windows Sandbox enabled |

---

## Usage

| Intent | Command |
|---|---|
| Interactive session | `run_interactive bin/cua-repl.sh` (full TTY handoff) |
| One-shot headless task | `bin/cua-repl.sh -p "open Safari and search for cua"` |
| Override the model | `bin/cua-repl.sh openai/computer-use-preview -p "…"` |
| Prompt from a file | `bin/cua-repl.sh --prompt-file task.txt` |
| Inspect grounding | `bin/cua-repl.sh --predict-click "the Send button"` |

Everything after the optional leading model string is passed through verbatim to
`python -m cua_agent.cli`.

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| `CLI dependencies not found` | `pip install 'cua-agent[cli]'` |
| `Import error … sandbox` | `pip install 'cua-agent[computer]'` |
| Hangs with no output | REPL is prompting on stdin → set `CUA_CONTAINER_NAME` / `CUA_API_KEY` |
| `❌ API key is required for cloud provider` | export `CUA_API_KEY` or use `--provider docker` |
| exit 69 from the wrapper | `cua-agent` not importable by `$CUA_PYTHON` |
| exit 78 from the wrapper | `provider=cloud` with no `CUA_API_KEY` |
| Budget error mid-run | raise `budget_usd` or shrink the task |
| `--predict-click` PIL error | `pip install pillow` |
| Clicks land wrong | try an `omniparser+…` model, or `--predict-click` first |

---

## Cost warning

This is a screenshot loop: every step uploads an image. Always keep a
`--budget`, keep `--images` low (1–3), and scope prompts tightly. Use
`-u/--usage` to watch spend per turn.

---

## Links

- **Upstream:** https://github.com/trycua/cua
- **Cua cloud containers:** https://cua.ai/
- **Skill:** `skills/cua-repl/SKILL.md` (full playbook)
