#!/usr/bin/env bash
# What Qt needs from the C++ runtime, and how much of it exists here.
#
# The third measurement. `libc-progress.sh` scores the C library against what a Qt
# *link* leaves undefined; `header-check.sh` scores the sysroot against what a Qt
# *compile* demands. Neither sees the C++ runtime, because it is not libc: the
# templates come from libstdc++'s headers and compile into the caller, but a
# compiled half remains — `operator new`, the `__cxa_*` ABI, the out-of-line pieces
# of `std::map`, `std::list` and `std::unordered_map`, `std::condition_variable`,
# `std::chrono`.
#
# `crates/staros-libc/cxx/runtime.cpp` is that half. Its contents were never
# designed: the linker named every symbol in it, one at a time, while
# `services/hello-cpp` was made to run. This script asks the same question ahead of
# time, of Qt rather than of a demo — the symbols Qt leaves undefined that
# libstdc++ defines, minus the ones this tree already provides.
#
# ## The number that matters is not the total
#
# The distribution's Qt is built *with* exceptions and RTTI, and this system builds
# without both — so part of the demand is `__cxa_throw`, `__gxx_personality_v0`,
# `__dynamic_cast` and the type-info vtables. That group is counted apart rather than
# filtered out, because "will disappear when the flag is set" is a prediction, and
# the honest place for a prediction is beside the measurement rather than inside it.
#
# ## The prediction was tested, and it was half wrong
#
# Linking `services/qt-hello` against the Qt this tree actually cross-builds — with
# `-fno-exceptions -fno-rtti` — named eight of this family as undefined:
# `__cxa_throw`, `__cxa_allocate_exception`, `__cxa_begin_catch`, `__cxa_end_catch`,
# `__cxa_rethrow`, `__cxa_current_exception_type`, `__gxx_personality_v0` and
# `_Unwind_Resume`, plus the vtables of `__class_type_info` and
# `__si_class_type_info`.
#
# The flag does not remove them, and the reason is that it was never going to: it
# governs what *this* build's compiler emits from source, and these come out of
# libstdc++'s headers, which contain `throw` in inline and template code regardless
# of how the translation unit including them is compiled. All eight are now written
# in `crates/staros-libc/cxx/runtime.cpp`, and every one of them stops the program
# with a message rather than unwinding.
#
# What the flag *did* remove is the eleven still listed below. So the group is real
# and the split within it was not visible from here — which is the argument for
# printing the whole list under `--missing` rather than only the part outside the
# filter. The first version of this script printed only the remainder, and the eight
# names above never appeared in its output at all.
#
# Usage: cxx-progress.sh [--missing]
set -uo pipefail
cd "$(dirname "$0")/.."

MISSING=0
[ "${1:-}" = "--missing" ] && MISSING=1

QT_LIB=/usr/lib
STDCXX="$QT_LIB/libstdc++.so.6"
[ -f "$STDCXX" ] || { echo "cxx-progress: no libstdc++ here — skipping"; exit 0; }
ls "$QT_LIB"/libQt6Core.so >/dev/null 2>&1 || {
    echo "cxx-progress: no Qt 6 libraries here — skipping"
    exit 0
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# What Qt leaves undefined, without the glibc version decoration — `abort@GLIBC_2.2.5`
# and `abort` are the same demand, and comparing them undecorated is the difference
# between 80 matches and none.
for module in Core Gui Qml Quick; do
    nm -u -D "$QT_LIB/libQt6$module.so" 2>/dev/null
done | awk '{print $2}' | cut -d@ -f1 | sort -u >"$WORK/undefined"

nm -D --defined-only "$STDCXX" 2>/dev/null | awk '{print $3}' | cut -d@ -f1 |
    sort -u >"$WORK/stdcxx"
comm -12 "$WORK/undefined" "$WORK/stdcxx" >"$WORK/demand"

# What this tree defines. Two places: the C library archive, and the C++ runtime
# object beside it — `operator new` is in the second and nothing in the first.
{
    ARCHIVE="$(find target -name libstaros_libc.a -printf '%T@ %p\n' 2>/dev/null |
        sort -rn | head -1 | cut -d' ' -f2-)"
    [ -n "$ARCHIVE" ] && nm -g "$ARCHIVE" 2>/dev/null
    RUNTIME="$(find target -name cxx-runtime.o -printf '%T@ %p\n' 2>/dev/null |
        sort -rn | head -1 | cut -d' ' -f2-)"
    [ -n "$RUNTIME" ] && nm -g "$RUNTIME" 2>/dev/null
} | awk '$2 ~ /^[TDBRWV]$/ {print $3}' | sort -u >"$WORK/have"

if [ ! -s "$WORK/have" ]; then
    echo "cxx-progress: nothing built yet; run cargo kbuild first" >&2
    exit 2
fi

comm -12 "$WORK/demand" "$WORK/have" >"$WORK/present"
comm -23 "$WORK/demand" "$WORK/have" >"$WORK/absent"

# The exception and RTTI machinery, which `-fno-exceptions -fno-rtti` removes from
# the demand entirely. Matched by name because that is what it is: the Itanium C++
# ABI's throw path, the personality routine, the type-info class vtables.
EXCEPTIONS='cxa_(throw|begin_catch|end_catch|rethrow|allocate_exception|current_exception|call_terminate)|gxx_personality|dynamic_cast|exception_ptr|_ZTI|_ZTVN10__cxxabi|bad_alloc|_ZNSt9exception'
grep -cE "$EXCEPTIONS" "$WORK/absent" >"$WORK/exc_count" || echo 0 >"$WORK/exc_count"
exc="$(cat "$WORK/exc_count")"

demand=$(wc -l <"$WORK/demand")
present=$(wc -l <"$WORK/present")
absent=$(wc -l <"$WORK/absent")
real=$((absent - exc))

echo
echo "  Qt leaves $demand C++ runtime symbol(s) to libstdc++"
echo "  $present provided here, $absent absent"
echo "  of the absent, $exc belong to exceptions and RTTI, and are absent from the"
echo "  cross-built Qt's own link — measured, not predicted; see the note at the"
echo "  top for the eight of this family that -fno-exceptions did *not* remove"
echo
echo "  $real actually to write"

if [ "$MISSING" = 1 ]; then
    # The whole list, exception machinery included. Printing only the remainder is
    # what hid the eight names described at the top of this file: they were inside
    # the filter, so no run of this script ever showed them, and they surfaced as
    # link errors instead.
    echo
    echo "  absent, all of it:"
    if command -v c++filt >/dev/null 2>&1; then
        c++filt <"$WORK/absent" | sed 's/^/    /'
    else
        sed 's/^/    /' "$WORK/absent"
    fi
fi
exit 0
