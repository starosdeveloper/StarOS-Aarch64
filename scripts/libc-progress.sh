#!/usr/bin/env bash
# Score `crates/staros-libc` against the contract in docs/libc-contract.txt.
#
# The roadmap's warning about G5 is that it is the phase which can become infinite,
# and the antidote is to measure rather than to feel. This is the measurement: what
# fraction of the symbols a real Qt build needs does this library actually define,
# broken down by the layers the roadmap implements in order.
#
# It reads the archive the kernel's `build.rs` produced, so it scores what was
# built, not what somebody remembers writing.
#
# Usage: scripts/libc-progress.sh [--missing LAYER]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
contract="$here/docs/libc-contract.txt"
[ -f "$contract" ] || { echo "no contract: run scripts/libc-contract.sh" >&2; exit 1; }

show_missing=""
if [ "${1:-}" = "--missing" ]; then
    show_missing="${2:-}"
    [ -n "$show_missing" ] || { echo "usage: libc-progress.sh --missing <layer>" >&2; exit 2; }
fi

archive="$(ls -t "$here"/target/*/debug/build/kernel-*/out/libstaros_libc.a 2>/dev/null | head -1 || true)"
[ -n "$archive" ] || { echo "no libstaros_libc.a — run cargo kbuild first" >&2; exit 1; }

nm="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-nm 2>/dev/null | head -1)"
[ -n "$nm" ] || { echo "no llvm-nm (rustup component add llvm-tools)" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Defined, externally visible symbols in the archive. `T` is code, `D`/`B` data and
# `R` read-only data — `stdout` is a symbol too, and a contract that only counted
# functions would score it as missing forever.
"$nm" --defined-only --no-sort "$archive" 2>/dev/null |
    awk '$2 ~ /^[TDBR]$/ { print $3 }' | sort -u >"$tmp/have"

grep -v '^#' "$contract" | awk 'NF == 2 { print $1, $2 }' | sort -u >"$tmp/want"

# Layer names, in the roadmap's order.
name_of() {
    case "$1" in
        1) echo "pure computation (str*, mem*, printf, math)" ;;
        2) echo "memory (malloc over MapAnon)" ;;
        3) echo "time (clock_gettime, nanosleep)" ;;
        4) echo "files and I/O (over fssrv)" ;;
        5) echo "threads and TLS (over SpawnThread)" ;;
        6) echo "multiplexing (poll, eventfd, pipe)" ;;
        7) echo "process and system" ;;
        *) echo "layer $1" ;;
    esac
}

echo "contract: $contract"
echo "archive:  $archive"
echo

total_have=0
total_want=0
for layer in 1 2 3 4 5 6 7; do
    awk -v l="$layer" '$1 == l { print $2 }' "$tmp/want" | sort -u >"$tmp/layer"
    want=$(wc -l <"$tmp/layer")
    [ "$want" -eq 0 ] && continue
    have=$(comm -12 "$tmp/layer" "$tmp/have" | wc -l)
    total_have=$((total_have + have))
    total_want=$((total_want + want))
    printf '  layer %s  %3d/%-3d  %s\n' "$layer" "$have" "$want" "$(name_of "$layer")"
    if [ "$show_missing" = "$layer" ]; then
        comm -23 "$tmp/layer" "$tmp/have" | sed 's/^/      missing: /'
    fi
done

echo
printf '  total     %3d/%-3d  (%d%%)\n' "$total_have" "$total_want" \
    "$((total_have * 100 / total_want))"
echo
echo "  A symbol counts here when the archive defines it, which is not the same as"
echo "  the call doing what its name suggests on another system: fork, dlopen and"
echo "  the System V IPC calls are present and refuse, each with the errno that says"
echo "  why. docs/LIBC-CONTRACT.md lists every one of those and its reason."
echo "  Run with --missing <layer> to list a layer's gaps."
