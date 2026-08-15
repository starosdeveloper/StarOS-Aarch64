/* assert.h — the first header Qt asks for, and the first one this sysroot lacked.
 *
 * `qglobal.h` includes it on line 20, before anything else of Qt's own, so nothing
 * of Qt compiles without it. It was found the way the C++ runtime's symbol list was
 * found: by handing a compiler the real headers and reading what it said.
 *
 * `assert` is a macro and must stay one — the standard says `NDEBUG` decides what it
 * expands to at every point of inclusion, so a function could not have the required
 * behaviour, and a program that includes this header twice with `NDEBUG` defined in
 * between is entitled to two different `assert`s.
 */
#ifndef _ASSERT_H
#define _ASSERT_H 1

#ifdef __cplusplus
extern "C" {
#endif

/* Report a failed assertion and end the program. Never returns.
 *
 * The name and signature are glibc's, because that is what the headers of every
 * library compiled against a Linux sysroot expand to — including libstdc++'s, which
 * this system's C++ programs use. A tidier name here would mean patching every one
 * of them. */
void __assert_fail(const char *expression, const char *file, unsigned int line,
                   const char *function) __attribute__((__noreturn__));

#ifdef __cplusplus
}
#endif

/* `static_assert` in C, which is this header's job and not the compiler's.
 *
 * C11 spells the keyword `_Static_assert` and puts the spelling everyone writes in
 * <assert.h> as a macro. C++ has had `static_assert` as a keyword since C++11 and
 * needs nothing here; C23 promoted it to a keyword too, and then the macro must not
 * exist, because `#define static_assert` over a keyword is what breaks a C23
 * translation unit.
 *
 * Qt found this. `qtypes.h` line 182, in its `#ifndef __cplusplus` half, opens with
 * `static_assert(sizeof(ptrdiff_t) == sizeof(size_t), ...)` — every C file in the
 * build reaches it through `qglobal.h`, and without the macro the compiler reads a
 * function declaration returning implicit int and stops three errors later at a
 * string literal where a parameter name should be. None of the three messages says
 * "assert.h".
 */
#if !defined(__cplusplus) && !defined(static_assert)
#if __STDC_VERSION__ < 202311L
#define static_assert _Static_assert
#endif
#endif

#endif /* _ASSERT_H */

/* Deliberately outside the include guard: the standard requires `assert` to be
 * redefined on every inclusion according to `NDEBUG` as it stands *then*. A guard
 * around this part would make the first inclusion win for the whole translation
 * unit, which is the one behaviour the standard singles out as wrong. */
#undef assert

#ifdef NDEBUG
#define assert(expression) ((void)0)
#else
#define assert(expression)                                                     \
    ((expression) ? (void)0                                                    \
                  : __assert_fail(#expression, __FILE__, __LINE__,             \
                                  __extension__ __PRETTY_FUNCTION__))
#endif
