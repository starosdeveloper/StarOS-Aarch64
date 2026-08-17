#!/usr/bin/env bash
#
# Link the Qt programs in `services/` against the cross-built Qt, and report what is
# missing. There are two of them: `qt-hello`, which is QtGui and a `QPainter`, and
# `shell`, which adds QtQml and QtQuick on top of it.
#
# This is separate from `crates/kernel/build.rs` on purpose, and the reason is where
# Qt lives: outside this repository, in a build directory whose path is a property of
# whoever built it. Wiring that into the kernel's build would make `cargo kbuild`
# fail on a machine that has not built Qt — which is every machine, the first time.
# So this script is the step you run once Qt is built, and `build.rs` picks the
# images up afterwards if they are there.
#
# It exists to be *read* as much as run. The link lines below are the whole argument
# about how a Qt program is put together on a system with no dynamic loader: every
# archive named explicitly, in dependency order, with the C library last.
#
# `shell` may be skipped on its own: qtdeclarative is a second build and a machine
# that has qtbase but not qtdeclarative is an ordinary intermediate state. That is
# reported and the script still exits zero, because `qt-hello` linking is not made
# false by qtdeclarative being absent.

set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The `-rtti` in these names is not a variant. It is the only build there is, and the
# suffix records why the directories were made again: `-fno-rtti` was the original
# decision for the whole C++ side, and Qt Quick's software renderer cannot identify a
# scene-graph node without `dynamic_cast`. Both trees were reconfigured and rebuilt
# for it. See `services/qstaros/staros-toolchain.cmake`, which is where the flag
# actually changed, and `crates/staros-libc/cxx/runtime.cpp`, which is where
# `__dynamic_cast` had to be written to make the flag mean anything.
QT_BUILD="${QT_BUILD:-$HOME/qt-src/build-qtbase-rtti}"
QT_DECL="${QT_DECL:-$HOME/qt-src/build-qtdeclarative-rtti}"

if [ ! -d "$QT_BUILD/lib" ]; then
    echo "no Qt build at $QT_BUILD"
    echo "set QT_BUILD, or build qtbase first — see services/qstaros/README.md"
    exit 2
fi

# The C++ headers, the same pair the toolchain file finds. Taken from the host's
# libstdc++: only the templates are used, and the compiled half is
# `crates/staros-libc/cxx/runtime.cpp`.
cxx_include="$(ls -d /usr/include/c++/* 2>/dev/null | head -1)"
cxx_target_include="$(ls -d "$cxx_include"/*-linux-gnu 2>/dev/null | head -1)"
if [ -z "$cxx_include" ] || [ -z "$cxx_target_include" ]; then
    echo "the C++ standard headers were not found under /usr/include/c++"
    exit 2
fi

sysroot="$root/crates/staros-libc/include"
libc="$(ls -t "$root"/target/aarch64-unknown-none/debug/build/*/out/libstaros_libc.a 2>/dev/null | head -1)"
runtime="$(ls -t "$root"/target/aarch64-unknown-none/debug/build/*/out/cxx-runtime.o 2>/dev/null | head -1)"
if [ -z "$libc" ] || [ -z "$runtime" ]; then
    echo "libstaros_libc.a or cxx-runtime.o is missing — run \`cargo kbuild\` first"
    exit 2
fi

echo "  qtbase          $QT_BUILD"
if [ -d "$QT_DECL/lib" ]; then
    echo "  qtdeclarative   $QT_DECL"
else
    echo "  qtdeclarative   absent — the QML program will be skipped"
fi
echo "  libc            $libc"
echo "  runtime         $runtime"
echo

# ------------------------------------------------------------------ compile flags
#
# The same flags the plugin itself was compiled with, because the programs and the
# plugin share Qt's headers and a difference in `-fno-exceptions` or `-fno-rtti`
# changes what those headers declare. `QT_STATICPLUGIN` is what makes
# `Q_IMPORT_PLUGIN` expand to a reference to the registration function rather than to
# nothing.
cxxflags=(
    --target=aarch64-unknown-none -nostdlibinc -std=c++17
    -fno-exceptions -fno-rtti -fno-omit-frame-pointer
    -fno-stack-protector -fno-pie -O1 -g
    -DQT_STATICPLUGIN -DQT_NO_EXCEPTIONS
    -D__linux__=1 -D__unix__=1 -DQT_LINUXBASE=1
    -isystem "$cxx_include" -isystem "$cxx_target_include" -isystem "$sysroot"
    -isystem "$QT_BUILD/include"
    -isystem "$QT_BUILD/include/QtCore"
    -isystem "$QT_BUILD/include/QtGui"
)

# The qtbase half of the link line, which both programs need and neither varies.
#
# Order is the argument. A static archive contributes only what something already
# undefined needs, so each archive must come after everything that refers to it:
#
#   the program           refers to Qt and to the plugin
#   the platform plugin   refers to QtGui and QtCore
#   QtGui                 refers to QtCore, FreeType, HarfBuzz, libpng, libjpeg
#   FreeType and HarfBuzz refer to *each other*, which is why they are repeated
#   QtCore                refers to PCRE2, zlib and the C library
#
# The C++ runtime object goes ahead of the C library and both go last. Getting this
# wrong produces `undefined symbol` for a name that is plainly defined in an archive
# further left on the same command line.
qtbase_libs=(
    "$QT_BUILD/plugins/platforms/libqstaros.a"
    "$QT_BUILD/lib/libQt6Gui.a"
    "$QT_BUILD/lib/libQt6BundledFreetype.a"
    "$QT_BUILD/lib/libQt6BundledHarfbuzz.a"
    "$QT_BUILD/lib/libQt6BundledFreetype.a"
    "$QT_BUILD/lib/libQt6BundledLibpng.a"
    "$QT_BUILD/lib/libQt6BundledLibjpeg.a"
    "$QT_BUILD/lib/libQt6Core.a"
    "$QT_BUILD/lib/libQt6BundledPcre2.a"
    "$QT_BUILD/lib/libQt6BundledZLIB.a"
)

# ------------------------------------------------------------------------ linking
#
# `link_program <name> <object> <archive>...`
#
# Everything after the object is placed between it and the C runtime, so a caller
# supplies exactly the archives its program adds.
link_program() {
    local name="$1"; shift
    local object="$1"; shift
    local out; out="$(dirname "$object")"

    echo "linking $name"
    # `-z separate-loadable-segments` is not tidiness. Without it lld packs segments
    # that share a page: this program has three `PT_LOAD`s — read-only, read+execute,
    # read+write — and the second landed at `0x80024930`, inside the first one's last
    # page. The kernel's `map_segment` requires a page-aligned `vaddr` and refuses
    # anything else, so the whole image failed to load with no message beyond
    # "building its address space failed".
    #
    # Every other program in this tree has two segments that happen to fall on page
    # boundaries anyway, which is why nothing had ever hit this. It costs at most two
    # pages of padding.
    ld.lld -m aarch64linux -static \
        "-T$root/services/init/boot/image.ld" \
        -z max-page-size=4096 -z norelro -z separate-loadable-segments \
        --gc-sections \
        -o "$out/$name.debug.elf" \
        "$object" "$@" "$runtime" "$libc" 2>"$out/link.log"
    local status=$?

    if [ $status -ne 0 ]; then
        echo
        echo "the link failed. Distinct undefined symbols, demangled:"
        echo
        grep -oE "undefined symbol: .*" "$out/link.log" |
            sed 's/undefined symbol: //' | sort -u | c++filt | sed 's/^/  /'
        echo
        echo "  $(grep -c 'undefined symbol' "$out/link.log") reference(s), $(grep -oE 'undefined symbol: .*' "$out/link.log" | sort -u | wc -l) distinct"
        echo "  full log: $out/link.log"
        return 1
    fi

    # Every loadable segment must start on a page boundary, and this is checked here
    # rather than discovered at boot.
    #
    # The kernel's `map_segment` refuses a `vaddr` that is not a multiple of 4096, and
    # the whole image then fails to load — which the kernel reports as one line about
    # an address space, naming no section and no address. That is a long way from the
    # cause.
    #
    # The cause is orphan placement: a section `services/init/boot/image.ld` does not
    # mention gets placed by lld according to its attributes, and a read-only one
    # lands ahead of `.text` as a segment of its own. The section list below is what
    # turns "the Qt program never appeared" into a name.
    local bad=0 vaddr
    while read -r vaddr; do
        [ -z "$vaddr" ] && continue
        if [ $((vaddr % 4096)) -ne 0 ]; then
            bad=1
            printf 'PT_LOAD at 0x%x is not page-aligned\n' "$vaddr"
        fi
    done <<EOF
$(readelf -lW "$out/$name.debug.elf" 2>/dev/null |
    awk '$1 == "LOAD" { print strtonum($3) }')
EOF

    if [ "$bad" = 1 ]; then
        echo
        echo "  The kernel's ELF loader requires page-aligned segments and will refuse"
        echo "  this image. It is caused by an orphan section — one that image.ld does"
        echo "  not name — being given a PT_LOAD of its own. Sections by segment:"
        echo
        readelf -lW "$out/$name.debug.elf" 2>/dev/null |
            sed -n '/Section to Segment/,$p' | head -8 | cut -c1-200 | sed 's/^/    /'
        echo
        echo "  Add the offending section to the .text output section in"
        echo "  services/init/boot/image.ld, or to /DISCARD/ if nothing reads it."
        return 1
    fi

    # Strip, because the debug copy is not what would be loaded. `llvm-objcopy` comes
    # with the Rust toolchain — the same one `crates/kernel/build.rs` uses for every
    # other image in this tree — rather than from a separate LLVM install that may
    # not be here.
    local objcopy
    objcopy="$(ls -d "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1)"
    if [ -z "$objcopy" ]; then
        echo "llvm-objcopy was not found in the Rust toolchain; leaving the image unstripped"
        return 0
    fi
    "$objcopy" --strip-all "$out/$name.debug.elf" "$out/$name.elf"

    local debug_size size
    debug_size="$(stat -c%s "$out/$name.debug.elf")"
    size="$(stat -c%s "$out/$name.elf")"
    printf '  linked:   %s\n' "$out/$name.elf"
    printf '  size:     %s bytes (%.2f MB), from %s with debug info\n\n' \
        "$size" "$(echo "$size" | awk '{print $1/1048576}')" "$debug_size"
    return 0
}

# ---------------------------------------------------------------------- qt-hello
out="${QT_HELLO_OUT:-$root/target/qt-hello}"
mkdir -p "$out"

echo "compiling qt-hello"
clang++ "${cxxflags[@]}" -c \
    "$root/services/qt-hello/main.cpp" -o "$out/qt-hello.o" || exit 1
link_program qt-hello "$out/qt-hello.o" "${qtbase_libs[@]}" || exit 1

# -------------------------------------------------------------------------- shell
if [ ! -d "$QT_DECL/lib" ]; then
    echo "  No qtdeclarative build at $QT_DECL, so the QML program is not linked."
    echo "  Build it with qt-configure-module, or set QT_DECL. The kernel reports"
    echo "  its absence at boot rather than pretending it ran."
    exit 0
fi

out="${SHELL_OUT:-$root/target/shell}"
mkdir -p "$out"

echo "compiling shell"
clang++ "${cxxflags[@]}" \
    -isystem "$QT_DECL/include" \
    -isystem "$QT_DECL/include/QtQml" \
    -isystem "$QT_DECL/include/QtQuick" \
    -c "$root/services/shell/main.cpp" -o "$out/shell.o" || exit 1

# The QML half of the link line.
#
# The four plugin archives come first and they are the reason this program works at
# all: a QML module in a static build registers its types from a plugin class, and
# nothing references that class unless `Q_IMPORT_QML_PLUGIN` in `main.cpp` does. Each
# of those macros produces an undefined symbol, and these archives are what resolve
# it. Drop one and the link still succeeds — the macro is what creates the
# reference — but the program dies at run time with `module "QtQuick" is not
# installed`, which reads like a missing directory on disk.
#
# Then the libraries, in dependency order: Quick refers to Qml and QmlModels, the
# module plugins refer to their own library, and all of them refer to QtGui and
# QtCore, which is why the qtbase block follows rather than precedes. QmlMeta is the
# `QtQml` QML module proper — separate from libQt6Qml, which is the engine.
qml_libs=(
    "$QT_DECL/qml/QtQuick/libqtquick2plugin.a"
    "$QT_DECL/qml/QtQml/libqmlplugin.a"
    "$QT_DECL/qml/QtQml/Models/libmodelsplugin.a"
    "$QT_DECL/qml/QtQml/WorkerScript/libworkerscriptplugin.a"
    "$QT_DECL/lib/libQt6Quick.a"
    "$QT_DECL/lib/libQt6QmlMeta.a"
    "$QT_DECL/lib/libQt6QmlModels.a"
    "$QT_DECL/lib/libQt6QmlWorkerScript.a"
    "$QT_DECL/lib/libQt6Qml.a"
    "$QT_DECL/lib/libQt6QmlModels.a"
)
link_program shell "$out/shell.o" "${qml_libs[@]}" "${qtbase_libs[@]}" || exit 1

echo "  Most of both images is .text: a static QtCore and QtGui with the raster"
echo "  paint engine, FreeType and HarfBuzz linked in, and for the QML one a"
echo "  JavaScript engine and a scene graph on top. There is no dynamic loader"
echo "  here, so nothing is shared with anything and every program that uses Qt"
echo "  carries its own copy — which is the cost of the same decision that makes"
echo "  Q_IMPORT_PLUGIN necessary."
