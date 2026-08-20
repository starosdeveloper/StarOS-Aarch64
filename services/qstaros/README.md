# qstaros — the QPA platform plugin

What Qt needs to know about this machine. Everything below it — the display
server, the input routing, the shared-memory surfaces, the C library — exists
and was exercised without Qt; this is where those meet Qt's names for them.

## Building it

The plugin builds *inside* qtbase's own tree, which is how every platform
plugin is built and not a convenience: a QPA plugin implements Qt's private
platform interfaces, which are versioned with the build rather than installed,
so an out-of-tree plugin would be reaching into headers Qt does not promise to
keep.

So the directory is symlinked in rather than copied — edits here take effect
without a second copy to keep in step.

```sh
QT_SRC=~/qt-src/qtbase-everywhere-src-6.11.1
STAROS=~/Documents/StarOS-Kernel-main/kernel-Aarch64

# 1. the patches, three of them, each explained in patches/README.md
cd "$QT_SRC"
for p in "$STAROS"/services/qstaros/patches/*.patch; do patch -p1 < "$p"; done

# 2. the plugin, symlinked into the tree
ln -sfn "$STAROS/services/qstaros" "$QT_SRC/src/plugins/platforms/staros"

# 3. configure and build
mkdir -p ~/qt-src/build-qtbase-rtti && cd ~/qt-src/build-qtbase-rtti
"$QT_SRC/configure" \
    -qt-host-path /usr -platform linux-clang -qpa staros \
    -static -release \
    -no-opengl -no-icu -no-glib -no-dbus -no-pkg-config \
    -no-feature-network -no-feature-sql -no-feature-testlib \
    -no-feature-printsupport -no-feature-widgets -no-feature-concurrent \
    -no-feature-process -no-feature-processenvironment -no-feature-dlopen \
    -prefix ~/qt-src/qt-staros \
    -- -DCMAKE_TOOLCHAIN_FILE="$STAROS/services/qstaros/staros-toolchain.cmake"
ninja
```

`cargo kbuild` must have run first: the toolchain file looks for
`libstaros_libc.a` and `cxx-runtime.o` under `target/`, and says so rather than
letting the link fail on a missing `_start`.

### The configure flags that are decisions, not preferences

`-qt-host-path /usr` uses the distribution's Qt 6 for the host tools — `moc`,
`rcc`, `qmltyperegistrar`. There is no need to build a host Qt as long as the
versions match.

`-no-feature-dlopen` is the sharpest one. `dlopen` *links* here, so configure's
test passes and Qt concludes the loader exists — but it returns null at run
time, because the kernel's ELF loader gives every `PT_LOAD` segment its
permissions once and EL0 has no way to add execute afterwards. Leaving the
feature on builds `QLibrary`, `QPluginLoader`'s file half and an ELF parser to
read plugin metadata out of files that could never be loaded. Turning it off is
what makes the static plugin path the only path, which it already was.

`-no-feature-process` for the same shape of reason: `fork` refuses.

`-no-opengl` because there is no GPU driver. The software rasteriser is what
draws, and QtQuick has a software backend that uses it.

`-no-feature-qml-jit`, for qtdeclarative, is the same shape of statement about
the kernel rather than about Qt. V4's just-in-time compiler writes machine code
into memory and jumps to it; `mprotect` here refuses `PROT_EXEC` with `EPERM`,
deliberately, because the ELF loader gives each `PT_LOAD` its permissions once
and nothing can add execute afterwards. The bytecode interpreter is not a
fallback — it is the only thing that can run.

### RTTI, and why the build directories say `-rtti`

The first build of both trees was `-fno-exceptions -fno-rtti`. Half of that
survived.

`QSGSoftwareRenderableNodeUpdater::visit(QSGGeometryNode *)` — the software
scene graph's dispatch, which decides what a node is before drawing it — is
five chained `dynamic_cast`s ending in `// We dont know, so skip`. There is no
type enum beside it. Without RTTI every node in every scene takes the last
branch, and a QML program runs, paints an empty window, and reports nothing
wrong.

So `services/qstaros/staros-toolchain.cmake` compiles with `-frtti`, both trees
were reconfigured and rebuilt for it into `build-qtbase-rtti` and
`build-qtdeclarative-rtti`, and `__dynamic_cast` is implemented in
`crates/staros-libc/cxx/runtime.cpp` — the one algorithm in that file rather
than a hook. `services/hello-cpp` exercises it over all three of the ABI's
type-information shapes before any of Qt is involved.

Exceptions are still genuinely absent, and that half of the decision stands.

## Qt Quick as well

qtdeclarative is a second tree, configured against the *installed* qtbase
rather than its build directory:

```sh
cd ~/qt-src/build-qtbase-rtti && ninja install    # into ~/qt-src/qt-staros
mkdir -p ~/qt-src/build-qtdeclarative-rtti && cd ~/qt-src/build-qtdeclarative-rtti
~/qt-src/qt-staros/bin/qt-configure-module \
    ~/qt-src/qtdeclarative-everywhere-src-6.11.1 -no-feature-qml-jit \
    -- -DQT_BUILD_TESTS=OFF -DQT_BUILD_EXAMPLES=OFF
ninja lib/libQt6Quick.a lib/libQt6Qml.a lib/libQt6QmlModels.a \
      lib/libQt6QmlWorkerScript.a lib/libQt6QmlMeta.a \
      qml/QtQuick/libqtquick2plugin.a qml/QtQml/libqmlplugin.a \
      qml/QtQml/Models/libmodelsplugin.a qml/QtQml/WorkerScript/libworkerscriptplugin.a
```

Named targets rather than a bare `ninja`, and that is a decision. A full build
includes `qmldom`, `qmlls` and `qmlformat` — developer tooling that would never
run on this system — and `qmldom` is the one part of qtdeclarative that uses
`typeid` on types with no vtable, which does not compile here. Building what
the program links is both faster and the truthful description of what this
system has.

### Configure's warning about the platform plugin

The summary prints:

    No QPA platform plugin enabled! This will produce a Qt that cannot run GUI
    applications.

It is wrong here, and the reason is worth knowing rather than working around:
the condition behind that message is a hard-coded list of Qt's own Linux
plugins — xcb, eglfs, directfb, linuxfb — and this plugin is not one of them.
`libqstaros.a` is built and `qt_static_plugin_QStarosIntegrationPlugin()` is in
it, which is what a program actually needs.

## Linking a program against it

`scripts/qt-link.sh` builds `services/qt-hello` and `services/shell`, and is
meant to be read as much as run: the link lines in it are the whole argument
about how a Qt program is put together with no dynamic loader — every archive
named, in dependency order, with the C++ runtime and the C library last.

For the QML program there is a second half to that argument. A QML module in a
static build registers its types from a plugin class, and nothing references
that class unless `Q_IMPORT_QML_PLUGIN` does. Miss one and the link still
succeeds — the macro is what creates the reference — and the program dies at run
time with `module "QtQuick" is not installed`, which reads like a missing
directory on disk.

```sh
cd "$STAROS" && ./scripts/qt-link.sh
```

It is a script rather than part of `crates/kernel/build.rs` because Qt lives
outside this repository, at a path that is a property of whoever built it.
Wiring that into the kernel's build would make `cargo kbuild` fail on any
machine that has not built Qt — which is every machine, the first time.

## The shape of the plugin

| file | what it knows |
| --- | --- |
| `qstarosconnection` | the display protocol, and it is the only file that does |
| `qstarosscreen` | the screen's geometry and depth, from the server's answer |
| `qstaroswindow` | a surface and its two buffers |
| `qstarosbackingstore` | a `QImage` over the shared pixels — not a copy of them |
| `qstarosintegration` | which of QPA's optional interfaces exist here |
| `main.cpp` | the factory Qt asks for the platform named "staros" |

Capability handles 3, 4 and 5 are the display request endpoint, its reply, and
the input channel. The order is the ABI; there is nothing to ask.

## What is deliberately absent

No OpenGL, no theme, no cursor, no clipboard. The last three are optional by
QPA's own contract, which is why they can be absent rather than stubbed: an
optional interface Qt asks for and does not get is a feature it turns off, and
a stub that answers wrongly is a feature it uses.

Vsync is absent too, and not simulated. ramfb has no flip and no vblank, so
there is nothing to synchronise against and a timer pretending to be one would
be a lie with a plausible period.
