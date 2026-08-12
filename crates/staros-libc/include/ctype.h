/* ctype.h — character classification for the C locale, which is the only one.
 *
 * These are the pure-computation half of the contract's layer 1: no table, no
 * locale, ASCII only. A byte above 0x7f is not a letter here, and that is the C
 * locale's answer rather than an approximation of one.
 */
#ifndef _STAROS_CTYPE_H
#define _STAROS_CTYPE_H 1

#ifdef __cplusplus
extern "C" {
#endif

int isalnum(int c);
int isalpha(int c);
int isblank(int c);
int iscntrl(int c);
int isdigit(int c);
int isgraph(int c);
int islower(int c);
int isprint(int c);
int ispunct(int c);
int isspace(int c);
int isupper(int c);
int isxdigit(int c);
int tolower(int c);
int toupper(int c);

#ifdef __cplusplus
}
#endif

#endif /* ctype.h */
