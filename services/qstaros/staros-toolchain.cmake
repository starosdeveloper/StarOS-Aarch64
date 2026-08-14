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
    "-nostdlibinc -isystem ${STAROS_SYSROOT} -ffreestanding -fno-builtin"
    "-fno-omit-frame-pointer -fno-stack-protector -fno-pie"
    # Qt reads the operating system from compiler macros, and a bare-metal target
    # defines none — `qsystemdetection.h` stops with "Qt has not been ported to this
    # OS". See the note on CMAKE_SYSTEM_NAME above: this is the ABI statement, made
    # where the compiler can see it.
    "-D__linux__=1 -D__unix__=1")
string(REPLACE ";" " " STAROS_COMMON_FLAGS "${STAROS_COMMON_FLAGS}")

set(CMAKE_C_FLAGS_INIT "${STAROS_COMMON_FLAGS}")
set(CMAKE_CXX_FLAGS_INIT
    "${STAROS_COMMON_FLAGS} -isystem ${STAROS_CXX_INCLUDE} -isystem ${STAROS_CXX_TARGET_INCLUDE} -fno-exceptions -fno-rtti")

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

# CMake's compiler check builds and *links* a program by default, which cannot work
# before the C library is in the link line. Compiling one is the part that proves
# the toolchain, and it is the part that can be done here.
set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)
