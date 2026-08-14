/* sys/time.h — the microsecond pair, and the clock read through it.
 *
 * `struct timeval` is kept distinct from `struct timespec` rather than converted at
 * the boundary: the two differ by a factor of a thousand in a field with the same
 * shape, and every bug that mixes them is a delay a thousand times too long or too
 * short.
 *
 * `gettimeofday` reads the same monotonic counter `clock_gettime` does, because
 * this system has no battery-backed clock and no network — see the note on
 * `CLOCK_REALTIME` in `docs/LIBC-CONTRACT.md`. The epoch is boot; the *differences*
 * between readings, which is what almost every caller uses, are exact.
 */
#ifndef _SYS_TIME_H
#define _SYS_TIME_H 1

#include <sys/types.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

/* `struct timeval` itself is defined by <time.h>, which had it first and which
 * every program includes anyway. Repeating it here would be a second definition to
 * keep in step, and C++ does not permit one. */

/* Accepted and ignored by everything here: there are no time zones, and the
 * argument has been deprecated in POSIX for decades. */
struct timezone {
    int tz_minuteswest;
    int tz_dsttime;
};

int gettimeofday(struct timeval *tv, void *tz);

#ifdef __cplusplus
}
#endif

#endif /* sys/time.h */
