# aish-os — a bare-bones Linux image with `aish` as the default shell

This directory builds a **minimal Linux system whose default login shell is
[`aish`](https://github.com/LightHeart-Ventures/aish)** — the AI-native shell —
provisioned by a **network installer** (Debian's `debian-installer`, driven by a
preseed file). Boot media is a ~60 MB netinst image; everything else is pulled
from the network at install time.

---

## TL;DR recommendation: build off Debian, do **not** roll our own distro

**Decision: build off an existing distro (Debian netinst + preseed). Rolling our
own distro is the wrong tool for this job.**

`aish` is a single userspace binary. It is *not* a kernel, an init system, a
libc, or a package manager. What it actually needs from the OS underneath it is
small and boring:

| aish runtime requirement | verified against the `v0.27.0` linux release binary |
|---|---|
| glibc x86-64 userland | `ELF 64-bit … interpreter /lib64/ld-linux-x86-64.so.2, for GNU/Linux 3.2.0` |
| `libstdc++6`, `libgcc-s1`, `libm` | in the Debian base system |
| `libgomp1` (OpenMP, pulled in by the `local` llama.cpp feature) | one extra apt package |
| `ca-certificates` | TLS to the Anthropic API |
| `git` | coordinator worktree / VCS tooling |
| a network | it's an *AI* shell — it talks to Claude |

That is the entire dependency surface. A general-purpose distro gives us all of
it, plus the parts that are genuinely hard and genuinely not our value-add:
a maintained kernel, signed security updates, hardware/driver coverage,
bootloader, initramfs, and a package manager. Owning those ourselves buys us
nothing and costs us a permanent maintenance tax.

### Why not roll our own?

Rolling our own (Buildroot / Yocto / Linux-From-Scratch / a custom package
tree) means we now own:

- kernel selection, config, and CVE patching cadence
- bootloader + initramfs + init/service manager
- a libc and core userland (coreutils, etc.)
- a package manager **or** an immutable-image build+update pipeline
- driver/firmware coverage for whatever hardware it lands on
- a security-update supply chain

None of that moves the needle on "aish is the default shell." It's months of
work to reach the reliability a distro already ships, and it never stops needing
attention. **Roll-your-own only earns its keep when the OS *itself* is the
product** (a router firmware, a unikernel appliance, a hard real-time target).
Here the shell is the product and the OS is plumbing — so we lease the plumbing.

### Why Debian specifically (vs. Ubuntu / Alpine / Arch / Fedora)?

- **Debian `debian-installer` is the canonical "network installer."** The
  netinst ISO is tiny (~60 MB), and it is driven by a mature, fully-documented
  **preseed** automation format. It also boots over **PXE/netboot** with the
  same config, so the exact same recipe covers ISO *and* diskless network
  provisioning. This is the most literal match for "let's use a network
  installer."
- **Ubuntu** works too (it *is* Debian-based) and our repo already ships
  `scripts/install-ubuntu-24.04.sh`, but modern Ubuntu Server moved to
  `subiquity`/autoinstall (cloud-init YAML). Debian preseed is lighter and
  closer to the metal for a *bare-bones* target.
- **Alpine (musl)** is the tempting ultra-minimal choice (a ~50 MB system), but
  **our published binary is glibc-linked** (`interpreter … ld-linux-x86-64`, and
  it pulls `libstdc++`/`libgomp`). Running it on Alpine needs either
  `gcompat`/`glibc` shims or a dedicated `x86_64-unknown-linux-musl` build. That
  is a real, worthwhile future option (see "Going smaller" below) but it is a
  *build-system change*, not a day-one path.
- **Arch/Fedora** are fine technically; Arch is rolling (less predictable for an
  appliance) and Fedora's kickstart is heavier than a Debian preseed for a
  strictly minimal box.

**Bottom line:** Debian netinst + preseed = smallest reasonable footprint,
strongest automation story, native network-installer + PXE support, and zero
new build-system to own.

---

## What this image contains (bare-bones on purpose)

The preseed installs **standard system utilities only** — no desktop, no X, no
tasksel bundles — plus:

- `aish` (fetched from the pinned GitHub release, checksum-verified) at
  `/usr/local/bin/aish`, registered in `/etc/shells`
- its runtime deps: `libgomp1`, `libstdc++6`, `libgcc-s1`, `ca-certificates`, `git`
- a login user (`aish`) **and root**, both with `aish` set as the login shell
- an `/etc/skel/.aishrc` + `/etc/profile.d/aish.sh` so `ANTHROPIC_API_KEY` and
  friends are picked up
- optional OpenSSH server (toggle in `preseed.cfg`)

First boot drops you straight into the aish REPL as the login shell.

---

## Files

| file | purpose |
|---|---|
| `preseed.cfg` | Debian `debian-installer` answer file — fully unattended, bare-bones install that ends by running the post-install hook |
| `aish-postinstall.sh` | in-target hook: installs runtime deps, fetches + verifies the aish binary, registers `/etc/shells`, sets aish as the default shell for the user + root, seeds config |
| `build-netinst-iso.sh` | remasters an official Debian netinst ISO so the preseed + post-install are baked in and auto-selected (fully hands-off boot media) |
| `pxe-netboot.md` | how to serve the *same* preseed over PXE/TFTP + HTTP for diskless network installs |
| `hyper-v-install.md` | step-by-step install of the built ISO on Microsoft Hyper-V (Gen 1 & Gen 2, PowerShell + GUI) |

---

## Quick start (build unattended install media)

```
# 1. Build a hands-off netinst ISO (downloads the official Debian netinst,
#    injects preseed.cfg + aish-postinstall.sh, re-masters an auto-booting ISO)
cd image
sudo ./build-netinst-iso.sh                 # -> dist/aish-os-netinst-amd64.iso

# 2. Write it to a USB stick (or attach to a VM) and boot.
#    The installer runs unattended and reboots into a system where `aish`
#    is the login shell.

# 3. First login: set your key, and you're in the aish REPL.
#    export ANTHROPIC_API_KEY=sk-ant-...
```

Override the pinned aish version or the login credentials at build time:

```
AISH_VERSION=v0.27.0 AISH_USER=me ./build-netinst-iso.sh
```

## Quick start (network / PXE, no ISO)

Serve `preseed.cfg` over HTTP and point the Debian netboot initrd at it — see
`pxe-netboot.md`. The `preseed/late_command` fetches `aish-postinstall.sh` from
the same web root, so the boot media stays stock.

---

## Going smaller later (musl / immutable)

Two clean follow-ups, both *build-system* changes rather than distro swaps:

1. **`x86_64-unknown-linux-musl` static build** → a fully static aish that runs
   on Alpine or even `FROM scratch`. Add the target to the release matrix, then
   an Alpine `setup-alpine`-answers file mirrors this preseed at ~a third of the
   footprint.
2. **Immutable appliance** (mkosi / debootstrap → single squashfs + verity)
   when you want an unattended, atomically-updated aish kiosk rather than a
   general-purpose box. Same Debian base, different delivery.

Neither is needed to ship "bare-bones Debian with aish as the default shell,"
which is what this directory does today.
