/* features.h — the header glibc uses to describe itself, and the first one a C++
 * standard library asks for.
 *
 * This system is not glibc, but libstdc++ decides what to compile by asking these
 * macros, so it has to be answered honestly: no GNU extensions, a C library that
 * claims C11 and POSIX, and no locale or wide-character machinery beyond the
 * little that crates/staros-libc actually implements.
 *
 * Everything declared through this sysroot is implemented in that crate, or it is
 * not declared at all. A header that promised a function nobody wrote would move
 * the failure from the link (where it names the symbol) to run time (where it does
 * not).
 */
#ifndef _FEATURES_H
#define _FEATURES_H 1

/* Tell libstdc++ it is looking at a hosted implementation: the freestanding
 * subset excludes <vector> and <string>, which are exactly what the phase needs. */
#define __STAROS__ 1
#define __STDC_HOSTED__ 1

/* No glibc. The version macros exist because libstdc++ tests them with #ifdef and
 * takes the "some other libc" path when they are absent, which is what we want. */
#undef __GLIBC__
#undef __GNU_LIBRARY__
/* libstdc++ asks this with `#if` in places that have not already tested
 * `__GLIBC__`, so it must exist and must answer "older than anything". */
#define __GLIBC_PREREQ(major, minor) 0

#define _POSIX_SOURCE 1
#define _POSIX_C_SOURCE 200809L
#define __USE_POSIX 1
#define __USE_POSIX199309 1
#define __USE_XOPEN2K 1
#define __USE_XOPEN2K8 1
#define __USE_ISOC99 1
#define __USE_ISOC11 1

/* glibc spells these on every declaration; libstdc++'s headers use some of them
 * directly, so they must exist even though they expand to nothing here. */
#define __BEGIN_DECLS
#define __END_DECLS
#define __THROW
#define __THROWNL
#define __NTH(fct) fct
#define __nonnull(params)
#define __attribute_pure__
#define __attribute_malloc__
#define __wur
/* `__restrict` and `__extension__` are deliberately *not* defined here: clang
 * understands both as keywords, and defining them as macros rewrote parameter
 * names inside libstdc++'s own headers. */

#endif /* features.h */
