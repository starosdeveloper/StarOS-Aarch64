#!/usr/bin/env bash
# Every function this sysroot declares must be one the C library defines.
#
# `crates/staros-libc/include/features.h` states the rule and gives the reason: a
# header that promises a function nobody wrote moves the failure from the link,
# where it names the symbol, to somewhere later and less obvious. It was written
# down and then not checked, and by the time anyone looked, `math.h` was declaring
# `erf`, `lgamma`, `lround`, `ilogb` and the whole `long double` family against an
# archive that defined none of them.
#
# `scripts/libc-progress.sh` did not catch it and could not: it scores the archive
# against the symbols a real Qt *link* leaves undefined, and a header is a different
# demand. Qt's own sources include <cmath>, and libstdc++'s <cmath> does `using
# ::erf;` — which fails at **compile** time, in a file that has nothing to do with
# the error. Two demands, two measurements.
#
# Usage: header-check.sh [--list]   (--list prints every declared name and verdict)
set -uo pipefail
cd "$(dirname "$0")/.."

LIST=0
[ "${1:-}" = "--list" ] && LIST=1

INCLUDE="crates/staros-libc/include"
# The *newest* archive, not the first one found. Cargo leaves an `out` directory
# per build-script fingerprint, so an old one survives beside the current one — and
# checking that told a very confident story about symbols that had just been added.
ARCHIVE="$(find target -name libstaros_libc.a -printf '%T@ %p\n' 2>/dev/null |
    sort -rn | head -1 | cut -d' ' -f2-)"
if [ -z "$ARCHIVE" ]; then
    echo "header-check: no libstaros_libc.a; run cargo kbuild first" >&2
    exit 2
fi

nm_tool="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-nm 2>/dev/null | head -1 || true)"
[ -z "$nm_tool" ] && nm_tool="$(command -v llvm-nm || command -v nm || true)"
if [ -z "$nm_tool" ]; then
    echo "header-check: no nm (rustup component add llvm-tools)" >&2
    exit 2
fi

# Symbols the archive defines, split by how.
#
# `T`/`D`/`B`/`R` are definitions this tree wrote. `W` and `V` are **weak**, and in
# this archive they are almost all `compiler_builtins`, which bundles a complete
# libm — `sin`, `pow`, `memcpy`, `erf` — and exports every one of it weakly. A
# strong definition beside a weak one wins, so the functions written here are the
# ones that run; the weak set is a floor underneath.
#
# That floor is why `erf` and `tgamma` looked implemented when nothing here had
# written them. Counting the two together would let this check report a promise as
# kept because a library nobody chose happened to keep it — so they are counted
# apart, and a name that only the fallback defines is reported as such.
DEFINED="$(mktemp)"
WEAK="$(mktemp)"
trap 'rm -f "$DEFINED" "$WEAK"' EXIT
"$nm_tool" -g "$ARCHIVE" 2>/dev/null | awk '$2 ~ /^[TDBR]$/ {print $3}' | sort -u >"$DEFINED"
"$nm_tool" -g "$ARCHIVE" 2>/dev/null | awk '$2 ~ /^[WV]$/ {print $3}' | sort -u >"$WEAK"

# Function names these headers declare.
#
# A parser rather than a compiler, and that is a real limitation: it reads lines
# that look like `type name(args);` and will miss a declaration split across lines
# or hidden behind a macro. It is not trying to be a C front end — it is trying to
# make one class of mistake impossible to leave in, and a check that catches most of
# a thing is worth more than an argument about the rest.
DECLARED="$(mktemp)"
trap 'rm -f "$DEFINED" "$WEAK" "$DECLARED"' EXIT
# `typedef` lines are dropped first. A function-pointer typedef —
# `typedef void (*handler)(int);` — has its first parenthesis after the *return*
# type, so the extraction below reads the name as `void` and then reports that the
# library fails to define it. Filtering the line is more honest than adding `void`
# to the keyword list, which would hide the next one of these.
grep -hvE '^[[:space:]]*typedef' "$INCLUDE"/*.h "$INCLUDE"/sys/*.h |
    grep -oE '^[a-z_][a-zA-Z0-9_ *]*\**[[:space:]]+\**([a-zA-Z_][a-zA-Z0-9_]*)[[:space:]]*\(' |
    grep -oE '[a-zA-Z_][a-zA-Z0-9_]*[[:space:]]*\($' |
    tr -d ' (' |
    grep -vE '^(if|for|while|switch|return|sizeof|defined|typedef|struct|union|enum)$' |
    sort -u >"$DECLARED"

# The names already known to be declared and undefined. A ratchet, not a waiver:
# the list may shrink freely, and it may only grow by someone editing the file,
# which puts a reason next to it in a diff.
KNOWN="docs/header-gap.txt"
ALLOWED="$(mktemp)"
trap 'rm -f "$DEFINED" "$WEAK" "$DECLARED" "$ALLOWED"' EXIT
grep -vE '^\s*(#|$)' "$KNOWN" 2>/dev/null | sort -u >"$ALLOWED"

missing=0
unexpected=0
fixed=0
weak=0
total=0
while read -r name; do
    [ -z "$name" ] && continue
    total=$((total + 1))
    if grep -qxF "$name" "$DEFINED"; then
        [ "$LIST" = 1 ] && echo "  ok       $name"
    elif grep -qxF "$name" "$WEAK"; then
        weak=$((weak + 1))
        [ "$LIST" = 1 ] && echo "  fallback $name — only compiler_builtins defines this"
    elif grep -qxF "$name" "$ALLOWED"; then
        missing=$((missing + 1))
        [ "$LIST" = 1 ] && echo "  known    $name"
    else
        missing=$((missing + 1))
        unexpected=$((unexpected + 1))
        echo "  NEW GAP  $name — declared here, defined nowhere"
    fi
done <"$DECLARED"

# Names on the list that are now defined: progress, and the list should lose them
# so it keeps meaning what it says.
while read -r name; do
    [ -z "$name" ] && continue
    if grep -qxF "$name" "$DEFINED"; then
        fixed=$((fixed + 1))
        echo "  CLOSED   $name — now defined; remove it from $KNOWN"
    fi
done <"$ALLOWED"

echo
echo "  headers declare $total function(s)"
echo "  $((total - missing - weak)) defined here, $weak by compiler_builtins' weak libm,"
echo "  $missing declared and undefined (see $KNOWN)"
if [ "$unexpected" -gt 0 ] || [ "$fixed" -gt 0 ]; then
    echo
    [ "$unexpected" -gt 0 ] && echo "  $unexpected promise(s) added without being kept"
    [ "$fixed" -gt 0 ] && echo "  $fixed name(s) kept but still listed as missing"
    exit 1
fi
if [ "$missing" -eq 0 ]; then
    echo "  every declared function is defined"
fi
exit 0
