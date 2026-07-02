# Installing aish-os on Hyper-V

This walks through booting the `dist/aish-os-netinst-amd64.iso` produced by
[`build-netinst-iso.sh`](./build-netinst-iso.sh) on **Microsoft Hyper-V**
(Windows 10/11 Pro, Windows Server, or Hyper-V Server). The ISO is a
BIOS+UEFI hybrid, so it boots on **either** Hyper-V VM generation — but the
knobs differ, so read the generation note below first.

---

## 0. Prereqs

- Hyper-V enabled: *Settings → Apps → Optional features → More Windows features
  → Hyper-V* (or `Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -All`).
- The built ISO copied somewhere Windows can read, e.g. `C:\iso\aish-os-netinst-amd64.iso`.
- A virtual switch with outbound network — the **Default Switch** (NAT) is
  fine and exists out of the box. The netinst *must* reach the network: it
  pulls Debian packages **and** the aish binary from GitHub at install time.

---

## 1. Generation 1 vs Generation 2 — pick one

| | **Gen 1 (recommended, simplest)** | **Gen 2** |
|---|---|---|
| Firmware | BIOS → boots the ISO's `isolinux` path | UEFI → boots the ISO's `grub` path |
| Secure Boot | n/a | **must be disabled** (our re-mastered grub is unsigned) |
| Network adapter | synthetic "Network Adapter" works (modern Debian has `hv_netvsc`) | synthetic, works |
| Boot-from-ISO | just attach the DVD drive | attach DVD **and** move it first in the firmware boot order |
| When to use | you just want it to work | you need UEFI/GPT, >2 TB disk, or a UEFI-only policy |

**If you don't have a specific reason to use Gen 2, use Gen 1** — no Secure Boot
dance, no boot-order fiddling. The instructions below give both.

> Secure Boot: the stock Debian installer is Microsoft-signed and *would* work
> with Secure Boot set to the **"Microsoft UEFI Certificate Authority"**
> template — but our ISO injects a custom unsigned `grub.cfg`/boot entry, so on
> Gen 2 you must **turn Secure Boot off**. Otherwise the VM refuses to boot the
> installer.

---

## 2A. Quickest path — PowerShell (Gen 1)

Run PowerShell **as Administrator**:

```powershell
$VMName = "aish-os"
$ISO    = "C:\iso\aish-os-netinst-amd64.iso"
$VHD    = "C:\Hyper-V\$VMName.vhdx"
$Switch = "Default Switch"          # NAT switch that ships with Hyper-V

New-VM -Name $VMName -Generation 1 -MemoryStartupBytes 2GB `
       -NewVHDPath $VHD -NewVHDSizeBytes 12GB -SwitchName $Switch

Set-VM -Name $VMName -ProcessorCount 2

# Attach the installer ISO and boot from it first
Set-VMDvdDrive  -VMName $VMName -Path $ISO
Set-VMBios      -VMName $VMName -StartupOrder @("CD","IDE","LegacyNetworkAdapter","Floppy")

Start-VM -Name $VMName
vmconnect.exe localhost $VMName     # opens the console window
```

That's it. The re-mastered ISO auto-selects the **"Automated aish-os install"**
entry (30 s timeout, or press Enter), runs the preseed fully unattended,
installs the aish binary via the post-install hook, and reboots into a system
where `aish` is the login shell.

---

## 2B. PowerShell (Gen 2 — UEFI)

```powershell
$VMName = "aish-os"
$ISO    = "C:\iso\aish-os-netinst-amd64.iso"
$VHD    = "C:\Hyper-V\$VMName.vhdx"
$Switch = "Default Switch"

New-VM -Name $VMName -Generation 2 -MemoryStartupBytes 2GB `
       -NewVHDPath $VHD -NewVHDSizeBytes 12GB -SwitchName $Switch

Set-VM -Name $VMName -ProcessorCount 2

# Debian's re-mastered grub is unsigned -> Secure Boot OFF
Set-VMFirmware -VMName $VMName -EnableSecureBoot Off

# Attach ISO, then make the DVD the first boot device
$dvd = Add-VMDvdDrive -VMName $VMName -Path $ISO -Passthru
Set-VMFirmware -VMName $VMName -FirstBootDevice $dvd

Start-VM -Name $VMName
vmconnect.exe localhost $VMName
```

---

## 3. GUI path (Hyper-V Manager) — equivalent to the above

1. **Action → New → Virtual Machine.**
2. **Generation:** choose **Generation 1** (simplest) or Generation 2.
3. **Memory:** 2048 MB, Dynamic Memory on is fine.
4. **Networking:** connect to **Default Switch**.
5. **Virtual hard disk:** create a new dynamic VHDX, ≥ 12 GB.
6. **Installation options:** *Install an operating system from a bootable
   image file* → browse to `aish-os-netinst-amd64.iso`.
7. Finish, then **before starting**, open the VM's **Settings**:
   - **Gen 2 only:** *Security* → **uncheck "Enable Secure Boot."**
   - **Gen 2 only:** *Firmware* → move **DVD Drive** to the top of the boot order.
   - *Processor* → 2 vCPUs (optional, speeds the install).
8. **Connect**, then **Start**. Let the auto-install run.

---

## 4. What happens during install (unattended)

The preseed does a bare-bones Debian install (standard utilities only, no
desktop), then the post-install hook:

- installs runtime deps (`libgomp1`, `libstdc++6`, `libgcc-s1`,
  `ca-certificates`, `git`),
- downloads the pinned aish release binary **and its `.sha256`**, verifies the
  checksum, installs it to `/usr/local/bin/aish`,
- registers it in `/etc/shells` and sets it as the login shell for the `aish`
  user **and** root,
- seeds `/etc/profile.d/aish.sh` so `ANTHROPIC_API_KEY` is picked up.

Total time is mostly network-bound (base system + one apt round-trip + a
~single binary download).

---

## 5. First boot

Log in as the `aish` user (default password is set in `preseed.cfg` — change
it). You land straight in the aish REPL. Export your key and go:

```
export ANTHROPIC_API_KEY=sk-ant-...
```

To persist it, drop it in `~/.aishrc` or `/etc/profile.d/aish.sh` (the latter is
already scaffolded by the installer).

---

## 6. Troubleshooting

| symptom | cause / fix |
|---|---|
| **Gen 2 VM shows "Boot Failed. EFI SCSI Device"** then nothing | Secure Boot still on, or DVD not first in boot order. Fix both in *Settings → Security / Firmware*. |
| Installer boots to a **manual menu** instead of auto-installing | You landed on the stock Debian menu — pick **"Automated aish-os install."** (The 30 s auto-timeout should select it for you.) |
| **No network / apt fails** in the installer | The NIC isn't on a switch with outbound access. Attach the adapter to the **Default Switch** (NAT). On Gen 1, the *synthetic* "Network Adapter" is correct — you do **not** need the Legacy Network Adapter (modern Debian kernels ship `hv_netvsc`). |
| **aish download fails** (`aish-postinstall.sh`) | GitHub unreachable from the VM, or the pinned `AISH_VERSION` asset was renamed. Confirm the VM has egress; re-run the hook, or rebuild the ISO with a valid `AISH_VERSION=`. |
| Want a **static IP** instead of DHCP | Preseed uses DHCP by default; set `d-i netcfg/disable_dhcp` + the static keys in `preseed.cfg`. |
| Console mouse/resize is awkward | That's expected for a text console — Hyper-V's *Enhanced Session* mode is for GUI/RDP guests; a bare-bones text box just uses the basic video console. |

---

## 7. Fully-scripted, no-console variant

Everything in §2A/§2B is already unattended once the VM starts. To provision a
VM end-to-end from a single script (create → install → wait → report IP), wrap
the PowerShell above and poll `Get-VMNetworkAdapter -VMName aish-os` for the
DHCP-assigned address after the install reboots. The install itself needs no
keyboard input — the preseed answers every prompt.
