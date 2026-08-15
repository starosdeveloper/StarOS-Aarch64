/* signal.h — the arithmetic of signals, on a system that delivers none.
 *
 * Nothing here ever raises a signal at a program: there is no timer that
 * interrupts, no terminal that sends SIGINT, and a fault kills the task rather than
 * delivering SIGSEGV to it. What is real is the *bookkeeping* — sets, masks,
 * handlers remembered — because libstdc++ and Qt call it during start-up and would
 * not link without it, and because `raise` really does run the handler a program
 * installed, which is the one path that works end to end.
 *
 * `sigset_t` is 128 bytes: glibc's size, not the eight a 64-bit mask needs. Qt
 * passes one to `pthread_sigmask` by pointer and a smaller structure would have
 * this library write past it. `crates/staros-libc/src/proc.rs` asserts the size and
 * the offsets in `struct sigaction`, and `services/hello-c` checks the *raw bits* a
 * set ends up with — a round-trip test of `sigaddset` and `sigismember` cannot see
 * an off-by-one in the numbering, because the two agree with each other by
 * construction. That falsification failed to fail once, which is why it is spelled
 * out here.
 */
#ifndef _SIGNAL_H
#define _SIGNAL_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Signals count from one. Signal 64 is the last bit of the first word of a mask,
 * and there is no signal 65 — which is what an implementation numbering from zero
 * gets wrong invisibly. */
#define SIGHUP     1
#define SIGINT     2
#define SIGQUIT    3
#define SIGILL     4
#define SIGTRAP    5
#define SIGABRT    6
#define SIGIOT     6
#define SIGBUS     7
#define SIGFPE     8
#define SIGKILL    9
#define SIGUSR1   10
#define SIGSEGV   11
#define SIGUSR2   12
#define SIGPIPE   13
#define SIGALRM   14
#define SIGTERM   15
#define SIGSTKFLT 16
#define SIGCHLD   17
#define SIGCONT   18
#define SIGSTOP   19
#define SIGTSTP   20
#define SIGTTIN   21
#define SIGTTOU   22
#define SIGURG    23
#define SIGXCPU   24
#define SIGXFSZ   25
#define SIGVTALRM 26
#define SIGPROF   27
#define SIGWINCH  28
#define SIGIO     29
#define SIGPOLL   29
#define SIGPWR    30
#define SIGSYS    31
#define NSIG      65

/* 1024 bits, as glibc has it. Larger than any signal number needs, and that is the
 * point: the size is part of the ABI, and a caller passing one of these by pointer
 * expects this many bytes to be readable. */
typedef struct {
    unsigned long __bits[16];
} sigset_t;

typedef void (*__sighandler_t)(int);

#define SIG_DFL ((__sighandler_t)0)
#define SIG_IGN ((__sighandler_t)1)
#define SIG_ERR ((__sighandler_t)-1)

/* `sigprocmask` and `pthread_sigmask` how-values. */
#define SIG_BLOCK   0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2

/* `sa_flags`. Accepted and recorded; none of them changes a delivery that does not
 * happen, except `SA_SIGINFO`, which changes which member of the handler union a
 * caller means and is therefore about layout rather than behaviour. */
#define SA_NOCLDSTOP 0x00000001
#define SA_NOCLDWAIT 0x00000002
#define SA_SIGINFO   0x00000004
#define SA_RESTART   0x10000000
#define SA_NODEFER   0x40000000
#define SA_RESETHAND 0x80000000

/* What an `SA_SIGINFO` handler is told about a signal.
 *
 * Never filled in — nothing here delivers a signal, so no handler is ever called
 * with one — and it exists anyway, for the same reason `struct rusage` does in
 * <sys/resource.h>: a handler's *signature* mentions it, and a type that is only
 * ever pointed at still has to be complete for the pointer types to match.
 *
 * The size is glibc's 128 bytes. The union is what makes that number real: the same
 * storage means `si_pid` for a SIGCHLD and `si_addr` for a SIGSEGV, and a structure
 * that declared only the members used here would be a different size and a different
 * ABI. */
typedef struct {
    int si_signo;
    int si_errno;
    int si_code;
    int __pad0;
    union {
        int __pad[28];
        struct {
            pid_t si_pid;
            uid_t si_uid;
            int si_status;
            long si_utime;
            long si_stime;
        } __child;
        struct {
            void *si_addr;
        } __fault;
    } __fields;
} siginfo_t;

/* glibc's accessor macros, which are how these members are named in code — the
 * union is an implementation detail of the layout, not something callers write. */
#define si_pid    __fields.__child.si_pid
#define si_uid    __fields.__child.si_uid
#define si_status __fields.__child.si_status
#define si_utime  __fields.__child.si_utime
#define si_stime  __fields.__child.si_stime
#define si_addr   __fields.__fault.si_addr

typedef void (*__sigaction_handler_t)(int, siginfo_t *, void *);

struct sigaction {
    /* A union, as glibc has it, and not the `void *` this was first written with.
     *
     * The two members are the two shapes a handler can have — one argument, or
     * three under `SA_SIGINFO` — and C++ will not convert between a function pointer
     * and `void *` at all. Qt found it in one line: `qcore_unix.cpp` line 33 writes
     * `noaction.sa_handler = SIG_IGN`, and against a `void *` member the error is
     * `assigning to 'void *' from '__sighandler_t' converts between void pointer and
     * function pointer`. The union is not a convenience here; it is the only way the
     * assignment is legal.
     *
     * Both members occupy the same eight bytes, so the structure's size and the
     * offsets `crates/staros-libc/src/proc.rs` asserts are unchanged. */
    union {
        __sighandler_t sa_handler;
        __sigaction_handler_t sa_sigaction;
    } __handler;
    sigset_t sa_mask;
    int sa_flags;
    void *sa_restorer;
};

/* Written as members in every program that uses them. */
#define sa_handler   __handler.sa_handler
#define sa_sigaction __handler.sa_sigaction

int sigemptyset(sigset_t *set);
int sigfillset(sigset_t *set);
int sigaddset(sigset_t *set, int signum);
int sigdelset(sigset_t *set, int signum);
int sigismember(const sigset_t *set, int signum);

int sigaction(int signum, const struct sigaction *act, struct sigaction *old);
int sigprocmask(int how, const sigset_t *set, sigset_t *old);
__sighandler_t signal(int signum, __sighandler_t handler);

/* `raise` runs this process's own handler, which is the one signal path that works
 * end to end. `kill` refuses: there is no other process to send to, and pretending
 * otherwise would have a caller believe it had stopped something. */
int raise(int signum);
int kill(pid_t pid, int signum);

#ifdef __cplusplus
}
#endif

#endif /* signal.h */
