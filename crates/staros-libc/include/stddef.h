/* stddef.h — the types every other header here needs.
 *
 * clang ships its own <stddef.h>, but `-nostdlibinc` keeps the compiler's include
 * directory *and* drops the system one, and the two disagree about who defines
 * `size_t` first. Defining it here once, guarded the way the compiler's copy
 * guards it, keeps a single definition in play whichever order they are found in.
 */
#ifndef _STAROS_STDDEF_H
#define _STAROS_STDDEF_H 1

#ifndef _SIZE_T
#define _SIZE_T
typedef __SIZE_TYPE__ size_t;
#endif

#ifndef _PTRDIFF_T
#define _PTRDIFF_T
typedef __PTRDIFF_TYPE__ ptrdiff_t;
#endif

#ifndef _WCHAR_T
#define _WCHAR_T
#ifndef __cplusplus
typedef __WCHAR_TYPE__ wchar_t;
#endif
#endif

#ifndef NULL
#ifdef __cplusplus
#define NULL nullptr
#else
#define NULL ((void *)0)
#endif
#endif

#define offsetof(type, member) __builtin_offsetof(type, member)

typedef struct {
    long long __ll;
    long double __ld;
} max_align_t;

#endif /* stddef.h */
