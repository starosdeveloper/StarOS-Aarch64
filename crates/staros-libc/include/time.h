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

struct tm {
    int tm_sec, tm_min, tm_hour, tm_mday, tm_mon, tm_year, tm_wday, tm_yday, tm_isdst;
};

#define CLOCK_REALTIME 0
#define CLOCK_MONOTONIC 1
#define CLOCKS_PER_SEC 1000000L

int clock_gettime(clockid_t clock, struct timespec *out);
int nanosleep(const struct timespec *request, struct timespec *remaining);
time_t time(time_t *out);
int gettimeofday(struct timeval *tv, void *tz);

/* Declared for <ctime>, not implemented: a calendar needs a real-time clock. */
struct tm *localtime_r(const time_t *when, struct tm *out);
struct tm *gmtime_r(const time_t *when, struct tm *out);
time_t mktime(struct tm *broken_down);
size_t strftime(char *s, size_t n, const char *format, const struct tm *broken_down);
double difftime(time_t a, time_t b);
clock_t clock(void);
char *ctime(const time_t *when);
struct tm *localtime(const time_t *when);
struct tm *gmtime(const time_t *when);
char *asctime(const struct tm *broken_down);
int timespec_get(struct timespec *out, int base);
#define TIME_UTC 1

#ifdef __cplusplus
}
#endif

#endif /* time.h */
