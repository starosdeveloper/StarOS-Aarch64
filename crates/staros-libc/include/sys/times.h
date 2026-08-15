/* sys/times.h — CPU time accounting, which this kernel does not keep.
 *
 * Implemented in `crates/staros-libc/src/time.rs`. All four fields come back zero
 * and the return value is real elapsed time.
 *
 * That split is the point. The four fields exist to divide CPU time into user and
 * system, this process and its children, and nothing here measures any of that — so
 * zero is the true answer in the shape the interface allows, rather than an
 * invented one. The *return* value is elapsed time since an arbitrary past instant,
 * which this system does have, so a caller timing an interval by subtracting two
 * returns gets a correct answer. Returning `(clock_t)-1` would report a failure that
 * did not occur and send a caller down an error path for no reason.
 *
 * Qt asks through `qtimerinfo_unix.cpp` line 14.
 */
#ifndef _SYS_TIMES_H
#define _SYS_TIMES_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

struct tms {
    clock_t tms_utime;
    clock_t tms_stime;
    clock_t tms_cutime;
    clock_t tms_cstime;
};

/* `out` may be null, in which case only the return value is produced. */
clock_t times(struct tms *out);

#ifdef __cplusplus
}
#endif

#endif /* sys/times.h */
