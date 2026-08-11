#!/usr/bin/env bash
# Prove the debugger half of G5.0: a breakpoint in an EL0 program is hit, and the
# stack gdb unwinds there is the real one.
#
# QEMU's gdbstub needs no change in this tree — `scripts/qemu-run.sh` passes extra
# arguments through, so `cargo krun -- -s -S` already stops the machine and listens
# on :1234. What this script adds is that the claim is *checked* rather than written
# down in a document: it boots, breaks on a named function in `fsclient`, and
# asserts the backtrace names the callers.
#
# Two things make it work at all, and both are worth knowing before the first manual
# session:
#   * EL0 programs are linked `-no-pie` at USER_BASE, so the VAs in the ELF are the
#     VAs in memory: `add-symbol-file <prog>.debug.elf -o 0` needs no slide.
#   * gdbstub knows nothing about processes. It sees the current CPU and reads memory
#     through whatever page tables are live, so a breakpoint at a user address fires
#     in *every* process whose code happens to sit there. With one program under
#     test that is invisible; with two it is the first thing that will confuse you.
#     `monitor info registers` prints TTBR0_EL1, which is the address-space id.
#
# Usage: scripts/gdb-check.sh [program] [function] [gdb-condition]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

prog="${1:-fsclient}"
func="${2:-touch_archive}"
# A condition that tells this program apart from every other one stopped at the same
# address. It is needed because of the second caveat above: EL0 programs all link at
# USER_BASE, so `break *0x80000010` fires in whichever process reaches that address
# first — usually one that has only just started, whose stack then unwinds into the
# kernel and looks like a broken unwinder. `touch_archive` loads through x8, which
# holds the archive's address and nothing else in the system does; that makes it a
# usable identity check. A different program needs its own.
cond="${3:-\$x8 == 0x900000000}"
port=1234

R=$'\033[31m'; G=$'\033[32m'; Z=$'\033[0m'

command -v gdb >/dev/null || { echo "gdb-check: no gdb on this host" >&2; exit 1; }

echo "building…"
cargo kbuild >/dev/null 2>&1

elf="$(ls -t target/*/debug/build/kernel-*/out/"$prog".debug.elf 2>/dev/null | head -1 || true)"
[ -n "$elf" ] || { echo "gdb-check: no $prog.debug.elf — did the build run?" >&2; exit 1; }

nm="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-nm 2>/dev/null | head -1)"
# The address, not the name: gdb would have to demangle Rust's v0 symbols to find
# the function by name, and an address is what a fault report gives you anyway.
# `|| true` on both lookups: the `awk`s stop at the first match, which closes the
# pipe under the tool still writing to it. That SIGPIPE is a normal early exit, but
# `set -o pipefail` reports it as failure and `set -e` then kills the script before
# it prints anything — a silent exit 141 that looks exactly like a hang. The two
# emptiness checks below are what actually catch a failed lookup.
start="$("$nm" --defined-only --demangle "$elf" | awk -v f="$func" '$3 ~ f && $2 ~ /[tT]/ {print $1; exit}' || true)"
[ -n "$start" ] || { echo "gdb-check: no symbol matching '$func' in $elf" >&2; exit 1; }

# Where in the function to stop. Two constraints, and missing either one produces a
# result that looks like a broken debugger:
#   * after the prologue — at the entry instruction the frame record is not pushed
#     yet, so `x29` still belongs to the caller's caller and the unwind skips a frame;
#   * at an instruction where the identifying register is already loaded, since the
#     condition above is what tells this process from every other one stopped here.
# Both are satisfied by breaking on the first instruction matching `$insn` after the
# prologue — for the default program that is the load from the archive itself.
insn="${4:-ldrb}"
objdump="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objdump 2>/dev/null | head -1)"
[ -n "$objdump" ] || { echo "gdb-check: no llvm-objdump (rustup component add llvm-tools)" >&2; exit 1; }
addr="$("$objdump" -d "$elf" |
    awk -v s="$start" -v want="$insn" '
        $0 ~ "^0*" s " <" { inside = 1; next }
        inside && /mov[ \t]+x29, sp/ { armed = 1; next }
        inside && armed && $0 ~ want { sub(":", "", $1); print $1; exit }
        inside && /^$/ { inside = 0 }
    ' || true)"
[ -n "$addr" ] || {
    echo "gdb-check: no '$insn' after $func's prologue in $elf" >&2
    exit 1
}
echo "breakpoint: $prog!$func at 0x$addr (first '$insn' past the prologue; entry is 0x$start)"

log="$(mktemp -t gdb-check-XXXXXX.log)"
out="$(mktemp -t gdb-check-XXXXXX.out)"
trap 'rm -f "$log" "$out"; kill %1 2>/dev/null || true' EXIT

# -S stops before the first instruction, so the breakpoint is set long before the
# program that will hit it exists.
printf '\n' | cargo krun -- -s -S >"$log" 2>&1 &
sleep 3

gdb -q -batch \
    -ex "set architecture aarch64" \
    -ex "set confirm off" \
    -ex "set pagination off" \
    -ex "target remote :$port" \
    -ex "add-symbol-file $elf -o 0" \
    -ex "break *0x$addr if $cond" \
    -ex "continue" \
    -ex "backtrace" \
    -ex "info registers pc" \
    -ex "kill" \
    target/aarch64-unknown-none/debug/kernel >"$out" 2>&1 || true

kill %1 2>/dev/null || true

echo "─── gdb ───"
sed -n '/Breakpoint 1/,$p' "$out" | head -20

fail=0
grep -q "^Breakpoint 1, " "$out" || { echo "${R}✗ the breakpoint never fired${Z}"; fail=1; }
grep -q "$func" "$out" || { echo "${R}✗ the backtrace does not name $func${Z}"; fail=1; }
# The frame below it must belong to the *program*, not the kernel. That distinction
# is the whole assertion: with symbols but no call-frame information gdb still
# prints a `#1`, and it is the kernel's `user_task_entry` — a frame that exists, but
# is not the caller. Matching "a caller is present" would pass on that.
crate="$(basename "$elf" .debug.elf)"
grep -qE "^#1 .*staros_$crate" "$out" || {
    echo "${R}✗ frame #1 is not in $crate — gdb did not unwind the program's own stack${Z}"
    fail=1
}

if [ "$fail" -eq 0 ]; then
    echo "${G}gdb-check: PASS — breakpoint hit in EL0 and the stack unwound to its caller${Z}"
else
    echo "${R}gdb-check: FAIL${Z}"
    echo "  full gdb output: $out (kept)"
    trap 'kill %1 2>/dev/null || true' EXIT
fi
exit "$fail"
