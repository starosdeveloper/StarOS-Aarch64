# Patches applied to qtbase 6.11.1 for this port

One so far, and the rule for adding another is that it must be a defect in Qt
rather than a difference in this system. Anything this system can answer for
itself belongs in `crates/staros-libc`, not here.

Apply from the unpacked source root:

    cd qtbase-everywhere-src-6.11.1
    patch -p1 < .../services/qstaros/patches/0001-prctl-header-under-linuxbase.patch

## 0001 — `prctl` used without its header under `QT_LINUXBASE`

`src/corelib/thread/qthread_unix.cpp` includes `<sys/prctl.h>` under

    #if defined(Q_OS_LINUX) && !defined(QT_LINUXBASE)

and then calls `prctl(PR_SET_NAME, name)` under

    #if defined(Q_OS_LINUX)

The two conditions do not match. Any build that defines `QT_LINUXBASE` — which
is Qt's own switch for "Linux ABI, reduced syscall surface", and which this port
sets for the reasons written in `services/qstaros/staros-toolchain.cmake` —
compiles the call without the declaration, and stops at

    error: use of undeclared identifier 'PR_SET_NAME'

The patch makes the include match the call. It cannot break an LSB build: the
header is Linux's own, present in any sysroot that defines `Q_OS_LINUX`, and
this port supplies it at `crates/staros-libc/include/sys/prctl.h`.

The alternative was to have some header this file already includes drag
`<sys/prctl.h>` in behind Qt's back. That would have worked and would have been
a lie about what `<sched.h>` is, left for the next person to discover.
