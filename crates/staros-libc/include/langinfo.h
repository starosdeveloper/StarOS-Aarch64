/* langinfo.h — the locale's answers, item by item.
 *
 * Implemented in `crates/staros-libc/src/locale.rs`. There is one locale here, C,
 * and one interesting answer in the whole header:
 *
 *   `CODESET` is "UTF-8", not the C locale's nominal "ANSI_X3.4-1968".
 *
 * That is a decision about this system rather than a copy of glibc's table. The
 * console, the file server and the initramfs all carry bytes through unchanged, so a
 * program that writes UTF-8 gets exactly what it wrote back. Answering "ASCII" would
 * make Qt transcode every non-Latin string into question marks — and Qt does ask:
 * `qcoreapplication.cpp` line 590 reads `nl_langinfo(CODESET)` at start-up and
 * decides the whole application's text codec from it. That single line is why this
 * header exists.
 *
 * The item numbers are glibc's, and they are not sequential: glibc packs the locale
 * category into the high bits, so `RADIXCHAR` is 65536 rather than 15. Copying the
 * numbers is what makes an object built elsewhere agree with this library about
 * which question it is asking.
 */
#ifndef _LANGINFO_H
#define _LANGINFO_H 1

#include <locale.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int nl_item;

/* LC_CTYPE. */
#define CODESET 14

/* LC_NUMERIC — the category number in the high bits, as glibc has it. */
#define RADIXCHAR 65536
#define THOUSEP   65537
#define DECIMAL_POINT RADIXCHAR
#define THOUSANDS_SEP THOUSEP

/* LC_TIME. */
#define ABDAY_1 131072
#define DAY_1   131079
#define ABMON_1 131086
#define MON_1   131098
#define AM_STR  131110
#define PM_STR  131111
#define D_T_FMT 131112
#define D_FMT   131113
#define T_FMT   131114

/* LC_MESSAGES. */
#define YESEXPR 327680
#define NOEXPR  327681

/* An unknown item yields the empty string, never null — C says so, and every caller
 * dereferences the result without checking. The returned pointer is into static
 * storage the caller must not modify or free. */
char *nl_langinfo(nl_item item);

#ifdef __cplusplus
}
#endif

#endif /* langinfo.h */
