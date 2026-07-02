# PXE / netboot install (no ISO)

The same `preseed.cfg` provisions aish-os over the network with **zero boot
media** — the most literal reading of "use a network installer." A machine PXE-
boots the Debian installer kernel+initrd over TFTP, then pulls the preseed (and
`aish-postinstall.sh`) over HTTP.

## Pieces

1. **DHCP** hands out `next-server` (your TFTP host) + `filename`
   (`pxelinux.0` for BIOS, `debian-installer/amd64/bootnetx64.efi` for UEFI).
2. **TFTP** serves the Debian **netboot** tarball for bookworm:
   `https://deb.debian.org/debian/dists/bookworm/main/installer-amd64/current/images/netboot/netboot.tar.gz`
   Untar it into your TFTP root.
3. **HTTP** (any static web server) serves two files from this directory:
   - `preseed.cfg`
   - `aish-postinstall.sh`

## Wire the preseed into the boot append line

Edit the netboot menu (`pxelinux.cfg/default` for BIOS, or the grub cfg for
UEFI) so the install entry appends:

```
auto=true priority=critical url=http://YOUR_HTTP_HOST/preseed.cfg
```

`url=` is the network equivalent of the ISO's `preseed/file=`. Debian fetches
and applies the preseed before partitioning.

Example `pxelinux.cfg/default` entry (BIOS):

```
default aishinstall
prompt 0
timeout 30
label aishinstall
    kernel debian-installer/amd64/linux
    append vga=788 initrd=debian-installer/amd64/initrd.gz auto=true priority=critical url=http://YOUR_HTTP_HOST/preseed.cfg ---
```

## One required edit to `preseed.cfg` for the netboot path

There is no CD, so the `late_command` won't find `/cdrom/aish-postinstall.sh`.
The shipped `late_command` already falls back to `wget`-ing the hook from
GitHub `raw`. If you'd rather serve it from **your** HTTP host (air-gapped or
pinned), change that fallback URL:

```
in-target sh -c 'wget -q -O /tmp/aish-postinstall.sh http://YOUR_HTTP_HOST/aish-postinstall.sh'
```

Everything else — deps, checksum-verified aish download, `/etc/shells`
registration, default-shell switch — is identical to the ISO path because it's
the same hook script.

## Air-gapped note

For a fully offline network install, also mirror the Debian `bookworm` suite
locally and point `d-i mirror/http/hostname` in `preseed.cfg` at your mirror,
and host the aish release asset on your HTTP server (set `AISH_REPO`/URL in the
hook accordingly).
