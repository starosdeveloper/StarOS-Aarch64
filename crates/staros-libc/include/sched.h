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

#include <stddef.h>
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Yield to whatever else is runnable. Always succeeds, and returns 0. */
int sched_yield(void);

/* Affinity: which CPUs a thread may run on.
 *
 * There is one CPU, so the answer is a set containing exactly CPU 0 — and *reading*
 * it is meaningful even though setting it is not. Qt asks in order to size its
 * thread pool (`qthread_unix.cpp` line 584): it calls `sched_getaffinity` and counts
 * the bits, and a wrong answer here becomes a thread pool of the wrong size, which
 * shows up as either idle cores or oversubscription rather than as an error.
 *
 * `sched_setaffinity` is absent, not refusing, for the reason the module note gives:
 * there is nothing for a program to try.
 *
 * The set is a fixed 1024-bit array, as glibc has it. Qt sizes a VLA from
 * `sizeof(cpu_set_t)`, so the size is part of the interface and not an internal
 * choice. */
#define CPU_SETSIZE 1024
#define __CPU_BITS (8 * sizeof(unsigned long))

typedef struct {
    unsigned long __bits[CPU_SETSIZE / (8 * sizeof(unsigned long))];
} cpu_set_t;

/* The macros are how a set is manipulated — the struct is storage, not an interface.
 * The `_S` forms take an explicit size, for a set allocated larger than one
 * `cpu_set_t`; Qt uses `CPU_COUNT_S`. */
#define CPU_ZERO_S(size, set)                                                  \
    do {                                                                       \
        unsigned long *__b = (set)->__bits;                                    \
        for (size_t __i = 0; __i < (size) / sizeof(unsigned long); ++__i)      \
            __b[__i] = 0;                                                      \
    } while (0)
#define CPU_ZERO(set) CPU_ZERO_S(sizeof(cpu_set_t), set)

#define CPU_SET_S(cpu, size, set)                                              \
    do {                                                                       \
        size_t __c = (size_t)(cpu);                                            \
        if (__c < (size) * 8)                                                  \
            (set)->__bits[__c / __CPU_BITS] |= 1UL << (__c % __CPU_BITS);      \
    } while (0)
#define CPU_SET(cpu, set) CPU_SET_S(cpu, sizeof(cpu_set_t), set)

#define CPU_CLR_S(cpu, size, set)                                              \
    do {                                                                       \
        size_t __c = (size_t)(cpu);                                            \
        if (__c < (size) * 8)                                                  \
            (set)->__bits[__c / __CPU_BITS] &= ~(1UL << (__c % __CPU_BITS));   \
    } while (0)
#define CPU_CLR(cpu, set) CPU_CLR_S(cpu, sizeof(cpu_set_t), set)

#define CPU_ISSET_S(cpu, size, set)                                            \
    ((size_t)(cpu) < (size) * 8 &&                                             \
     ((set)->__bits[(size_t)(cpu) / __CPU_BITS] &                              \
      (1UL << ((size_t)(cpu) % __CPU_BITS))) != 0)
#define CPU_ISSET(cpu, set) CPU_ISSET_S(cpu, sizeof(cpu_set_t), set)

/* `__builtin_popcountl` rather than a loop over bits: the compiler emits a `cnt`
 * instruction for it, and more to the point a hand-written bit count is a place for
 * an off-by-one that a caller would never see — it would simply size its thread pool
 * one thread wrong. */
static inline int __cpu_count(size_t size, const cpu_set_t *set)
{
    int total = 0;
    for (size_t i = 0; i < size / sizeof(unsigned long); ++i)
        total += __builtin_popcountl(set->__bits[i]);
    return total;
}
#define CPU_COUNT_S(size, set) __cpu_count(size, set)
#define CPU_COUNT(set) CPU_COUNT_S(sizeof(cpu_set_t), set)

/* Fills `set` with the CPUs the thread may run on: bit 0, and nothing else.
 * `pid` is ignored — there is one process. */
int sched_getaffinity(pid_t pid, size_t size, cpu_set_t *set);

#ifdef __cplusplus
}
#endif

#endif /* sched.h */
