/* stdlib.h — memory, conversion and exit, as `crates/staros-libc` implements them. */
#ifndef _STAROS_STDLIB_H
#define _STAROS_STDLIB_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

void *malloc(size_t size);
void *calloc(size_t count, size_t size);
void *realloc(void *ptr, size_t size);
void free(void *ptr);
void *aligned_alloc(size_t align, size_t size);
int posix_memalign(void **out, size_t align, size_t size);

_Noreturn void exit(int status);
_Noreturn void _Exit(int status);
_Noreturn void abort(void);
/* `atexit` is `__cxa_atexit` with a null object, and that is how it is
 * implemented — one list, one order, one place to get the ordering wrong. */
int atexit(void (*function)(void));

int atoi(const char *s);
long strtol(const char *s, char **end, int base);
int abs(int value);

/* The environment, all of it, implemented in `crates/staros-libc/src/proc.rs`.
 *
 * `putenv` is the odd one and its signature is why Qt found it: it takes a
 * *non-const* `char *` and keeps the caller's string rather than copying it, so the
 * caller's buffer becomes part of the environment and freeing it corrupts it.
 * `setenv` copies. Qt uses both — `qtenvironmentvariables.cpp` line 289 — and with
 * only `getenv` declared the compiler guessed, giving two errors from one cause:
 * `cannot initialize a variable of type 'int' with an rvalue of type 'char *'` on
 * line 287, then `use of undeclared identifier 'putenv'` on 289. */
char *getenv(const char *name);
/* Refuses to answer for a setuid program. There is no setuid here, so it is
 * `getenv` — declared because glibc has it and libraries reach for it. */
char *secure_getenv(const char *name);
int setenv(const char *name, const char *value, int overwrite);
int unsetenv(const char *name);
int putenv(char *entry);
int clearenv(void);

/* `realpath`: the absolute path with `.`, `..` and any symbolic links resolved.
 *
 * There are no symbolic links here, so the resolution is purely textual — which
 * makes it complete rather than approximate: with nothing to follow, normalising the
 * path *is* the answer. Implemented in `crates/staros-libc/src/file.rs`.
 *
 * `resolved` must point at `PATH_MAX` bytes (<limits.h>) or be null, in which case
 * the result is allocated and the caller frees it. Qt passes a `PATH_MAX` buffer —
 * `qfilesystemengine_unix.cpp` line 1966. */
char *realpath(const char *path, char *resolved);

/* The rest of C's <stdlib.h>, declared because libstdc++'s <cstdlib> imports the
 * whole of it into namespace std and will not compile without the names. Only the
 * ones above are implemented; calling one of these fails at the link, naming what
 * it wanted — see the same note in <wchar.h>. */
typedef struct {
    int quot;
    int rem;
} div_t;
typedef struct {
    long quot;
    long rem;
} ldiv_t;
typedef struct {
    long long quot;
    long long rem;
} lldiv_t;

double atof(const char *s);
long atol(const char *s);
long long atoll(const char *s);
double strtod(const char *s, char **end);
float strtof(const char *s, char **end);
long double strtold(const char *s, char **end);
long long strtoll(const char *s, char **end, int base);
unsigned long strtoul(const char *s, char **end, int base);
unsigned long long strtoull(const char *s, char **end, int base);
int rand(void);
void srand(unsigned seed);
void qsort(void *base, size_t count, size_t size, int (*compare)(const void *, const void *));
void *bsearch(const void *key, const void *base, size_t count, size_t size,
              int (*compare)(const void *, const void *));
int system(const char *command);
int at_quick_exit(void (*function)(void));
_Noreturn void quick_exit(int status);
long labs(long value);
long long llabs(long long value);
div_t div(int numerator, int denominator);
ldiv_t ldiv(long numerator, long denominator);
lldiv_t lldiv(long long numerator, long long denominator);
int mblen(const char *s, size_t n);
int mbtowc(wchar_t *out, const char *s, size_t n);
int wctomb(char *out, wchar_t value);
size_t mbstowcs(wchar_t *dst, const char *src, size_t n);
size_t wcstombs(char *dst, const wchar_t *src, size_t n);

#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 2147483647
#define MB_CUR_MAX 1

#ifdef __cplusplus
}
#endif

#endif /* stdlib.h */
