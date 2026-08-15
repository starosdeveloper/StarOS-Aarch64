/* string.h — the memory and string functions, as `crates/staros-libc` implements
 * them.
 *
 * Every declaration here has an implementation in that crate. A header that
 * declared more would move the failure from the link, where it names the missing
 * symbol, to run time, where it does not.
 */
#ifndef _STAROS_STRING_H
#define _STAROS_STRING_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

void *memcpy(void *dst, const void *src, size_t n);
void *memmove(void *dst, const void *src, size_t n);
void *memset(void *dst, int byte, size_t n);
int memcmp(const void *a, const void *b, size_t n);
void *memchr(const void *s, int byte, size_t n);

size_t strlen(const char *s);
int strcmp(const char *a, const char *b);
int strncmp(const char *a, const char *b, size_t n);
char *strcpy(char *dst, const char *src);
char *strncpy(char *dst, const char *src, size_t n);
char *strcat(char *dst, const char *src);
char *strchr(const char *s, int c);
char *strrchr(const char *s, int c);
char *strstr(const char *haystack, const char *needle);
size_t strcspn(const char *s, const char *reject);
size_t strspn(const char *s, const char *accept);

/* There is one error, and it has one name. `strerror` exists because C++ streams
 * call it; giving every failure the same string is more honest than inventing a
 * table of errno values this system never sets. */
char *strerror(int code);

/* The rest of C's <string.h>. They are here first of all because <cstring> imports
 * the whole of it into namespace std and will not compile without the names — but
 * unlike when this note was first written, all but one now have an implementation
 * behind them in `crates/staros-libc/src/string.rs`. */
char *strncat(char *dst, const char *src, size_t n);
char *strtok(char *s, const char *delim);
/* The reentrant `strtok`, and the one worth using: the traversal state lives in the
 * caller's pointer instead of in a static, so two callers cannot corrupt each
 * other's walk. Implemented since the beginning and undeclared until Qt's
 * `qsimd.cpp` line 647 asked for it — the message was
 * `no member named 'strtok_r' in the global namespace; did you mean 'strtok'?`,
 * followed by a second error about the argument count, which is what happens when a
 * compiler takes a helpful guess. */
char *strtok_r(char *s, const char *delim, char **save);
char *strpbrk(const char *s, const char *accept);
size_t strxfrm(char *dst, const char *src, size_t n);
char *strdup(const char *s);
char *strndup(const char *s, size_t n);
void *memccpy(void *dst, const void *src, int c, size_t n);

/* Declared with nothing behind it, deliberately and alone.
 *
 * `strcoll` compares two strings in the current locale's collation order. This
 * system has the C locale and no other, where collation order *is* byte order — so a
 * correct `strcoll` here would be `strcmp`, and writing that would be a lie the day
 * a second locale exists. It is declared because <cstring> needs the name and left
 * undefined so that a program which actually calls it fails at the link, naming
 * `strcoll`, rather than silently getting C-locale ordering it did not ask for. */
int strcoll(const char *a, const char *b);

#ifdef __cplusplus
}
#endif

#endif /* string.h */
