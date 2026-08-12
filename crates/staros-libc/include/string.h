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

/* Declared and not implemented, because <cstring> imports the whole of C's
 * <string.h> into namespace std and will not compile without the names — the same
 * arrangement as <wchar.h>, and for the same reason. A call to one of these is an
 * undefined symbol naming exactly what the program wanted. */
char *strncat(char *dst, const char *src, size_t n);
char *strtok(char *s, const char *delim);
char *strpbrk(const char *s, const char *accept);
int strcoll(const char *a, const char *b);
size_t strxfrm(char *dst, const char *src, size_t n);
char *strdup(const char *s);
void *memccpy(void *dst, const void *src, int c, size_t n);

#ifdef __cplusplus
}
#endif

#endif /* string.h */
