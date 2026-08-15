# The CMake toolchain file Qt is cross-configured with.
#
# It describes one thing: how to compile and link for this system. Everything Qt's
# configure then decides — which features exist, which libraries are found — follows
# from whether a test program compiles with these flags, so an error here shows up
# as a feature quietly turned off rather than as a message about the toolchain.
#
# `CMAKE_SYSTEM_NAME` is Linux and not Generic, deliberately. Generic means "no
# operating system", and Qt's build then skips every Unix source file — the event
# dispatcher, the file engine, the thread implementation — and produces a QtCore
# with nothing underneath it. This system presents a Linux ABI on purpose (glibc's
# `struct stat`, `dirent`, `sigset_t`, errno numbers, `poll`), and
# `crates/staros-libc` was written from a measured list of what a real Qt link asks
# glibc for. Saying Linux is a statement about the ABI, which is true; the services
# that are missing refuse at run time with the errno that names the reason.

set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)

# clang, targeting bare metal. There is no cross gcc on this machine and no need for
# one: clang is a cross compiler by construction, and `--target` is the whole
# difference between building for the host and for this.
set(CMAKE_C_COMPILER clang)
set(CMAKE_CXX_COMPILER clang++)
set(CMAKE_C_COMPILER_TARGET aarch64-unknown-none)
set(CMAKE_CXX_COMPILER_TARGET aarch64-unknown-none)
set(CMAKE_ASM_COMPILER clang)
set(CMAKE_ASM_COMPILER_TARGET aarch64-unknown-none)

# The sysroot, as three include paths rather than one `--sysroot`.
#
# `--sysroot` expects a tree shaped like a distribution's — `usr/include`,
# `usr/lib`, a linker script — and this is not one: the C headers are in the source
# tree, the C++ templates come from the *host's* libstdc++, and the library is a
# single `.a` produced by cargo. Naming the three directly is what that actually is.
set(STAROS_ROOT "${CMAKE_CURRENT_LIST_DIR}/../..")
set(STAROS_SYSROOT "${STAROS_ROOT}/crates/staros-libc/include")

# The host's libstdc++ headers. Only the templates are used — the compiled half is
# `crates/staros-libc/cxx/runtime.cpp`, measured against Qt by
# `scripts/cxx-progress.sh` and complete as of this file being written.
file(GLOB STAROS_CXX_INCLUDE "/usr/include/c++/*")
list(GET STAROS_CXX_INCLUDE 0 STAROS_CXX_INCLUDE)
file(GLOB STAROS_CXX_TARGET_INCLUDE "${STAROS_CXX_INCLUDE}/*-linux-gnu")
list(GET STAROS_CXX_TARGET_INCLUDE 0 STAROS_CXX_TARGET_INCLUDE)

set(STAROS_COMMON_FLAGS
    # `--target` spelled out as a flag as well as set through
    # CMAKE_<LANG>_COMPILER_TARGET. The variable reaches compilation and does not
    # reliably reach the *link*, and the symptom is the far end of a long
    # configure: `is incompatible with elf_x86_64`, from a linker nobody chose,
    # about objects that are perfectly correct.
    "--target=aarch64-unknown-none"
    # No `-ffreestanding`, and that is the fifth cross-build error rather than an
    # omission.
    #
    # In a freestanding implementation the entry point is implementation-defined, so
    # `main` stops being special and C++ mangles it like any other function:
    # `_Z4mainiPPc`. The C library's `_start` calls `main`, the linker does not find
    # it, and the message is `undefined symbol: main` followed by lld's own hint —
    # "did you mean to declare main() as extern C?" — pointing at the object that
    # defines it. Three attempts went into link order before the object was
    # disassembled and the mangled name was simply there.
    #
    # `services/hello-cpp` has always been built without it, which is why the C++
    # side of this tree works and why the flag looked harmless. What it would have
    # bought is covered already: `-nostdlibinc` keeps the host's headers out and
    # `-fno-builtin` stops the compiler assuming library functions it can inline.
    #
    # The sysroot is *not* here — see STAROS_SYSROOT_FLAG below. Its position on the
    # command line differs between C and C++ and the difference is load-bearing.
    "-nostdlibinc -fno-builtin"
    "-fno-omit-frame-pointer -fno-stack-protector -fno-pie"
    # Qt reads the operating system from compiler macros, and a bare-metal target
    # defines none — `qsystemdetection.h` stops with "Qt has not been ported to this
    # OS". See the note on CMAKE_SYSTEM_NAME above: this is the ABI statement, made
    # where the compiler can see it.
    "-D__linux__=1 -D__unix__=1"
    # And the qualifier on that claim, which Qt itself provides.
    #
    # `__linux__` says "this ABI is Linux's", which is true. It does *not* say "every
    # Linux syscall is here", which is not — and Qt reads the first as the second in
    # a handful of places. The sharpest is `qfutex_linux_p.h`: on any `Q_OS_LINUX`
    # target Qt implements every mutex, semaphore and read-write lock as a raw
    # `syscall(__NR_futex, ...)`, and the first sign of trouble is
    # `fatal error: 'asm/unistd.h' file not found` — a header about syscall numbers,
    # from a file about locking.
    #
    # `QT_LINUXBASE` is Qt's own name for exactly this situation: Linux ABI, reduced
    # syscall surface. It is what the LSB builds set, and the comment beside the
    # futex selection reads "use Linux mutexes everywhere except for LSB builds". It
    # steers four decisions in the sources this build compiles — futexes,
    # `pthread_setname_np`, inotify, and one FreeType include path — and in every one
    # of them the LSB branch is the branch that is true here.
    #
    # What Qt falls back to is the POSIX threading this library already provides:
    # `crates/staros-libc/src/thread.rs` has mutexes and condition variables over the
    # kernel's `NotifySignal`/`Wait`, which is the same primitive a futex is, reached
    # by the name this system actually uses.
    "-DQT_LINUXBASE=1")
string(REPLACE ";" " " STAROS_COMMON_FLAGS "${STAROS_COMMON_FLAGS}")

set(STAROS_SYSROOT_FLAG "-isystem ${STAROS_SYSROOT}")

# In C++, the libstdc++ directories come *before* the C sysroot, and that ordering is
# the whole reason this is written out instead of appended.
#
# libstdc++'s `<cmath>` is not a self-contained header: line 55 is
# `#include_next <math.h>`, and `#include_next` resumes the search at the entry
# *after* the directory the current file was found in. With the sysroot listed first,
# there is nothing after `/usr/include/c++/16` to resume into, and the message is
#
#     /usr/include/c++/16/cmath:55:15: fatal error: 'math.h' file not found
#
# about a `math.h` that exists, in a directory that is on the command line. Every
# libstdc++ C-compatibility header — <cmath>, <cstdio>, <cstdlib>, <cstring>,
# <cwchar> — works the same way, so this ordering is what makes any of them usable.
#
# The C compiler has no such wrapper layer and takes the sysroot on its own.
set(CMAKE_C_FLAGS_INIT "${STAROS_COMMON_FLAGS} ${STAROS_SYSROOT_FLAG}")
set(CMAKE_CXX_FLAGS_INIT
    "${STAROS_COMMON_FLAGS} -isystem ${STAROS_CXX_INCLUDE} -isystem ${STAROS_CXX_TARGET_INCLUDE} ${STAROS_SYSROOT_FLAG} -fno-exceptions -fno-rtti")

# Static everything. There is no dynamic loader here — `dlopen` refuses, and the
# kernel's ELF loader gives `PT_LOAD` fixed permissions — so a plugin is linked in
# rather than found, which is why `Q_IMPORT_PLUGIN` will be needed in the
# application.
set(BUILD_SHARED_LIBS OFF)
set(CMAKE_POSITION_INDEPENDENT_CODE OFF)

# Nothing on the host is a valid dependency for the target. Without this, Qt's
# configure finds the host's zlib and OpenGL and links a target library against x86
# objects — which fails at the end of a long build with an error about file formats.
set(CMAKE_FIND_ROOT_PATH "${STAROS_ROOT}")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)

# And pkg-config, which walks around all four of those.
#
# `CMAKE_FIND_ROOT_PATH_MODE_*` constrain CMake's own `find_library` and
# `find_path`. `pkg_check_modules` does not use either: it runs `/usr/bin/pkg-config`,
# which reads the *host's* `/usr/lib/pkgconfig` and answers about the host's
# packages. Qt's configure asked it about libb2, jemalloc, libsystemd and at-spi2,
# was told yes to all four, and put `-I/usr/include` on every compile line in QtCore.
#
# `-nostdlibinc` does not survive that. It removes the compiler's default include
# paths; an explicit `-I/usr/include` puts glibc's headers back, ahead of the
# `-isystem` sysroot, and the build then compiles this system's programs against the
# host's C library. The error arrives from a header that is not wrong — glibc's
# `bits/floatn.h` typedefs `__float128`, aarch64 does not have it, and the message
# names a file nobody in this tree included:
#
#     /usr/include/bits/floatn.h:97:9: error: __float128 is not supported on this target
#
# Pointing PKG_CONFIG_LIBDIR at a directory with no `.pc` files in it is how
# pkg-config is told the target has no packages, which is true — this system has
# exactly the libraries in `crates/` and they do not ship pkg-config metadata. The
# executable is left findable so that a `pkg_check_modules` call still *runs* and
# still answers, rather than erroring out about a missing tool.
set(STAROS_EMPTY_PKGCONFIG "${CMAKE_BINARY_DIR}/staros-no-pkgconfig")
file(MAKE_DIRECTORY "${STAROS_EMPTY_PKGCONFIG}")
set(ENV{PKG_CONFIG_LIBDIR} "${STAROS_EMPTY_PKGCONFIG}")
set(ENV{PKG_CONFIG_PATH} "")
set(ENV{PKG_CONFIG_SYSROOT_DIR} "${STAROS_ROOT}")

# Linking, which is where a cross build usually goes wrong quietly.
#
# `-fuse-ld=lld` is not a preference. Without it clang hands the link to the host's
# `gcc`, which hands it to the host's `ld`, which is an x86 linker — and the error
# is "file in wrong format" from a tool nobody mentioned in the configuration.
#
# `-nostdlib` for the same reason: the host's crt1.o and libc are not this system's,
# and letting them in produces a binary that links and cannot run. What replaces
# them is `libstaros_libc.a` and the C++ runtime object beside it, both built by
# `cargo kbuild` — so that has to have been run before Qt is configured, and the
# check below says so rather than letting the link fail with a missing `_start`.
find_file(STAROS_LIBC libstaros_libc.a
    PATHS "${STAROS_ROOT}/target/aarch64-unknown-none/debug/build"
    PATH_SUFFIXES "" NO_DEFAULT_PATH NO_CACHE)
if(NOT STAROS_LIBC)
    file(GLOB_RECURSE STAROS_LIBC_CANDIDATES
        "${STAROS_ROOT}/target/aarch64-unknown-none/debug/build/*/out/libstaros_libc.a")
    list(SORT STAROS_LIBC_CANDIDATES)
    list(REVERSE STAROS_LIBC_CANDIDATES)
    list(LENGTH STAROS_LIBC_CANDIDATES STAROS_LIBC_COUNT)
    if(STAROS_LIBC_COUNT EQUAL 0)
        message(FATAL_ERROR
            "no libstaros_libc.a under ${STAROS_ROOT}/target — run `cargo kbuild` first")
    endif()
    list(GET STAROS_LIBC_CANDIDATES 0 STAROS_LIBC)
endif()

file(GLOB_RECURSE STAROS_CXX_RUNTIME
    "${STAROS_ROOT}/target/aarch64-unknown-none/debug/build/*/out/cxx-runtime.o")
list(SORT STAROS_CXX_RUNTIME)
list(REVERSE STAROS_CXX_RUNTIME)
list(LENGTH STAROS_CXX_RUNTIME STAROS_CXX_RUNTIME_COUNT)
if(STAROS_CXX_RUNTIME_COUNT EQUAL 0)
    message(FATAL_ERROR
        "no cxx-runtime.o under ${STAROS_ROOT}/target — run `cargo kbuild` first")
endif()
list(GET STAROS_CXX_RUNTIME 0 STAROS_CXX_RUNTIME)

# The linker script every EL0 program in this tree is linked with. It places the
# image at `USER_BASE` and defines the thread-local boundary symbols — `__tdata_start`
# and its relatives — which `crates/staros-libc`'s TLS setup references and which no
# default script provides. Without it the first executable Qt tries to link fails on
# a symbol that has nothing to do with Qt.
set(STAROS_IMAGE_LD "${STAROS_ROOT}/services/init/boot/image.ld")

# Link with `ld.lld` directly, not through the compiler driver.
#
# This is the fix for the error that took four attempts to see. clang, asked to link
# for `aarch64-unknown-none`, hands the job to `/usr/bin/g++` — it has no linker
# configuration for a bare-metal aarch64 triple and falls back to the system's GCC
# driver. That driver then calls `ld.lld` with its own `-m elf_x86_64` *first*, every
# host `-L` path, and GCC's LTO plugin; our `-Wl,-m,aarch64linux` arrives second and
# the first one wins. The visible symptoms were, in order: "file in wrong format",
# "incompatible with elf_x86_64", and finally "undefined symbol: main" — three
# different messages, one cause, and none of them naming g++.
#
# `crates/kernel/build.rs` has linked this tree's EL0 programs with `rust-lld`
# directly since the first one. Doing the same here is not a workaround; it is the
# same decision, and the compiler driver was the deviation.
find_program(STAROS_LD ld.lld REQUIRED)
set(CMAKE_LINKER "${STAROS_LD}")

# The rules, spelled out because the default ones invoke the compiler. `-m` is the
# emulation, and nothing else supplies it once the driver is gone.
#
# `<LINK_FLAGS>` is deliberately *not* expanded. CMake fills it with things meant
# for a compiler driver — `-Wl,-v` during the ABI test, `-Wl,` prefixes generally —
# and `ld.lld` refuses them by name. Our own flags are in the rule, so the
# substitution has nothing to contribute and everything to break.
set(STAROS_LD_FLAGS "-m aarch64linux -static -T${STAROS_IMAGE_LD}")
set(CMAKE_C_LINK_EXECUTABLE
    "<CMAKE_LINKER> ${STAROS_LD_FLAGS} <OBJECTS> -o <TARGET> <LINK_LIBRARIES>")
set(CMAKE_CXX_LINK_EXECUTABLE
    "<CMAKE_LINKER> ${STAROS_LD_FLAGS} <OBJECTS> -o <TARGET> <LINK_LIBRARIES>")

# Nothing else goes on the link line: the flags above are the whole of it, and a
# leftover `--target` or `-fuse-ld` would now be handed to `ld.lld`, which does not
# know them.
set(STAROS_LINK_FLAGS "")

# The C library and the C++ runtime go in `STANDARD_LIBRARIES`, which the link rule
# above places *last*.
#
# Order is the whole point. A static archive contributes only what something already
# needs, so an archive ahead of the objects pulls in `_start`, `_start` needs `main`,
# and `main` is in an object the linker has not read yet. The error is `undefined
# symbol: main` about a program that defines `main` on the next line.
set(CMAKE_C_STANDARD_LIBRARIES "${STAROS_CXX_RUNTIME} ${STAROS_LIBC}" CACHE STRING "" FORCE)
set(CMAKE_CXX_STANDARD_LIBRARIES "${STAROS_CXX_RUNTIME} ${STAROS_LIBC}" CACHE STRING "" FORCE)

# Empty, in both the `_INIT` and plain forms. Everything the link needs is in the
# rule; anything here would be handed to `ld.lld`, which does not take compiler
# flags. The plain form is set as well because the compiler-ABI test reads it before
# the `_INIT` values have been folded into the cache — which is why an earlier
# attempt at this file compiled correctly for aarch64 and linked for x86.
set(CMAKE_EXE_LINKER_FLAGS_INIT "")
set(CMAKE_SHARED_LINKER_FLAGS_INIT "")
set(CMAKE_MODULE_LINKER_FLAGS_INIT "")
set(CMAKE_EXE_LINKER_FLAGS "" CACHE STRING "" FORCE)
set(CMAKE_SHARED_LINKER_FLAGS "" CACHE STRING "" FORCE)
set(CMAKE_MODULE_LINKER_FLAGS "" CACHE STRING "" FORCE)
