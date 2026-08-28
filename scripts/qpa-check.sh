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

# The meta-object compiler, and why this script has to run it.
#
# `main.cpp` ends with `#include "main.moc"` — the file `moc` generates from its own
# `Q_OBJECT`. Without it the translation unit does not exist at all, and this script
# quietly checked seven files out of eight while reporting on the plugin as a whole.
# That is the failure mode a compile check is supposed to prevent, not have.
#
# The generated sources are checked too, not just used. `moc_qstarosinput.cpp` is a
# real translation unit in the CMake build, it is written by a tool whose version is
# whatever the host has, and it is the one file in the plugin nobody reads before it
# fails. Generating into a temporary directory rather than the tree keeps the build's
# own artefacts out of a check that must not depend on a previous build.
MOC="$(command -v moc || true)"
[ -x "$MOC" ] || MOC="$(ls /usr/lib/qt6/moc /usr/lib64/qt6/moc 2>/dev/null | head -1)"
[ -x "$MOC" ] || { echo "qpa-check: no moc — skipping"; exit 0; }

MOC_DIR="$(mktemp -d /tmp/qpa-moc-XXXXXX)"
trap 'rm -rf "$MOC_DIR"' EXIT

# moc runs its own preprocessor, so it needs the Qt include path as well as the
# plugin's: `Q_PLUGIN_METADATA(IID QPlatformIntegrationFactoryInterface_iid ...)`
# names a macro that lives in `qpa/qplatformintegrationplugin.h`, and moc that cannot
# expand it stops at `Parse error at "IID"`. The plugin directory is what lets it
# find `staros.json`; a missing metadata file is an error there too, and a plugin
# built without its metadata loads nowhere.
MOC_INC=(
    -I "$PLUGIN"
    -I "$QT_INC"
    -I "$QT_INC/QtCore" -I "$QT_INC/QtCore/$QT_VER" -I "$QT_INC/QtCore/$QT_VER/QtCore"
    -I "$QT_INC/QtGui" -I "$QT_INC/QtGui/$QT_VER" -I "$QT_INC/QtGui/$QT_VER/QtGui"
)
if ! "$MOC" "${MOC_INC[@]}""$PLUGIN/main.cpp" -o "$MOC_DIR/main.moc" 2>"$MOC_DIR/err"; then
    echo "  ✗ moc main.cpp"
    sed 's/^/      /' "$MOC_DIR/err"
    echo "qpa-check: FAIL — moc did not run"
    exit 1
fi
if ! "$MOC" "${MOC_INC[@]}""$PLUGIN/qstarosinput.h" -o "$MOC_DIR/moc_qstarosinput.cpp" \
        2>"$MOC_DIR/err"; then
    echo "  ✗ moc qstarosinput.h"
    sed 's/^/      /' "$MOC_DIR/err"
    echo "qpa-check: FAIL — moc did not run"
    exit 1
fi
echo "  moc: main.moc, moc_qstarosinput.cpp"

fail=0
for src in "$PLUGIN"/*.cpp "$MOC_DIR"/moc_qstarosinput.cpp; do
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
        -I "$PLUGIN" -I "$PLUGIN/mkspec" -I "$MOC_DIR" \
        -fsyntax-only "$src" 2>&1)"
    if [ -n "$out" ]; then
        echo "  ✗ $(basename "$src")"
        printf '%s\n' "$out" | head -20 | sed 's/^/      /'
        fail=$((fail + 1))
    else
        echo "  ✓ $(basename "$src")"
    fi
done

# The QML half. He asked for a 100% QML interface, so the modules that matter are
# QtQml and QtQuick — including QV4, the JavaScript engine, and the *software*
# scene-graph adaptation, which is the renderer this system will use because there
# is no GPU driver and the build is `-no-opengl`.
#
# Nothing is compiled from the plugin here: these are Qt's own headers, and what is
# being asked is whether this sysroot can host them at all. It is the cheapest way
# to find a missing header — QV4 pulled in <cwctype>, which named nineteen
# functions nobody had written, and libstdc++'s locale machinery reached past
# ctype.h for glibc's classification table.
QML_PROBE="$MOC_DIR/qml-probe.cpp"
cat >"$QML_PROBE" <<'PROBE'
#include <QtGui/qguiapplication.h>
#include <QtQml/qqmlengine.h>
#include <QtQml/qqmlcomponent.h>
#include <QtQml/private/qv4engine_p.h>
#include <QtQml/private/qv4function_p.h>
#include <QtQuick/qquickwindow.h>
#include <QtQuick/private/qsgsoftwareadaptation_p.h>
#include <QtQuick/private/qsgabstractsoftwarerenderer_p.h>
PROBE

if [ -d "$QT_INC/QtQuick/$QT_VER" ]; then
    out="$(clang++ --target=aarch64-unknown-none -nostdlibinc -std=c++17 \
        -fno-exceptions -fno-rtti -fno-omit-frame-pointer -fno-stack-protector -fno-pie \
        -D__linux__=1 -D__unix__=1 -DQT_NO_OPENGL=1 -DQT_NO_EXCEPTIONS=1 \
        -ferror-limit=20 \
        -isystem "$CXX_INC" -isystem "$CXX_TGT" -isystem "$SYSROOT" \
        -isystem "$QT_INC" \
        -isystem "$QT_INC/QtCore" -isystem "$QT_INC/QtCore/$QT_VER" \
        -isystem "$QT_INC/QtCore/$QT_VER/QtCore" \
        -isystem "$QT_INC/QtGui" -isystem "$QT_INC/QtGui/$QT_VER" \
        -isystem "$QT_INC/QtGui/$QT_VER/QtGui" \
        -isystem "$QT_INC/QtQml" -isystem "$QT_INC/QtQml/$QT_VER" \
        -isystem "$QT_INC/QtQml/$QT_VER/QtQml" \
        -isystem "$QT_INC/QtQuick" -isystem "$QT_INC/QtQuick/$QT_VER" \
        -isystem "$QT_INC/QtQuick/$QT_VER/QtQuick" \
        -I "$PLUGIN/mkspec" \
        -fsyntax-only "$QML_PROBE" 2>&1)"
    if [ -n "$out" ]; then
        echo "  ✗ QtQml + QtQuick + QV4 + the software renderer"
        printf '%s\n' "$out" | head -20 | sed 's/^/      /'
        fail=$((fail + 1))
    else
        echo "  ✓ QtQml + QtQuick + QV4 + the software renderer"
    fi
else
    echo "  — no QtQuick headers here; the QML probe is skipped"
fi

echo
if [ "$fail" -gt 0 ]; then
    echo "qpa-check: FAIL — $fail translation unit(s) did not compile"
    exit 1
fi
echo "qpa-check: PASS — the plugin compiles for aarch64 against this tree's sysroot"
echo "  (compile only; linking needs a Qt cross-built against the same sysroot)"
exit 0
