# Patches applied to qtbase 6.11.1 for this port

Four, and the rule for adding another is that it must be a defect in Qt rather
than a difference in this system. Anything this system can answer for itself
belongs in `crates/staros-libc`, not here.

Three of the four are the same defect wearing different clothes, and that is
worth saying at the top rather than leaving to be noticed: Qt uses `Q_OS_LINUX`
in places where it means *glibc on a Linux kernel*, while a `QT_LINUXBASE` build
is `Q_OS_LINUX` and is not that. Each time, some other file already draws the
distinction correctly and the two conditions have simply drifted apart. The fix
is always to make them the same condition, never to add a new one.

Apply all of them from the unpacked source root:

    cd qtbase-everywhere-src-6.11.1
    for p in .../services/qstaros/patches/*.patch; do patch -p1 < "$p"; done

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

## 0002 — `FutexAlwaysAvailable` must honour `QT_LINUXBASE`

`qfutex_p.h` picks the Linux futex backend with

    #elif defined(Q_OS_LINUX) && !defined(QT_LINUXBASE)

while `QBasicMutex::FutexAlwaysAvailable` is true for `Q_OS_LINUX` alone. A
`QT_LINUXBASE` build therefore gets the dummy futex, does not define
`QT_ALWAYS_USE_FUTEX`, and stops in `qmutex.cpp` at a `static_assert` that says
so in as many words. The two conditions describe one decision.

There is no futex here to take the other branch with: `syscall()` refuses every
number, because this kernel's are its own. The semaphore path is the correct one
for this system, and 0004 is what makes it actually work.

## 0003 — build an out-of-tree platform plugin dropped into `src/plugins/platforms`

Not a defect, and the one patch here that is a *build arrangement* rather than a
fix. A QPA plugin needs `Qt::GuiPrivate`, which is versioned with the build and
not part of the installed API, so building it inside qtbase's tree is what every
platform plugin already does. `services/qstaros` is symlinked in as
`src/plugins/platforms/staros` and this hunk adds the `add_subdirectory` for it,
guarded on the directory existing so it is inert in a tree without one.

## 0004 — `unlockInternal(void *)` must honour `QT_LINUXBASE` too

The sharpest of the three, because it does not fail at compile time. The
function opens with

    #if defined(Q_OS_FREEBSD) || defined(Q_OS_LINUX) || defined(Q_OS_WIN)
        // these platforms always have futex and have never called this function
        // from inline code
        Q_UNREACHABLE();
    #endif

so on `Q_OS_LINUX` the whole body is dead code and the compiler removes it. The
symbol survives with a size of zero and lands on whatever the linker places
next — here, on `QFreeList<QMutexPrivate>::~QFreeList()`.

But 0002 made `FutexAlwaysAvailable` false, so the inline `QMutex::unlock()`
takes its non-futex branch and *calls this function*. Control arrives in the
destructor of an unrelated class with a `QMutex*` in `x0`, which walks it as a
free list: `sem_destroy` over stack garbage, then `operator delete[]`, then
`free()` on a pointer of `0x137`.

What that looks like from outside is an EL0 data abort at `0x117` whose top
frame is a destructor nothing called, and two symbols sharing one address in
`nm`. Nothing in it mentions mutexes, futexes, or the assumption at the top of
the function.
