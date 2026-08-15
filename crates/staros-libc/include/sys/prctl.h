/* sys/prctl.h — the Linux catch-all, of which one option means something here.
 *
 * `PR_SET_NAME` and `PR_GET_NAME` set and read the current thread's name, and
 * `crates/staros-libc/src/proc.rs` implements both against storage the thread
 * already has. Everything else `prctl` can be asked concerns machinery this kernel
 * does not have — seccomp, capabilities, the dumpable flag, the child subreaper —
 * and is refused with `EINVAL`, which is what Linux itself returns for an option a
 * kernel does not recognise.
 *
 * Qt asks: `qthread_unix.cpp` line 348 sets the thread name from `QThread`'s object
 * name, which is what makes a thread identifiable in a debugger.
 *
 * The interface is variadic and the meaning of the arguments depends on the option,
 * which is why nothing here can be type-checked. `PR_SET_NAME` takes a pointer to at
 * least 16 bytes including the terminator; a longer name is truncated, as on Linux.
 */
#ifndef _SYS_PRCTL_H
#define _SYS_PRCTL_H 1

#ifdef __cplusplus
extern "C" {
#endif

/* Linux's numbering, and the two that do something. */
#define PR_SET_NAME 15
#define PR_GET_NAME 16

/* Accepted by the interface and refused with EINVAL, listed because callers name
 * them in code that has a fallback. */
#define PR_SET_PDEATHSIG    1
#define PR_GET_PDEATHSIG    2
#define PR_SET_DUMPABLE     4
#define PR_GET_DUMPABLE     3
#define PR_SET_NO_NEW_PRIVS 38
#define PR_SET_CHILD_SUBREAPER 36

int prctl(int option, ...);

#ifdef __cplusplus
}
#endif

#endif /* sys/prctl.h */
