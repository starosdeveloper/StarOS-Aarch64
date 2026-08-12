/* time.h — one clock, monotonic, with its epoch at boot.
 *
 * `CLOCK_REALTIME` and `CLOCK_MONOTONIC` are the same counter here. That is a lie
 * of a known shape and it is deliberate: there is no battery-backed clock and no
 * network, so the only other honest answer is "unknown", after which a program has
 * nothing to stamp a log line with. Differences between readings are exact.
 */
#ifndef _STAROS_TIME_H
#define _STAROS_TIME_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef long time_t;
typedef int clockid_t;
typedef long clock_t;

struct timespec {
    time_t tv_sec;
    long tv_nsec;
};

struct timeval {
    time_t tv_sec;
    long tv_usec;
};

/* The two fields after `tm_isdst` are glibc's extension, and they are here because
 * leaving them out is not a simplification: libstdc++ and Qt were compiled against
 * glibc's headers, so their idea of this structure is 56 bytes. A caller that
 * allocates 36 and a `gmtime_r` that writes 56 corrupt whatever follows on the
 * stack, which is a fault somewhere else entirely. */
struct tm {
    int tm_sec, tm_min, tm_hour, tm_mday, tm_mon, tm_year, tm_wday, tm_yday, tm_isdst;
    long tm_gmtoff;
    const char *tm_zone;
};

#define CLOCK_REALTIME 0
#define CLOCK_MONOTONIC 1
#define CLOCKS_PER_SEC 1000000L

int clock_gettime(clockid_t clock, struct timespec *out);
int nanosleep(const struct timespec *request, struct timespec *remaining);
time_t time(time_t *out);
int gettimeofday(struct timeval *tv, void *tz);

/* The calendar. It is exact — the conversion is Hinnant's era arithmetic, tested
 * against every day for a century — but it is a *UTC* calendar: local time is UTC
 * here because there is no zone database to read and no environment to read TZ
 * from, and `tzname` says "UTC" rather than pretending otherwise. What the epoch
 * means is the clock's business, and this system's clock counts from boot. */
struct tm *localtime_r(const time_t *when, struct tm *out);
struct tm *gmtime_r(const time_t *when, struct tm *out);
struct tm *localtime(const time_t *when);
struct tm *gmtime(const time_t *when);
time_t mktime(struct tm *broken_down);
time_t timegm(struct tm *broken_down);
size_t strftime(char *s, size_t n, const char *format, const struct tm *broken_down);
double difftime(time_t a, time_t b);
clock_t clock(void);
char *ctime(const time_t *when);
char *asctime(const struct tm *broken_down);
int timespec_get(struct timespec *out, int base);
void tzset(void);
extern char *tzname[2];
extern long timezone;
extern int daylight;
#define TIME_UTC 1

#ifdef __cplusplus
}
#endif

#endif /* time.h */
