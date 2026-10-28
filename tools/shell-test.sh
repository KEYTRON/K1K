#!/bin/sh
# Drive the shell the way a person would: boot K1K with a disk, wait for the
# prompt, type commands through the QEMU monitor (`sendkey`, which goes through
# the emulated PS/2 keyboard and so through the ring-3 `kbd` driver) and check
# that the shell's answers appear on the serial console.
#
#   tools/shell-test.sh <k1k.iso> <disk.img> <qemu flags...>
set -eu
ISO="$1"; DISK="$2"; shift 2
D="$(mktemp -d)"
LOG="$D/serial.log"; MON="$D/mon.sock"
QPID=""
cleanup() { [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true; rm -rf "$D"; }
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; echo "--- serial tail:" >&2; tail -25 "$LOG" >&2; exit 1; }

qemu-system-x86_64 -M q35 -cdrom "$ISO" -boot d -display none -serial "file:$LOG" \
    -monitor "unix:$MON,server,nowait" -no-reboot \
    -drive "file=$DISK,if=none,format=raw,id=nvme0" -device nvme,drive=nvme0,serial=K1K-NVME-0001 \
    "$@" >/dev/null 2>&1 &
QPID=$!

wait_for() {    # wait_for <fixed string> <seconds>
    i=0
    while [ "$i" -lt "$2" ]; do
        grep -qF -- "$1" "$LOG" 2>/dev/null && return 0
        sleep 1; i=$((i + 1))
    done
    return 1
}

key() { echo "sendkey $1" | socat - "UNIX-CONNECT:$MON" >/dev/null 2>&1; sleep 0.12; }
type_line() {   # type_line <text>: letters, digits, space, / . -
    text="$1"
    while [ -n "$text" ]; do
        c="${text%"${text#?}"}"; text="${text#?}"
        case "$c" in
            ' ') key spc ;; '/') key slash ;; '.') key dot ;; '-') key minus ;;
            *) key "$c" ;;
        esac
    done
    key ret
}

wait_for 'k1k> ' 60 || fail "the shell never showed a prompt"
echo "ok: the shell is up and shows its prompt"

type_line "echo shell-works"
wait_for 'shell-works' 15 || fail "echo"
echo "ok: echo"

type_line "cat /README.TXT"
wait_for 'K1K disk image v1' 15 || fail "cat /README.TXT did not print the file"
echo "ok: cat reads a file through the file service"

type_line "ls /SVC"
wait_for 'SH.ELF' 15 || fail "ls /SVC did not list SH.ELF"
grep -qF 'MANIFEST.TXT' "$LOG" || fail "ls /SVC did not list MANIFEST.TXT"
echo "ok: ls lists a directory"

type_line "uptime"
wait_for 'up ' 15 || fail "uptime"
type_line "nosuchthing"
wait_for 'nosuchthing: not a built-in command' 15 || fail "an unknown command was not reported"
echo "ok: uptime, and an unknown command is reported"
echo "shell-test: all passed"
