#!/bin/sh
# usage: mkiso.sh <kernel-elf> <limine.conf> <out.iso>
set -eu
KERNEL=$1
CONF=$2
OUT=$3
LIMINE=third_party/limine
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/k1k-iso.XXXXXX")
trap 'rm -rf "$ROOT"' EXIT

mkdir -p "$ROOT/boot/limine" "$ROOT/EFI/BOOT"
cp "$KERNEL" "$ROOT/boot/k1k"
cp "$CONF" "$ROOT/boot/limine/limine.conf"
cp "$LIMINE/limine-bios.sys" "$LIMINE/limine-bios-cd.bin" "$LIMINE/limine-uefi-cd.bin" "$ROOT/boot/limine/"
cp "$LIMINE/BOOTX64.EFI" "$LIMINE/BOOTIA32.EFI" "$ROOT/EFI/BOOT/"

mkdir -p "$(dirname "$OUT")"
xorriso -as mkisofs -quiet -b boot/limine/limine-bios-cd.bin \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    --efi-boot boot/limine/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    "$ROOT" -o "$OUT" 2>/dev/null
"$LIMINE/limine" bios-install "$OUT" 2>/dev/null
echo "built $OUT"
