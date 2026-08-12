/* wchar.h — the wide-character subset libstdc++ insists on.
 *
 * The C++ standard library's configuration on this host has `wchar_t` support
 * turned on, so `<string>` pulls in `<cwchar>` whether or not a program ever uses
 * a wide string. What it needs from it is a handful of pure functions over
 * `wchar_t` buffers — those are implemented; the stream and conversion halves are
 * declared only where libstdc++ names them, and `mbstate_t` is a real (if empty)
 * type rather than a lie about a conversion state nothing tracks.
 */
#ifndef _STAROS_WCHAR_H
#define _STAROS_WCHAR_H 1

#include <stdarg.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef __WINT_TYPE__ wint_t;

typedef struct {
    int __count;
    unsigned int __value;
} mbstate_t;

#define WEOF ((wint_t)-1)
#ifndef WCHAR_MIN
#define WCHAR_MIN __WCHAR_MIN__
#define WCHAR_MAX __WCHAR_MAX__
#endif

size_t wcslen(const wchar_t *s);
int wcscmp(const wchar_t *a, const wchar_t *b);
int wmemcmp(const wchar_t *a, const wchar_t *b, size_t n);
wchar_t *wmemcpy(wchar_t *dst, const wchar_t *src, size_t n);
wchar_t *wmemmove(wchar_t *dst, const wchar_t *src, size_t n);
wchar_t *wmemset(wchar_t *dst, wchar_t value, size_t n);
wchar_t *wmemchr(const wchar_t *s, wchar_t value, size_t n);
wchar_t *wcschr(const wchar_t *s, wchar_t value);

/* Multibyte conversion, in the one form libstdc++ calls: this system is UTF-8 in
 * and out, and these do the ASCII subset. Anything above 0x7f is refused rather
 * than truncated, because a silently wrong conversion is worse than a failure a
 * caller can see. */
size_t mbrtowc(wchar_t *out, const char *s, size_t n, mbstate_t *state);
size_t wcrtomb(char *out, wchar_t value, mbstate_t *state);
int mbsinit(const mbstate_t *state);

/* Everything below is *declared and not implemented*, and that is deliberate.
 *
 * libstdc++'s <cwchar> imports this whole list into namespace std and fails to
 * compile if a name is missing, so `<string>` cannot be included without them.
 * Declaring a function nobody calls costs nothing at link time; a program that
 * *does* call one gets an undefined symbol naming exactly what it wanted, which is
 * where this system would rather fail than in a stub returning zero.
 *
 * (This is the one place where the rule in features.h — declared means
 * implemented — is knowingly relaxed, because the alternative is patching a
 * generated c++config.h to turn wchar_t support off.)
 */
typedef struct _IO_FILE FILE;

wint_t btowc(int c);
int wctob(wint_t c);
wint_t fgetwc(FILE *stream);
wchar_t *fgetws(wchar_t *s, int n, FILE *stream);
wint_t fputwc(wchar_t c, FILE *stream);
int fputws(const wchar_t *s, FILE *stream);
int fwide(FILE *stream, int mode);
int fwprintf(FILE *stream, const wchar_t *format, ...);
int fwscanf(FILE *stream, const wchar_t *format, ...);
wint_t getwc(FILE *stream);
wint_t getwchar(void);
wint_t putwc(wchar_t c, FILE *stream);
wint_t putwchar(wchar_t c);
wint_t ungetwc(wint_t c, FILE *stream);
int swprintf(wchar_t *s, size_t n, const wchar_t *format, ...);
int swscanf(const wchar_t *s, const wchar_t *format, ...);
int vfwprintf(FILE *stream, const wchar_t *format, va_list args);
int vfwscanf(FILE *stream, const wchar_t *format, va_list args);
int vswprintf(wchar_t *s, size_t n, const wchar_t *format, va_list args);
int vswscanf(const wchar_t *s, const wchar_t *format, va_list args);
int vwprintf(const wchar_t *format, va_list args);
int vwscanf(const wchar_t *format, va_list args);
int wprintf(const wchar_t *format, ...);
int wscanf(const wchar_t *format, ...);
size_t mbrlen(const char *s, size_t n, mbstate_t *state);
size_t mbsrtowcs(wchar_t *dst, const char **src, size_t n, mbstate_t *state);
size_t wcsrtombs(char *dst, const wchar_t **src, size_t n, mbstate_t *state);
wchar_t *wcscat(wchar_t *dst, const wchar_t *src);
wchar_t *wcscpy(wchar_t *dst, const wchar_t *src);
wchar_t *wcsncat(wchar_t *dst, const wchar_t *src, size_t n);
wchar_t *wcsncpy(wchar_t *dst, const wchar_t *src, size_t n);
int wcsncmp(const wchar_t *a, const wchar_t *b, size_t n);
int wcscoll(const wchar_t *a, const wchar_t *b);
size_t wcsxfrm(wchar_t *dst, const wchar_t *src, size_t n);
size_t wcscspn(const wchar_t *s, const wchar_t *reject);
size_t wcsspn(const wchar_t *s, const wchar_t *accept);
wchar_t *wcspbrk(const wchar_t *s, const wchar_t *accept);
wchar_t *wcsrchr(const wchar_t *s, wchar_t value);
wchar_t *wcsstr(const wchar_t *haystack, const wchar_t *needle);
wchar_t *wcstok(wchar_t *s, const wchar_t *delim, wchar_t **state);
double wcstod(const wchar_t *s, wchar_t **end);
float wcstof(const wchar_t *s, wchar_t **end);
long double wcstold(const wchar_t *s, wchar_t **end);
long wcstol(const wchar_t *s, wchar_t **end, int base);
long long wcstoll(const wchar_t *s, wchar_t **end, int base);
unsigned long wcstoul(const wchar_t *s, wchar_t **end, int base);
unsigned long long wcstoull(const wchar_t *s, wchar_t **end, int base);
size_t wcsftime(wchar_t *s, size_t n, const wchar_t *format, const void *tm);

#ifdef __cplusplus
}
#endif

#endif /* wchar.h */
