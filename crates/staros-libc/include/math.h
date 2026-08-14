/* math.h — the mathematics of `crates/staros-libc/src/math.rs`.
 *
 * Everything declared here is implemented, with one block of exceptions at the end:
 * this target's `long double` is 128-bit quad precision, which nothing in this tree
 * computes. Those names are declared because libstdc++'s <cmath> imports them into
 * namespace std and will not compile otherwise; a program that actually calls one
 * fails at the link, naming it. That is the honest failure — a `sinl` secretly
 * computing in double precision would be right to fifteen digits and wrong to
 * thirty-three, which is the kind of bug that surfaces a year later. */
#ifndef _STAROS_MATH_H
#define _STAROS_MATH_H 1

#ifdef __cplusplus
extern "C" {
#endif

#define M_E 2.7182818284590452354
#define M_LOG2E 1.4426950408889634074
#define M_LOG10E 0.43429448190325182765
#define M_LN2 0.69314718055994530942
#define M_LN10 2.30258509299404568402
#define M_PI 3.14159265358979323846
#define M_PI_2 1.57079632679489661923
#define M_PI_4 0.78539816339744830962
#define M_1_PI 0.31830988618379067154
#define M_2_PI 0.63661977236758134308
#define M_2_SQRTPI 1.12837916709551257390
#define M_SQRT2 1.41421356237309504880
#define M_SQRT1_2 0.70710678118654752440

#define HUGE_VAL (__builtin_huge_val())
#define HUGE_VALF (__builtin_huge_valf())
#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))

/* The classification macros are the compiler's own: they are bit tests on the
 * argument, GCC and Clang both emit them inline, and a function call here would be
 * slower and no more correct. */
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#define isnormal(x) __builtin_isnormal(x)
#define signbit(x) __builtin_signbit(x)
#define fpclassify(x) \
    __builtin_fpclassify(FP_NAN, FP_INFINITE, FP_NORMAL, FP_SUBNORMAL, FP_ZERO, x)
#define isgreater(x, y) __builtin_isgreater(x, y)
#define isgreaterequal(x, y) __builtin_isgreaterequal(x, y)
#define isless(x, y) __builtin_isless(x, y)
#define islessequal(x, y) __builtin_islessequal(x, y)
#define islessgreater(x, y) __builtin_islessgreater(x, y)
#define isunordered(x, y) __builtin_isunordered(x, y)

#define FP_NAN 0
#define FP_INFINITE 1
#define FP_ZERO 2
#define FP_SUBNORMAL 3
#define FP_NORMAL 4

#define MATH_ERRNO 1
#define MATH_ERREXCEPT 2
/* Zero, and it is not a placeholder: this library reports domain errors through the
 * returned NaN alone. See the module comment in src/math.rs. */
#define math_errhandling 0

double fabs(double x);
double floor(double x);
double ceil(double x);
double trunc(double x);
double round(double x);
double rint(double x);
double nearbyint(double x);
double sqrt(double x);
double cbrt(double x);
double exp(double x);
double exp2(double x);
double expm1(double x);
double log(double x);
double log2(double x);
double log10(double x);
double log1p(double x);
double sin(double x);
double cos(double x);
double tan(double x);
double asin(double x);
double acos(double x);
double atan(double x);
double sinh(double x);
double cosh(double x);
double tanh(double x);
double asinh(double x);
double acosh(double x);
double atanh(double x);

double pow(double x, double y);
double fmod(double x, double y);
double remainder(double x, double y);
double atan2(double y, double x);
double hypot(double x, double y);
double copysign(double x, double y);
double fmax(double x, double y);
double fmin(double x, double y);
double fdim(double x, double y);
double fma(double x, double y, double z);

double ldexp(double x, int n);
double scalbn(double x, int n);
double frexp(double x, int *exponent);
double modf(double x, double *integral);
/* The GNU extension GCC emits by itself when a function computes both. */
void sincos(double x, double *sine, double *cosine);
void sincosf(float x, float *sine, float *cosine);

float fabsf(float x);
float floorf(float x);
float ceilf(float x);
float truncf(float x);
float roundf(float x);
float rintf(float x);
float sqrtf(float x);
float cbrtf(float x);
float expf(float x);
float expm1f(float x);
float logf(float x);
float log2f(float x);
float log10f(float x);
float log1pf(float x);
float sinf(float x);
float cosf(float x);
float tanf(float x);
float asinf(float x);
float acosf(float x);
float atanf(float x);
float sinhf(float x);
float coshf(float x);
float tanhf(float x);
float atanhf(float x);
float powf(float x, float y);
float fmodf(float x, float y);
float remainderf(float x, float y);
float atan2f(float y, float x);
float hypotf(float x, float y);
float copysignf(float x, float y);
float fmaxf(float x, float y);
float fminf(float x, float y);
float scalbnf(float x, int n);
float modff(float x, float *integral);

/* Declared, not implemented — see the note at the top of this file. */
long double fabsl(long double x);
long double floorl(long double x);
long double ceill(long double x);
long double truncl(long double x);
long double roundl(long double x);
long double rintl(long double x);
long double sqrtl(long double x);
long double cbrtl(long double x);
long double expl(long double x);
long double expm1l(long double x);
long double logl(long double x);
long double log2l(long double x);
long double log10l(long double x);
long double log1pl(long double x);
long double sinl(long double x);
long double cosl(long double x);
long double tanl(long double x);
long double asinl(long double x);
long double acosl(long double x);
long double atanl(long double x);
long double sinhl(long double x);
long double coshl(long double x);
long double tanhl(long double x);
long double asinhl(long double x);
long double acoshl(long double x);
long double atanhl(long double x);
long double powl(long double x, long double y);
long double fmodl(long double x, long double y);
long double remainderl(long double x, long double y);
long double atan2l(long double y, long double x);
long double hypotl(long double x, long double y);
long double copysignl(long double x, long double y);
long double fmaxl(long double x, long double y);
long double fminl(long double x, long double y);
long double fdiml(long double x, long double y);
long double fmal(long double x, long double y, long double z);
long double ldexpl(long double x, int n);
long double scalbnl(long double x, int n);
long double frexpl(long double x, int *exponent);
long double modfl(long double x, long double *integral);
long double nexttowardl(long double x, long double y);
double nexttoward(double x, long double y);
double nextafter(double x, double y);
float nextafterf(float x, float y);
double erf(double x);
double erfc(double x);
double lgamma(double x);
double tgamma(double x);
long lround(double x);
long lrint(double x);
long long llround(double x);
long long llrint(double x);
int ilogb(double x);
double logb(double x);

/* What `<cmath>` requires, and what it means that this list exists.
 *
 * libstdc++'s <cmath> writes `using ::erff;` for every name C99 defines, so a
 * declaration missing here fails the compile of any C++ file that includes it —
 * far from the mistake, in a header nobody edited. The list was not written from
 * memory: it is what clang named, one `no member named` at a time.
 *
 * The `long double` entries are declared and **not defined**, on purpose. On this
 * target `long double` is 128-bit quad, nothing here does quad arithmetic, and a
 * version that forwarded through `double` would answer with fifty-three bits of
 * precision to a caller that asked for a hundred and thirteen. Undefined fails at
 * the link, by name; docs/header-gap.txt lists every one with this reason. */

/* `float_t` and `double_t`: the types intermediate results are computed in.
 * FLT_EVAL_METHOD is 0 on AArch64 — float arithmetic really is done in float — so
 * these are the obvious types rather than the `double`-widened ones an x87 needs. */
typedef float float_t;
typedef double double_t;

float erff(float x);
float erfcf(float x);
float lgammaf(float x);
float tgammaf(float x);
long double erfl(long double x);
long double erfcl(long double x);
long double lgammal(long double x);
long double tgammal(long double x);

float exp2f(float x);
long double exp2l(long double x);
float acoshf(float x);
float asinhf(float x);
float fdimf(float x, float y);
float fmaf(float x, float y, float z);
float frexpf(float x, int *exponent);
float ldexpf(float x, int n);
float nearbyintf(float x);
long double nearbyintl(long double x);

float logbf(float x);
long double logbl(long double x);
int ilogbf(float x);
int ilogbl(long double x);

long lrintf(float x);
long lrintl(long double x);
long long llrintf(float x);
long long llrintl(long double x);
long lroundf(float x);
long lroundl(long double x);
long long llroundf(float x);
long long llroundl(long double x);

double nan(const char *tag);
float nanf(const char *tag);
long double nanl(const char *tag);

long double nextafterl(long double x, long double y);
float nexttowardf(float x, long double y);

double remquo(double x, double y, int *quo);
float remquof(float x, float y, int *quo);
long double remquol(long double x, long double y, int *quo);

double scalbln(double x, long n);
float scalblnf(float x, long n);
long double scalblnl(long double x, long n);

#ifdef __cplusplus
}
#endif

#endif /* math.h */
