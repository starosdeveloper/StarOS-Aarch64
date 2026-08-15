/* semaphore.h — POSIX unnamed semaphores.
 *
 * Implemented in `crates/staros-libc/src/thread.rs`, over the same parking the
 * mutexes and condition variables use: a count, and a queue of threads waiting for
 * it to become positive. `sem_wait` blocks in the kernel rather than spinning.
 *
 * Named semaphores — `sem_open`, `sem_close`, `sem_unlink` — exist and refuse. A
 * name is a global identifier, and the objection is the one <sys/ipc.h> makes at
 * more length: a process that guessed the name would have the semaphore. Unnamed
 * semaphores in memory two threads already share have no such problem, which is why
 * those are the ones that work.
 *
 * `sem_t` is 32 bytes, matching the layout on the Rust side. The size is the ABI —
 * callers embed one in a structure — and `thread.rs` asserts it.
 */
#ifndef _SEMAPHORE_H
#define _SEMAPHORE_H 1

#include <stddef.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque, like the other synchronisation types in <pthread.h>: the contents are a
 * count and a wait queue whose representation callers must not depend on. */
typedef struct {
    long __opaque[4];
} sem_t;

/* `shared` is accepted and ignored — a semaphore between *processes* would need the
 * memory holding it to be shared, which is a capability this system delegates
 * explicitly, so the flag alone cannot make it true. Threads share their address
 * space and are what these are for. */
/* These report failure the way a syscall wrapper does — -1 with `errno` — and not
 * the way the `pthread_*` functions beside them do, which return the error number.
 * That is POSIX's inconsistency rather than this library's, and it is stated here
 * because the two families look like siblings and are not. */
int sem_init(sem_t *s, int shared, unsigned int value);
int sem_destroy(sem_t *s);
int sem_post(sem_t *s);
int sem_wait(sem_t *s);
int sem_trywait(sem_t *s);
/* `deadline` is absolute, not a duration, and it is on the one clock this system has
 * — monotonic. A caller passing a `CLOCK_REALTIME` deadline gets monotonic
 * behaviour; see `crates/staros-libc/src/time.rs`.
 *
 * The wait spins with a yield rather than sleeping, because the kernel's parkers
 * have no deadline to wake on. It is correct and it costs a timeslice. */
int sem_timedwait(sem_t *s, const struct timespec *deadline);

#ifdef __cplusplus
}
#endif

#endif /* semaphore.h */
