---
name: cua-repl
description: Drive the trycua/cua computer-use agent REPL (`python -m cua_agent.cli`) from aish — interactive GUI automation sessions, one-shot headless prompts, click prediction, sandbox provider selection, spend caps, and troubleshooting. Use when the user asks to "use cua", "drive the computer/GUI", "click/type in the VM", "run the computer-use agent", or when a task needs a real desktop instead of a shell.
---

# cua-repl — computer-use agent sessions from aish

Cua (https://github.com/trycua/cua) is an agent framework that drives a **real
desktop** (cloud container, macOS VM, Windows Sandbox, or Linux Docker
container) with screenshots + clicks + keystrokes. aish gives you a shell; cua
gives you a GUI. Reach for it only when the task genuinely needs pixels —
anything doable with a binary belongs in `run_program`.

## 0. Preflight — ALWAYS first

```
run_program bin/cua-doctor.sh      (cwd: plugins/cua-repl)
```

Reads `status  check  detail` lines; exit 1 means at least one FAIL. Fix FAILs
before launching — a missing extra or unset key otherwise manifests as the REPL
blocking on an interactive stdin prompt that you cannot answer.

Install, once:

```
python3 -m pip install 'cua-agent[cli,computer]'
```

`[cli]` → the REPL's `yaspin` spinner. `[computer]` → `cua-sandbox`, without
which the agent has nothing to drive. Requires Python **>=3.11,<3.14**.

## 1. Launching

| Intent | Call |
|---|---|
| Interactive session (human drives) | `run_interactive bin/cua-repl.sh` — full TTY handoff |
| One-shot task, you read the result | `run_program bin/cua-repl.sh ["<model>"] -p "<task>"` |
| Task from a file | `run_program bin/cua-repl.sh --prompt-file task.txt` |
| Where would it click? | `run_program bin/cua-repl.sh --predict-click "the Send button"` |

**Never** `run_interactive` for an unattended run and never `run_program` an
interactive session: the REPL reads `>` from stdin forever and freezes the
prompt. `-p/--prompt` is the headless path; without it the loop goes
interactive after the first task.

The wrapper injects the configured model, `--provider`, `--images`,
`--budget`, `--usage`, `--trajectory` and `CUA_REPL_ARGS`, then execs
`python -m cua_agent.cli`. A leading bare word overrides the model:

```
bin/cua-repl.sh openai/computer-use-preview -p "open Settings"
```

## 2. Models and loops

Model strings are `provider/model`, and a prefix composes a grounding loop:

- `anthropic/claude-sonnet-4-5-20250929` — Anthropic computer-use
- `openai/computer-use-preview` — OpenAI CUA
- `omniparser+anthropic/claude-…` — OmniParser grounding + any VLM
  (needs `pip install cua-som` separately; it is AGPL and NOT a cua-agent extra)
- `huggingface-local/ByteDance-Seed/UI-TARS-1.5-7B` — local UI-TARS

Each needs its own key: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, … Export them
from a credentials profile as `${profile:KEY}` — never inline a secret.

## 3. Providers (where the desktop lives)

| `--provider` | Desktop | Needs |
|---|---|---|
| `cloud` (default) | Cua-hosted container | `CUA_API_KEY` + `CUA_CONTAINER_NAME` |
| `docker` | local Linux container | docker daemon |
| `lume` | local macOS VM | lume CLI, Apple silicon |
| `winsandbox` | local Windows Sandbox | Windows Sandbox enabled |

`CUA_CONTAINER_NAME` unset + `cloud` ⇒ the REPL prompts on stdin. Always set it
for non-interactive runs. Local providers default the name to `cli-sandbox`.

## 4. Cost control (this loop is screenshot-heavy)

- `--budget 5` — hard cap; the run raises when the trajectory exceeds it. Keep
  one configured for every unattended run.
- `--images 3` — screenshots retained in context. Each one is expensive; 1–2 is
  usually enough for linear tasks.
- `-c/--cache` — prompt caching on supported providers.
- `-u/--usage` — print running cost per turn (and a session total).
- `--trajectory` — dump a replayable trajectory to `./trajectories` for
  debugging a bad run. It also writes screenshots to disk: mind the repo.

## 5. Reading a session

```
🛠️  click_left(412, 308)        💸 $0.14
🔧 Calling function: read_file
📤 <function output>
Total cost: $0.41
```

`🛠️` = computer action, `🔧/📤` = tool call + output, plain text = the model
talking. `exit` / `quit` / `q` leaves an interactive session.

## 6. Troubleshooting

| Symptom | Cause → fix |
|---|---|
| `CLI dependencies not found` | missing `[cli]` extra → install `cua-agent[cli]` |
| `Import error … sandbox` | missing `[computer]` extra → install `cua-agent[computer]` |
| Hangs with no output | REPL is prompting for container name / API key on stdin → set `CUA_CONTAINER_NAME`, `CUA_API_KEY` |
| `❌ API key is required for cloud provider` | `CUA_API_KEY` unset → export it or use `--provider docker` |
| Budget error mid-run | `--budget` exceeded → raise cap or shrink the task |
| `--predict-click` errors on PIL | `pip install pillow` |
| Clicks land in the wrong place | wrong grounding loop → try an `omniparser+…` model or `--predict-click` to inspect |

## 7. Etiquette

- Scope the prompt tightly; a vague GUI task burns dollars in screenshots.
- Prefer `--predict-click` to verify grounding before letting a loop click.
- Never point a session at a desktop holding credentials you would not paste
  into a model: every step is screenshotted and uploaded.
- Report the actual transcript tail and cost — never summarize a run you did
  not read.
