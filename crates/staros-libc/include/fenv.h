/* fenv.h — the floating-point environment, which here is two hardware registers.
 *
 * `crates/staros-libc/src/fenv.rs`. Unusually for this library there is no service
 * behind any of it: FPCR holds the rounding mode, FPSR holds the sticky exception
 * flags, both are readable and writable from EL0, and every function below is a few
 * instructions on them. Nothing refuses.
 *
 * Qt found it the way it found the rest — `qlocale_tools.cpp` line 28 includes
 * <fenv.h> on any Linux target, and libstdc++'s own <fenv.h> is a wrapper that
 * `#include_next`s this one.
 *
 * The `FE_*` rounding constants are FPCR bit positions rather than 0 through 3.
 * That is glibc's choice on this architecture and it is copied here on purpose: an
 * object compiled elsewhere passes `0x400000` to `fesetround` meaning "toward
 * +infinity", and a library that had numbered them 0..3 would round the wrong way
 * without any diagnostic anywhere.
 *
 * One thing is not the hardware's answer, and it is written down rather than left to
 * be discovered: floating-point *traps* do not happen. AArch64's trap-enable bits
 * are optional in the architecture and read-as-zero on the cores this targets, so an
 * exception is always a sticky flag and never a signal. `feraiseexcept` sets the
 * flag directly for the same reason.
 */
#ifndef _FENV_H
#define _FENV_H 1

#ifdef __cplusplus
extern "C" {
#endif

/* The exception flags, at their FPSR bit positions. */
#define FE_INVALID   0x01
#define FE_DIVBYZERO 0x02
#define FE_OVERFLOW  0x04
#define FE_UNDERFLOW 0x08
#define FE_INEXACT   0x10
#define FE_ALL_EXCEPT 0x1f

/* The rounding modes, at their FPCR bit positions — see the note above. */
#define FE_TONEAREST  0x000000
#define FE_UPWARD     0x400000
#define FE_DOWNWARD   0x800000
#define FE_TOWARDZERO 0xc00000

/* Just the flags, for `fegetexceptflag`/`fesetexceptflag`. */
typedef unsigned int fexcept_t;

/* Both registers. 32 bits each although the system registers are 64: the upper
 * halves are architecturally reserved, and two `unsigned int`s is what a program
 * compiled against a Linux sysroot allocated. */
typedef struct {
    unsigned int __fpcr;
    unsigned int __fpsr;
} fenv_t;

/* The default environment — round to nearest, no flags raised. A null pointer
 * cannot be passed to `fesetenv`, so this has to be a real object; it is defined
 * alongside the functions. */
extern const fenv_t __fe_dfl_env;
#define FE_DFL_ENV (&__fe_dfl_env)

int fegetround(void);
/* Nonzero if `mode` is not one of the four — including a value that happens to fit
 * inside the rounding field but names nothing. */
int fesetround(int mode);

int feclearexcept(int mask);
/* Returns the *intersection* of `mask` and what is raised, not a boolean: a caller
 * asking about three exceptions wants to know which of them happened. */
int fetestexcept(int mask);
int feraiseexcept(int mask);

int fegetexceptflag(fexcept_t *out, int mask);
int fesetexceptflag(const fexcept_t *saved, int mask);

int fegetenv(fenv_t *out);
int fesetenv(const fenv_t *env);
/* Save the environment and clear the flags, so a block of arithmetic runs without
 * its exceptions reaching the caller's. */
int feholdexcept(fenv_t *out);
/* Restore an environment and re-raise whatever was raised in the meantime, in that
 * order — reading the flags before the restore overwrites them is the whole
 * function. */
int feupdateenv(const fenv_t *env);

#ifdef __cplusplus
}
#endif

#endif /* fenv.h */
