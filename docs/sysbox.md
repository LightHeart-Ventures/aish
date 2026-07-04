# Sysbox: nested containers for coordinator workers

Background coordinators run inside a container (`Dockerfile.worker`). When one of
those coordinators itself wants to spawn a **grandchild** container (its own
sub-worker), the in-container `aish` needs a real Docker daemon to talk to — and
that daemon must create the grandchild in **the same mount namespace** as the
in-container `aish`, so a worktree path the child emits (`/aish/work/...`)
resolves identically for the daemon that creates it.

The naïve answer — bind-mount the host `docker.sock` (Docker-out-of-Docker) —
breaks path semantics (the **host** daemon resolves `-v` paths, not the
container's) and hands the container host-root. [Sysbox](https://github.com/nestybox/sysbox)
solves both: it is an OCI runtime that lets an **unprivileged** container run its
own private `dockerd` — no `--privileged`, no shared socket.

## How it works here

Three moving parts, all gated on the single env knob `AISH_SYSBOX_RUNTIME`:

1. **`src/container.rs`**
   - `sysbox_runtime_from_env()` — reads `AISH_SYSBOX_RUNTIME` (e.g.
     `sysbox-runc`); unset → stock runc/crun path, byte-for-byte unchanged.
   - `sysbox_registered(rt, name)` — probes `docker info` and confirms the
     runtime is actually registered with the daemon. Any error → `false`
     (misconfigured host silently falls back instead of emitting a bad flag).
   - `resolve_runtime_override(rt)` — returns the runtime name **only** when it's
     configured AND registered AND `rt == Docker` (Sysbox targets dockerd).
   - `run_argv()` — when the override is present, emits `--runtime=<name>`, adds
     `-e AISH_START_INNER_DOCKERD=1`, and **skips `--cap-drop=ALL`** (Sysbox needs
     to assign its own OCI cap set to the container init so the inner `dockerd`
     gets `CAP_SYS_ADMIN` etc. — safe because caps are confined to the shifted
     user namespace).

2. **`Dockerfile.worker`** — ships the full engine (`docker-ce` + `docker-ce-cli`
   + `containerd.io`), not just the CLI, because the daemon runs *inside*. The
   entrypoint is `scripts/worker-entrypoint.sh`.

3. **`scripts/worker-entrypoint.sh`** — PID 1. When `AISH_START_INNER_DOCKERD=1`
   it boots a private `dockerd`, waits (bounded, 15s) for `/var/run/docker.sock`,
   then `exec`s `aish`. Without the flag it `exec`s `aish` directly — identical
   to the historical entrypoint.

The in-container `aish` then runs its normal launch path: `detect_runtime` finds
the inner docker CLI+daemon, `resolve_selection` returns `Container(Docker)`, and
`run_argv`'s existing `-v {work}:{workdir}` logic **just works** — no
path-mapping special case, because inner daemon and inner `aish` share a mount
namespace.

## Host setup (one-time, per machine that runs workers)

Sysbox must be installed and registered with the host `dockerd`:

```
# Nestybox / Docker sysbox-ce release .deb
sudo apt install ./sysbox-ce_*.deb
docker info --format '{{json .Runtimes}}'   # verify sysbox-runc is listed
```

Requires a modern kernel with unprivileged user namespaces enabled
(Ubuntu 24.04 is fine out of the box).

Then opt workers in:

```
export AISH_SYSBOX_RUNTIME=sysbox-runc
```

If the runtime is not installed/registered, `resolve_runtime_override` returns
`None` and workers launch on the stock runtime exactly as before — the feature is
fully fail-safe and additive.

## Trade-offs

| | Sysbox (this) | DooD (socket share) |
|---|---|---|
| Nesting model | true nested (private inner daemon) | siblings on host daemon |
| Path semantics | correct, zero hacks | needs identity-mount hack |
| Host-root exposure | none (userns-shifted) | socket = host root |
| Image weight | heavier (full daemon) | light (CLI only) |
| Host dependency | sysbox-ce installed + registered | none |
| Startup cost | inner `dockerd` boot (~1–2s/worker) | none |

The only real adoption cost is the host install; everything on the `aish` side is
env-gated and inert until `AISH_SYSBOX_RUNTIME` is set.
