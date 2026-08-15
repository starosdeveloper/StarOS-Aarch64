/* sys/resource.h — the limits, which here are facts about the kernel rather than
 * settings.
 *
 * Implemented in `crates/staros-libc/src/proc.rs`. Qt's bundled `forkfd` includes
 * this header for `struct rusage`, which `wait4` takes a pointer to.
 *
 * `getrlimit` answers with the real numbers where there are real numbers to give:
 * `RLIMIT_STACK` is 256 KiB because that is where the kernel stops growing a task's
 * stack, and `RLIMIT_NOFILE` is the descriptor table's actual size. A program that
 * sizes a recursion or an `alloca` from `RLIMIT_STACK` gets an answer it can use
 * rather than `RLIM_INFINITY` and a fault later.
 *
 * `setrlimit` refuses. These are not tunables here — the stack bound is in the
 * kernel's page-fault handler and the descriptor count is an array's length — so
 * accepting a new value and not honouring it would be worse than saying no.
 */
#ifndef _SYS_RESOURCE_H
#define _SYS_RESOURCE_H 1

#include <sys/types.h>
#include <sys/time.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef unsigned long rlim_t;
typedef unsigned long rlim64_t;

#define RLIM_INFINITY   ((rlim_t)-1)
#define RLIM64_INFINITY ((rlim64_t)-1)
#define RLIM_SAVED_MAX  RLIM_INFINITY
#define RLIM_SAVED_CUR  RLIM_INFINITY

/* Linux's numbering, because this presents a Linux ABI. The three the
 * implementation answers with real values are STACK, NOFILE and AS; the rest are
 * accepted and answered `RLIM_INFINITY`, which is true — nothing here counts CPU
 * seconds or core files. */
#define RLIMIT_CPU     0
#define RLIMIT_FSIZE   1
#define RLIMIT_DATA    2
#define RLIMIT_STACK   3
#define RLIMIT_CORE    4
#define RLIMIT_RSS     5
#define RLIMIT_NPROC   6
#define RLIMIT_NOFILE  7
#define RLIMIT_MEMLOCK 8
#define RLIMIT_AS      9

struct rlimit {
    rlim_t rlim_cur;
    rlim_t rlim_max;
};

/* Same layout, same functions underneath. glibc separates them because a 32-bit
 * `rlim_t` once existed; on LP64 there is one type and the `64` names are aliases,
 * which is what the implementation does. */
struct rlimit64 {
    rlim64_t rlim_cur;
    rlim64_t rlim_max;
};

#define RUSAGE_SELF     0
#define RUSAGE_CHILDREN (-1)

/* Never filled in — `wait4` refuses with `ECHILD` before it would touch one, and
 * there is no `getrusage` here to fill one either. It exists because `forkfd.h`
 * declares a `struct rusage *` parameter, and a struct that is only ever pointed at
 * still has to be a complete type for the pointer types to match: without this
 * declaration the compiler invents a second `struct rusage` at file scope and the
 * error reads `incompatible pointer types passing 'struct rusage *' to parameter of
 * type 'struct rusage *'`, which is true and unhelpful. */
struct rusage {
    struct timeval ru_utime;
    struct timeval ru_stime;
    long ru_maxrss;
    long ru_ixrss;
    long ru_idrss;
    long ru_isrss;
    long ru_minflt;
    long ru_majflt;
    long ru_nswap;
    long ru_inblock;
    long ru_oublock;
    long ru_msgsnd;
    long ru_msgrcv;
    long ru_nsignals;
    long ru_nvcsw;
    long ru_nivcsw;
};

int getrlimit(int resource, struct rlimit *out);
int setrlimit(int resource, const struct rlimit *limit);
int getrlimit64(int resource, struct rlimit64 *out);
int setrlimit64(int resource, const struct rlimit64 *limit);

#ifdef __cplusplus
}
#endif

#endif /* sys/resource.h */
