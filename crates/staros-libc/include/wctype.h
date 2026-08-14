/* wctype.h — classifying wide characters, in the "C" locale and no other.
 *
 * Found by QML. libstdc++'s <cwctype> is what QV4's lexer pulls in, so the
 * JavaScript engine does not compile without these nineteen names — which makes
 * them different in kind from the `wcs*` string family, declared here and left
 * undefined because libstdc++ requires the declarations and nothing calls them.
 *
 * A character above 127 classifies as **nothing**: false to every question, and
 * unchanged by `towlower`. That is what the C locale is, and it is the honest
 * answer rather than a small one — deciding whether U+00E9 is a letter needs the
 * Unicode character database, and an approximation would have a lexer accept
 * identifiers it should reject.
 */
#ifndef _WCTYPE_H
#define _WCTYPE_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* `wint_t` comes from the compiler's own `__WINT_TYPE__`, exactly as <wchar.h>
 * takes it. Writing `unsigned int` here instead was wrong on this target and, worse,
 * only wrong in one of the two headers — so a translation unit including both got a
 * redefinition and one including either alone did not. */
#ifndef __STAROS_WINT_DEFINED
#define __STAROS_WINT_DEFINED 1
typedef __WINT_TYPE__ wint_t;
#endif

/* Handles, not pointers: an integer makes an unknown class name a zero that
 * classifies nothing, rather than a null nobody checks. */
typedef int wctype_t;
typedef int wctrans_t;

#ifndef WEOF
#define WEOF ((wint_t)-1)
#endif

int iswalnum(wint_t c);
int iswalpha(wint_t c);
int iswblank(wint_t c);
int iswcntrl(wint_t c);
int iswdigit(wint_t c);
int iswgraph(wint_t c);
int iswlower(wint_t c);
int iswprint(wint_t c);
int iswpunct(wint_t c);
int iswspace(wint_t c);
int iswupper(wint_t c);
int iswxdigit(wint_t c);

wint_t towlower(wint_t c);
wint_t towupper(wint_t c);

/* The indirect forms: `iswctype(c, wctype("alpha"))` is `iswalpha(c)` with the
 * class chosen at run time. The twelve POSIX names and nothing else; anything else
 * is zero, and zero classifies nothing. */
wctype_t wctype(const char *name);
int iswctype(wint_t c, wctype_t class_);
wctrans_t wctrans(const char *name);
wint_t towctrans(wint_t c, wctrans_t trans);

#ifdef __cplusplus
}
#endif

#endif /* wctype.h */
