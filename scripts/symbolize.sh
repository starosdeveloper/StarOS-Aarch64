#!/usr/bin/env bash
# Turn the addresses in a `[fault] backtrace` line back into function names.
#
# The kernel prints a backtrace by walking the EL0 task's `x29` chain, which is all
# it can do: it has no symbol table, and giving it one would mean carrying every
# program's symbols in the kernel image. The names live on the *host*, in the
# unstripped `<name>.debug.elf` that `build.rs` keeps beside each embedded image.
#
# Usage:
#   scripts/symbolize.sh fsclient 0x80000014 0x800016c8
#   printf '\n' | cargo krun 2>&1 | scripts/symbolize.sh fsclient
#
# The first argument names the program (or is a path to an ELF). With no addresses
# after it, the script reads stdin and symbolizes every backtrace line it finds —
# so a whole boot log can be piped through it.
#
# This is the only debugging tool in the tree that survives the move to real
# hardware: gdbstub is a QEMU feature, an `x29` chain is not.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ $# -lt 1 ]; then
    echo "usage: symbolize.sh <program|elf-path> [address...]" >&2
    exit 2
fi

target="$1"
shift

# An explicit path wins; otherwise look for the unstripped copy the build left in
# OUT_DIR. Newest first, because a stale one from an earlier build would resolve
# addresses to plausible, wrong names — the worst possible failure for this tool.
if [ -f "$target" ]; then
    elf="$target"
else
    elf="$(ls -t "$here"/target/*/debug/build/kernel-*/out/"$target".debug.elf 2>/dev/null | head -1 || true)"
fi
if [ -z "${elf:-}" ] || [ ! -f "$elf" ]; then
    echo "symbolize: no unstripped ELF for '$target'" >&2
    echo "  (build first: cargo kbuild; services live in target/*/debug/build/kernel-*/out/)" >&2
    exit 1
fi

# llvm-nm ships with the Rust toolchain, so there is nothing extra to install.
nm="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-nm 2>/dev/null | head -1 || true)"
if [ -z "$nm" ]; then
    nm="$(command -v llvm-nm || command -v nm || true)"
fi
if [ -z "$nm" ]; then
    echo "symbolize: no llvm-nm found (try: rustup component add llvm-tools)" >&2
    exit 1
fi

# `--demangle` turns Rust's v0 mangling back into `crate::module::function`; without
# it every frame reads like `_RNvCs29BnM5DK1my_15staros_fsclient13touch_archive`,
# which is a name only in the technical sense. A demangled name may contain spaces
# (generics), so everything after the type letter is kept as one field.
syms="$("$nm" --numeric-sort --defined-only --demangle "$elf" 2>/dev/null |
    awk 'NF>=3 { addr=$1; $1=""; $2=""; sub(/^ +/, ""); print addr, $0 }')"
if [ -z "$syms" ]; then
    echo "symbolize: $elf has no symbol table — is this the stripped copy?" >&2
    exit 1
fi

# Map addresses to `symbol+offset` by finding the last symbol at or below each one.
# A symbol table gives no sizes here, so an address past the last function still
# resolves to it; that is why the offset is always printed rather than hidden.
resolve() {
    awk -v addrs="$*" '
        NR == FNR { a[NR] = strtonum("0x" $1); name = $0; sub(/^[^ ]+ /, "", name); n[NR] = name; count = NR; next }
        END {
            split(addrs, want, " ")
            for (i = 1; i in want; i++) {
                target = strtonum(want[i])
                best = ""; bestaddr = 0
                for (j = 1; j <= count; j++) {
                    if (a[j] <= target && a[j] >= bestaddr) { bestaddr = a[j]; best = n[j] }
                }
                if (best == "")
                    printf "  %s  <no symbol below this address>\n", want[i]
                else
                    printf "  %s  %s+0x%x\n", want[i], best, target - bestaddr
            }
        }
    ' <(printf '%s\n' "$syms") /dev/null
}

echo "symbols from: $elf"

if [ $# -gt 0 ]; then
    resolve "$@"
    exit 0
fi

# No addresses given: read a log and symbolize each backtrace line in it.
#
# Every backtrace in the log is resolved against the *one* ELF named on the command
# line, and the kernel's report says which task faulted, not which program. All EL0
# programs are linked at the same `USER_BASE`, so a backtrace from another program
# resolves to real-looking, wrong names — the fault line is echoed above each block
# so the mismatch is at least visible.
echo "  (all EL0 programs link at USER_BASE — a backtrace from another program will resolve to wrong names here)"
found=0
fault=""
while IFS= read -r line; do
    case "$line" in
        *"[fault] task"*"killed"*) fault="$line" ;;
    esac
    case "$line" in
        *"backtrace"*"):"*)
            [ -n "$fault" ] && echo "${fault#*"[fault]"}"
            echo "${line#*"[fault]"}"
            # shellcheck disable=SC2086  # deliberate word splitting: one arg per address
            resolve ${line##*): }
            found=1
            ;;
    esac
done
if [ "$found" -eq 0 ]; then
    echo "  (no backtrace lines on stdin)"
fi
