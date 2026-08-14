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

struct sigaction {
    /* Named `sa_handler` and not a union: `SA_SIGINFO` handlers take three
     * arguments, and a program that sets one casts it. Offering the union would
     * mean offering `siginfo_t`, which nothing here can fill in truthfully. */
    void *sa_handler;
    sigset_t sa_mask;
    int sa_flags;
    void *sa_restorer;
};

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
