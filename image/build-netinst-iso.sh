#!/usr/bin/env bash
# build-netinst-iso.sh — re-master an official Debian netinst ISO into a
# hands-off installer that provisions a bare-bones system with aish as the
# default login shell.
#
# It:
#   1. downloads the official Debian (bookworm) amd64 netinst ISO if absent
#   2. extracts it, injects preseed.cfg + aish-postinstall.sh
#   3. adds an auto-selected "Automated aish-os install" boot entry
#      (both BIOS/isolinux and UEFI/grub) that preseeds from /cdrom/preseed.cfg
#   4. repacks a BIOS+UEFI hybrid ISO to dist/aish-os-netinst-amd64.iso
#
# Requires: xorriso, and isolinux's isohdpfx.bin (Debian pkg: isolinux).
# Run with enough privilege to write the work dir (sudo recommended for perms
# on extracted read-only ISO trees).
#
# Tunables (env):
#   AISH_VERSION   aish release tag baked into preseed late_command (default v0.27.0)
#   AISH_USER      login user switched to aish                       (default aish)
#   DEBIAN_ISO_URL override the netinst source ISO URL
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
AISH_VERSION="${AISH_VERSION:-v0.27.0}"
AISH_USER="${AISH_USER:-aish}"

DEBIAN_VERSION="12.11.0"
DEBIAN_ISO_URL="${DEBIAN_ISO_URL:-https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-${DEBIAN_VERSION}-amd64-netinst.iso}"

WORK="${HERE}/.build"
SRC_ISO="${WORK}/debian-netinst-amd64.iso"
EXTRACT="${WORK}/iso"
OUT_DIR="${HERE}/dist"
OUT_ISO="${OUT_DIR}/aish-os-netinst-amd64.iso"

log() { printf '\033[0;34m[build-iso]\033[0m %s\n' "$*"; }
die() { printf '\033[0;31m[build-iso] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# ---- deps ---------------------------------------------------------------
command -v xorriso >/dev/null 2>&1 || die "xorriso not found (apt-get install xorriso)"
ISOHDPFX=""
for c in /usr/lib/ISOLINUX/isohdpfx.bin /usr/lib/syslinux/isohdpfx.bin /usr/share/syslinux/isohdpfx.bin; do
  [ -f "$c" ] && ISOHDPFX="$c" && break
done
[ -n "$ISOHDPFX" ] || die "isohdpfx.bin not found (apt-get install isolinux syslinux-common)"

[ -f "${HERE}/preseed.cfg" ] || die "preseed.cfg missing next to this script"
[ -f "${HERE}/aish-postinstall.sh" ] || die "aish-postinstall.sh missing next to this script"

mkdir -p "$WORK" "$OUT_DIR"

# ---- 1. fetch the source ISO -------------------------------------------
if [ ! -f "$SRC_ISO" ]; then
  log "downloading Debian netinst: $DEBIAN_ISO_URL"
  if command -v wget >/dev/null 2>&1; then wget -O "$SRC_ISO" "$DEBIAN_ISO_URL"
  else curl -fSL -o "$SRC_ISO" "$DEBIAN_ISO_URL"; fi
else
  log "reusing cached source ISO: $SRC_ISO"
fi

# ---- 2. extract ---------------------------------------------------------
log "extracting ISO -> $EXTRACT"
rm -rf "$EXTRACT"
mkdir -p "$EXTRACT"
xorriso -osirrox on -indev "$SRC_ISO" -extract / "$EXTRACT" >/dev/null 2>&1
chmod -R u+w "$EXTRACT"

# ---- 3. inject preseed + hook ------------------------------------------
log "injecting preseed.cfg + aish-postinstall.sh"
# Bake the requested version/user into the copied preseed so the ISO is
# self-describing (overrides the defaults inside preseed.cfg's late_command).
sed \
  -e "s|AISH_VERSION=v[0-9][0-9.]*|AISH_VERSION=${AISH_VERSION}|g" \
  -e "s|AISH_USER=[A-Za-z0-9_]*|AISH_USER=${AISH_USER}|g" \
  "${HERE}/preseed.cfg" > "${EXTRACT}/preseed.cfg"
install -m 0755 "${HERE}/aish-postinstall.sh" "${EXTRACT}/aish-postinstall.sh"

APPEND_OPTS="auto=true priority=critical preseed/file=/cdrom/preseed.cfg"

# ---- 3a. BIOS boot (isolinux) ------------------------------------------
if [ -d "${EXTRACT}/isolinux" ]; then
  log "adding isolinux (BIOS) auto-install entry"
  cat > "${EXTRACT}/isolinux/aish.cfg" <<EOF
default aishinstall
prompt 0
timeout 30
label aishinstall
    menu label ^Automated aish-os install
    kernel /install.amd/vmlinuz
    append vga=788 initrd=/install.amd/initrd.gz ${APPEND_OPTS} ---
EOF
  # Make our menu the one isolinux reads first.
  if ! grep -q 'include aish.cfg' "${EXTRACT}/isolinux/isolinux.cfg" 2>/dev/null; then
    printf 'include aish.cfg\n%s' "$(cat "${EXTRACT}/isolinux/isolinux.cfg")" \
      > "${EXTRACT}/isolinux/isolinux.cfg.new"
    mv "${EXTRACT}/isolinux/isolinux.cfg.new" "${EXTRACT}/isolinux/isolinux.cfg"
  fi
fi

# ---- 3b. UEFI boot (grub) ----------------------------------------------
if [ -f "${EXTRACT}/boot/grub/grub.cfg" ]; then
  log "adding grub (UEFI) auto-install entry"
  cat > "${EXTRACT}/boot/grub/aish.cfg" <<EOF
set default=0
set timeout=3
menuentry "Automated aish-os install" {
    set background_color=black
    linux    /install.amd/vmlinuz vga=788 ${APPEND_OPTS} ---
    initrd   /install.amd/initrd.gz
}
EOF
  if ! grep -q 'source /boot/grub/aish.cfg' "${EXTRACT}/boot/grub/grub.cfg" 2>/dev/null; then
    printf 'source /boot/grub/aish.cfg\n%s' "$(cat "${EXTRACT}/boot/grub/grub.cfg")" \
      > "${EXTRACT}/boot/grub/grub.cfg.new"
    mv "${EXTRACT}/boot/grub/grub.cfg.new" "${EXTRACT}/boot/grub/grub.cfg"
  fi
fi

# ---- 4. regenerate md5sum.txt (d-i integrity check) --------------------
if [ -f "${EXTRACT}/md5sum.txt" ]; then
  log "regenerating md5sum.txt"
  ( cd "$EXTRACT"
    find . -type f ! -name md5sum.txt -print0 | xargs -0 md5sum > md5sum.txt )
fi

# ---- 5. repack a BIOS+UEFI hybrid ISO ----------------------------------
log "repacking -> $OUT_ISO"
EFI_ARGS=()
if [ -f "${EXTRACT}/boot/grub/efi.img" ]; then
  EFI_ARGS=( -eltorito-alt-boot -e boot/grub/efi.img -no-emul-boot -isohybrid-gpt-basdat )
fi

xorriso -as mkisofs \
  -r -V "AISH_OS" \
  -o "$OUT_ISO" \
  -J -joliet-long \
  -isohybrid-mbr "$ISOHDPFX" \
  -partition_offset 16 \
  -c isolinux/boot.cat \
  -b isolinux/isolinux.bin \
  -no-emul-boot -boot-load-size 4 -boot-info-table \
  "${EFI_ARGS[@]}" \
  "$EXTRACT"

log "done: $OUT_ISO"
log "write it to USB (e.g. 'dd if=$OUT_ISO of=/dev/sdX bs=4M status=progress') or boot it in a VM."
