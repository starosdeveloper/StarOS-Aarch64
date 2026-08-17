/* sys/syscall.h — the numbers a program passes to `syscall()`, and why none of them
 * work here.
 *
 * The numbers below are Linux's, for AArch64. This kernel's are its own, and they
 * are in `crates/abi/src/syscall.rs`; the two sets have nothing to do with each
 * other, so `syscall()` in `crates/staros-libc/src/proc.rs` refuses *every* number
 * with `ENOSYS` rather than forwarding one and calling `Revoke` when the caller
 * asked for `gettid`.
 *
 * That makes this header look pointless, and it is not. The raw `syscall()`
 * interface is used precisely where a library wrapper might be missing, so every
 * caller of it checks the return value — and the code that does the checking still
 * has to compile. Qt is the case in hand: `qv4stacklimits.cpp` decides whether it is
 * on the main thread with
 *
 *     if (getpid() != static_cast<pid_t>(syscall(SYS_gettid)))
 *         return stackPropertiesGeneric();
 *
 * and the refusal takes it down the generic path, which asks `pthread_getattr_np`
 * for the stack — a question this library answers with the real bounds. Without the
 * header the file does not compile at all, and the error names a missing include
 * rather than a missing system call.
 *
 * So: real numbers, so that a program logging the one that failed logs a number a
 * person can look up; and a guarantee that all of them fail the same way.
 *
 * Only the numbers something in this tree actually names are listed. This is not a
 * transcription of Linux's table, and it should not become one — an entry here is a
 * claim that some caller passes it, and an unused entry is a claim with nothing
 * behind it.
 */
#ifndef _SYS_SYSCALL_H
#define _SYS_SYSCALL_H 1

/* Both spellings, because both are in use: `__NR_x` is the kernel header's name and
 * `SYS_x` is glibc's alias for it, and code picks whichever its author knew. */
#define __NR_futex       98
#define __NR_tgkill      131
#define __NR_gettid      178
#define __NR_getrandom   278
#define __NR_membarrier  283

#define SYS_futex        __NR_futex
#define SYS_tgkill       __NR_tgkill
#define SYS_gettid       __NR_gettid
#define SYS_getrandom    __NR_getrandom
#define SYS_membarrier   __NR_membarrier

#ifdef __cplusplus
extern "C" {
#endif

/* Declared here as well as conceptually in <unistd.h>, because a translation unit
 * that includes this header is exactly the one about to call it. Returns -1 with
 * `errno` set to `ENOSYS`, for every number, always. */
long syscall(long number, ...);

#ifdef __cplusplus
}
#endif

#endif /* sys/syscall.h */
