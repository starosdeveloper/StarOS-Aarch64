/* pthread.h — threads, as `crates/staros-libc` implements them over `SpawnThread`.
 *
 * The opaque types are 32 bytes each and **all-zero is a valid initialised state**,
 * which is what makes the static initialisers below correct rather than
 * approximately correct: a `static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;`
 * lands in `.bss` and is ready to lock.
 */
#ifndef _STAROS_PTHREAD_H
#define _STAROS_PTHREAD_H 1

#include <stddef.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef unsigned long pthread_t;
typedef struct {
    long __opaque[4];
} pthread_mutex_t;
typedef struct {
    long __opaque[4];
} pthread_cond_t;
typedef struct {
    long __opaque[4];
} pthread_rwlock_t;
typedef struct {
    long __opaque[4];
} pthread_attr_t;
typedef struct {
    long __opaque[2];
} pthread_mutexattr_t;
typedef struct {
    long __opaque[2];
} pthread_condattr_t;
typedef struct {
    long __opaque[2];
} pthread_rwlockattr_t;
typedef unsigned pthread_once_t;
typedef int pthread_key_t;

#define PTHREAD_MUTEX_INITIALIZER { { 0, 0, 0, 0 } }
#define PTHREAD_COND_INITIALIZER { { 0, 0, 0, 0 } }
#define PTHREAD_RWLOCK_INITIALIZER { { 0, 0, 0, 0 } }
#define PTHREAD_ONCE_INIT 0
#define PTHREAD_CREATE_JOINABLE 0
#define PTHREAD_CREATE_DETACHED 1
#define PTHREAD_CANCEL_ENABLE 0
#define PTHREAD_CANCEL_DISABLE 1
/* Mutex kinds. There is one implementation and it is not recursive; the constants
 * exist because libstdc++ names them when it builds `std::recursive_mutex`, and a
 * program that actually relocks one will deadlock rather than be lied to. */
#define PTHREAD_MUTEX_NORMAL 0
#define PTHREAD_MUTEX_RECURSIVE 1
#define PTHREAD_MUTEX_ERRORCHECK 2
#define PTHREAD_MUTEX_DEFAULT 0

int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*start)(void *), void *arg);
int pthread_join(pthread_t thread, void **retval);
int pthread_detach(pthread_t thread);
pthread_t pthread_self(void);
int pthread_equal(pthread_t a, pthread_t b);
_Noreturn void pthread_exit(void *retval);

int pthread_mutex_init(pthread_mutex_t *m, const pthread_mutexattr_t *attr);
int pthread_mutex_destroy(pthread_mutex_t *m);
int pthread_mutex_lock(pthread_mutex_t *m);
int pthread_mutex_trylock(pthread_mutex_t *m);
int pthread_mutex_unlock(pthread_mutex_t *m);

int pthread_cond_init(pthread_cond_t *c, const pthread_condattr_t *attr);
int pthread_cond_destroy(pthread_cond_t *c);
int pthread_cond_wait(pthread_cond_t *c, pthread_mutex_t *m);
int pthread_cond_timedwait(pthread_cond_t *c, pthread_mutex_t *m,
                           const struct timespec *abstime);
int pthread_cond_clockwait(pthread_cond_t *c, pthread_mutex_t *m, int clock,
                           const struct timespec *abstime);
int pthread_cond_signal(pthread_cond_t *c);
int pthread_cond_broadcast(pthread_cond_t *c);

int pthread_condattr_init(pthread_condattr_t *attr);
int pthread_condattr_destroy(pthread_condattr_t *attr);
int pthread_condattr_setclock(pthread_condattr_t *attr, int clock);

int pthread_rwlock_init(pthread_rwlock_t *l, const pthread_rwlockattr_t *attr);
int pthread_rwlock_destroy(pthread_rwlock_t *l);
int pthread_rwlock_rdlock(pthread_rwlock_t *l);
int pthread_rwlock_wrlock(pthread_rwlock_t *l);
int pthread_rwlock_unlock(pthread_rwlock_t *l);

int pthread_once(pthread_once_t *once, void (*routine)(void));
int pthread_key_create(pthread_key_t *key, void (*destructor)(void *));
int pthread_key_delete(pthread_key_t key);
int pthread_setspecific(pthread_key_t key, const void *value);
void *pthread_getspecific(pthread_key_t key);

int pthread_attr_init(pthread_attr_t *attr);
int pthread_attr_destroy(pthread_attr_t *attr);
int pthread_attr_setstacksize(pthread_attr_t *attr, size_t bytes);
int pthread_attr_getstacksize(const pthread_attr_t *attr, size_t *bytes);
int pthread_attr_setdetachstate(pthread_attr_t *attr, int state);
int pthread_attr_setschedpolicy(pthread_attr_t *attr, int policy);
int pthread_attr_getschedpolicy(const pthread_attr_t *attr, int *policy);
int pthread_attr_setinheritsched(pthread_attr_t *attr, int inherit);
int pthread_attr_getstack(const pthread_attr_t *attr, void **base, size_t *size);
int pthread_getattr_np(pthread_t thread, pthread_attr_t *attr);
int pthread_getname_np(pthread_t thread, char *name, size_t len);
int pthread_setcancelstate(int state, int *old);
int pthread_cancel(pthread_t thread);
void pthread_testcancel(void);

/* What libstdc++'s gthreads layer names. `pthread_mutexattr_*` are real (and
 * ignored — there is one kind of mutex); `pthread_mutex_timedlock` and the
 * cancellation hooks are declared without an implementation, so a program that
 * needs them fails at the link naming the one it wanted. */
int pthread_mutexattr_init(pthread_mutexattr_t *attr);
int pthread_mutexattr_destroy(pthread_mutexattr_t *attr);
int pthread_mutexattr_settype(pthread_mutexattr_t *attr, int type);
int pthread_mutex_timedlock(pthread_mutex_t *m, const struct timespec *abstime);
int pthread_mutex_clocklock(pthread_mutex_t *m, int clock, const struct timespec *abstime);
int pthread_rwlock_tryrdlock(pthread_rwlock_t *l);
int pthread_rwlock_trywrlock(pthread_rwlock_t *l);
int pthread_atfork(void (*prepare)(void), void (*parent)(void), void (*child)(void));
int sched_yield(void);

/* Live thread accounting, which no C library exposes and every debugging session
 * wants. Not POSIX; named so nobody mistakes it for it. */
unsigned long staros_threads_live(void);
unsigned long staros_thread_pointer(void);

#ifdef __cplusplus
}
#endif

#endif /* pthread.h */
