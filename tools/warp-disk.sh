#!/bin/sh
# Fill the /SVC of a K1K disk image with WARP: every service becomes a package for the platform
# k1k-x86_64, a signed throwaway repository is served for the length of the build, and `warp
# --root ... --platform k1k-x86_64 install` stages /SVC (the images and MANIFEST.TXT) which is
# then copied onto the FAT volume. WARP does not run on K1K yet, the host stages for it.
#
#   tools/warp-disk.sh <disk.img> <user-bin-dir> <version> <name:IMAGE:caps>...
#
# WARP comes from $WARP (a built binary of 0.4.12 or later) or is built from $WARP_SRC
# (default ../WARP). make-index.py comes from the same source tree.
set -eu
DISK="$1"; USER_BIN="$2"; VERSION="$3"; shift 3
HERE="$(cd "$(dirname "$0")/.." && pwd)"
B="$HERE/build/warp"
WARP_SRC="${WARP_SRC:-$HERE/../WARP}"
[ -f "$WARP_SRC/tools/make-index.py" ] || { echo "warp-disk: WARP sources not found (set WARP_SRC)" >&2; exit 1; }
if [ -z "${WARP:-}" ]; then
    rm -rf "$B/build"
    mkdir -p "$B/build"
    cp -r "$WARP_SRC/src" "$WARP_SRC/Makefile" "$B/build/"
    rm -f "$B/build/src/"*.o "$B/build/src/vendor/"*.o "$B/build/warp"
    make -s -C "$B/build" >/dev/null
    WARP="$B/build/warp"
fi
"$WARP" --version | grep -q 'warp 0\.4\.\(1[2-9]\|[2-9]\)' || { echo "warp-disk: $WARP is too old (need 0.4.12 or later for --root/--platform)" >&2; exit 1; }

rm -rf "$B/repo" "$B/root" "$B/pk"
mkdir -p "$B/repo" "$B/root"
[ -f "$B/priv.hex" ] || "$WARP" keygen "$B/priv.hex" "$B/pub.hex" >/dev/null

PKGS=""
for s in "$@"; do
    name="${s%%:*}"; rest="${s#*:}"; img="${rest%%:*}"; caps="${rest#*:}"
    [ "$caps" = "-" ] && caps=""
    d="$B/pk/$name"; mkdir -p "$d/files/SVC"
    cp "$USER_BIN/$name" "$d/files/SVC/$img"
    printf '{"name": "k1k-%s", "version": "%s", "description": "K1K service %s",\n "install_links": [{"from": "SVC/%s", "to": "/SVC/%s"}],\n "k1k_service": {"name": "%s", "image": "%s", "caps": "%s"}}\n' \
        "$name" "$VERSION" "$name" "$img" "$img" "$name" "$img" "$caps" > "$d/manifest.json"
    tar -C "$d" --owner=0 --group=0 --numeric-owner --mtime='2026-01-01 00:00:00' -I 'gzip -n' \
        -cf "$B/repo/k1k-$name-$VERSION-k1k_x86_64.warp" manifest.json files
    PKGS="$PKGS lab/k1k-$name"
done

# A free port, a server for the length of the build.
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
( cd "$B/repo" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1 ) & SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
python3 "$WARP_SRC/tools/make-index.py" "$B/repo" --base-url "http://127.0.0.1:$PORT" --key "$B/priv.hex" --warp "$WARP" >/dev/null
sleep 1

export WARP_ROOT="$B/root" WARP_PLATFORM=k1k-x86_64
"$WARP" repo add lab "http://127.0.0.1:$PORT" --pubkey-file "$B/pub.hex" >/dev/null
"$WARP" repo disable k1os >/dev/null 2>&1 || true      # this build needs no network but its own
"$WARP" update >/dev/null
for p in $PKGS; do "$WARP" install "$p" >/dev/null; done

mcopy -i "$DISK" -o "$B/root/SVC/MANIFEST.TXT" ::/SVC/MANIFEST.TXT
for f in "$B/root/SVC/"*; do
    [ "$(basename "$f")" = MANIFEST.TXT ] || mcopy -i "$DISK" -o "$f" "::/SVC/$(basename "$f")"   # links into the store: mcopy follows them
done
echo "warp-disk: $(grep -vc '^#' "$B/root/SVC/MANIFEST.TXT") services staged by WARP into /SVC"
