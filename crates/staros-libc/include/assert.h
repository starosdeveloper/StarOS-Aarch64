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
