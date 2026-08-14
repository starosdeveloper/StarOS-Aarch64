#!/usr/bin/env bash
# Compile the QPA plugin for aarch64 against this tree's own sysroot.
#
# Compile, not link. There is no Qt built for this target yet — that is a separate
# and much larger step — so what this proves is the half that can be proved now, and
# it is the half every port gets wrong: that Qt's headers, libstdc++'s headers and
# `crates/staros-libc/include` agree, and that the plugin's classes really do
# implement the interfaces Qt declares.
#
# It is worth having as a check rather than a one-off because it fails for reasons
# that arrive from outside: a Qt update changes a pure virtual's signature, a header
# starts including something this sysroot does not have. Both are cheap to see here
# and expensive to see during a cross build.
#
# `-D__linux__` deserves its own sentence. Qt decides what OS it is on from compiler
# macros, and `aarch64-unknown-none` sets none, so `qsystemdetection.h` stops with
# "Qt has not been ported to this OS". Saying Linux is not a disguise: this system
# deliberately presents a Linux ABI — glibc's `struct stat`, `dirent`, `sigset_t`,
# errno numbers, `poll`/`eventfd`/`pipe` — and `crates/staros-libc` was built against
# a measured list of what a real Qt link needs from glibc. Where a Linux *service* is
# absent the call refuses with the errno that names the reason. The alternative is a
# `Q_OS_STAROS` patched into Qt, which is the right answer for a port that upstreams
# and a large detour for one that has not built Qt yet.
#
# Usage: qpa-check.sh
set -uo pipefail
cd "$(dirname "$0")/.."

PLUGIN="services/qstaros"
SYSROOT="crates/staros-libc/include"

command -v clang++ >/dev/null || { echo "qpa-check: no clang++"; exit 2; }

QT_INC="/usr/include/qt6"
[ -d "$QT_INC" ] || { echo "qpa-check: no Qt 6 headers at $QT_INC — skipping"; exit 0; }
QT_VER="$(ls -d "$QT_INC"/QtCore/6.* 2>/dev/null | head -1 | xargs -r basename)"
[ -z "$QT_VER" ] && { echo "qpa-check: no Qt private headers — skipping"; exit 0; }

CXX_INC="$(ls -d /usr/include/c++/*/ 2>/dev/null | head -1)"
CXX_TGT="$(ls -d /usr/include/c++/*/x86_64*/ /usr/include/c++/*/aarch64*/ 2>/dev/null | head -1)"
[ -z "$CXX_INC" ] && { echo "qpa-check: no C++ standard headers — skipping"; exit 0; }

echo "  Qt $QT_VER, libstdc++ from $CXX_INC"

fail=0
for src in "$PLUGIN"/*.cpp; do
    out="$(clang++ --target=aarch64-unknown-none -nostdlibinc -std=c++17 \
        -fno-exceptions -fno-rtti -fno-omit-frame-pointer -fno-stack-protector -fno-pie \
        -D__linux__=1 -D__unix__=1 -DQT_NO_OPENGL=1 -DQT_NO_EXCEPTIONS=1 \
        -Wall -Wextra -ferror-limit=20 \
        -isystem "$CXX_INC" -isystem "$CXX_TGT" -isystem "$SYSROOT" \
        -isystem "$QT_INC" \
        -isystem "$QT_INC/QtCore" -isystem "$QT_INC/QtCore/$QT_VER" \
        -isystem "$QT_INC/QtCore/$QT_VER/QtCore" \
        -isystem "$QT_INC/QtGui" -isystem "$QT_INC/QtGui/$QT_VER" \
        -isystem "$QT_INC/QtGui/$QT_VER/QtGui" \
        -I "$PLUGIN" \
        -fsyntax-only "$src" 2>&1)"
    if [ -n "$out" ]; then
        echo "  ✗ $(basename "$src")"
        printf '%s\n' "$out" | head -20 | sed 's/^/      /'
        fail=$((fail + 1))
    else
        echo "  ✓ $(basename "$src")"
    fi
done

echo
if [ "$fail" -gt 0 ]; then
    echo "qpa-check: FAIL — $fail translation unit(s) did not compile"
    exit 1
fi
echo "qpa-check: PASS — the plugin compiles for aarch64 against this tree's sysroot"
echo "  (compile only; linking needs a Qt cross-built against the same sysroot)"
exit 0
