/* sched.h — giving up the rest of a timeslice.
 *
 * One function, which the kernel has had as a syscall since the scheduler existed.
 * It was declared in <pthread.h> because that is where the first caller looked for
 * it; POSIX puts it here, and a C++ program including <sched.h> and nothing else
 * has a right to find it.
 *
 * The scheduling *policy* calls — `sched_setscheduler`, priorities, affinity — are
 * absent rather than refusing. There is one policy here: round robin by timer tick,
 * decided by the kernel and not negotiable from EL0. Declaring a setter that always
 * failed would invite a program to keep trying, and there is nothing for it to try.
 */
#ifndef _SCHED_H
#define _SCHED_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Yield to whatever else is runnable. Always succeeds, and returns 0. */
int sched_yield(void);

#ifdef __cplusplus
}
#endif

#endif /* sched.h */
