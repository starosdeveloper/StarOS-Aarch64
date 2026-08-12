/* locale.h — one locale, named "C", which is the only one this system has.
 *
 * `setlocale` accepts the C locale and refuses everything else by returning null,
 * which is what a caller checks. Claiming success for a locale nobody implemented
 * would give a program the wrong decimal separator and no way to find out.
 */
#ifndef _STAROS_LOCALE_H
#define _STAROS_LOCALE_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define LC_ALL 6
#define LC_COLLATE 3
#define LC_CTYPE 0
#define LC_MONETARY 4
#define LC_NUMERIC 1
#define LC_TIME 2
#define LC_MESSAGES 5

struct lconv {
    char *decimal_point;
    char *thousands_sep;
    char *grouping;
    char *int_curr_symbol;
    char *currency_symbol;
    char *mon_decimal_point;
    char *mon_thousands_sep;
    char *mon_grouping;
    char *positive_sign;
    char *negative_sign;
    char int_frac_digits;
    char frac_digits;
    char p_cs_precedes;
    char p_sep_by_space;
    char n_cs_precedes;
    char n_sep_by_space;
    char p_sign_posn;
    char n_sign_posn;
};

char *setlocale(int category, const char *locale);
struct lconv *localeconv(void);

/* libstdc++ was configured against glibc, so its locale layer names glibc's
 * `__locale_t`. There is exactly one locale here and no object behind it, so the
 * type exists as an opaque pointer: enough for the headers to compile, and
 * pointing at nothing so a program that tries to use one cannot get a plausible
 * wrong answer. */
typedef struct __locale_struct *__locale_t;
typedef __locale_t locale_t;

#ifdef __cplusplus
}
#endif

#endif /* locale.h */
