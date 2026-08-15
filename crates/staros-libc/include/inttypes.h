/* inttypes.h — printing and parsing the fixed-width integers.
 *
 * The first thing Qt's own build asked for that this sysroot did not have. clang
 * ships an `inttypes.h` of its own, but it is a wrapper: it does `#include_next
 * <inttypes.h>` and expects the C library to supply the real one. Without it the
 * error names clang's file and points at a line that is doing the right thing.
 *
 * Almost all of this is macros, and macros are the reason the header exists: `%ld`
 * is right for `int64_t` on this target and wrong on a 32-bit one, so code that
 * wants to print an `int64_t` portably writes `PRId64` and lets the header decide.
 * Qt does, in thousands of places.
 */
#ifndef _INTTYPES_H
#define _INTTYPES_H 1

/* clang's own, which defines the exact-width types themselves. It needs no C
 * library and is the right source for them: the widths are the compiler's opinion
 * about the target, not this library's. */
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The widest integer this target has. 64 bits, and `intmax_t` is the name C gives
 * it — there is no 128-bit integer type in C here even though the machine has the
 * instructions. */
typedef long intmax_t;
typedef unsigned long uintmax_t;

/* The length modifiers, for this target's LP64: `long` is 64 bits and so is a
 * pointer, `int` is 32. Written out per width rather than derived, because the
 * derivation is what would be wrong on a different target and the values are what
 * a reader needs to check. */
#define __PRI64 "l"
#define __PRIPTR "l"

#define PRId8 "d"
#define PRId16 "d"
#define PRId32 "d"
#define PRId64 __PRI64 "d"
#define PRIdLEAST8 "d"
#define PRIdLEAST16 "d"
#define PRIdLEAST32 "d"
#define PRIdLEAST64 __PRI64 "d"
#define PRIdFAST8 "d"
#define PRIdFAST16 __PRI64 "d"
#define PRIdFAST32 __PRI64 "d"
#define PRIdFAST64 __PRI64 "d"
#define PRIdMAX __PRI64 "d"
#define PRIdPTR __PRIPTR "d"

#define PRIi8 "i"
#define PRIi16 "i"
#define PRIi32 "i"
#define PRIi64 __PRI64 "i"
#define PRIiLEAST8 "i"
#define PRIiLEAST16 "i"
#define PRIiLEAST32 "i"
#define PRIiLEAST64 __PRI64 "i"
#define PRIiFAST8 "i"
#define PRIiFAST16 __PRI64 "i"
#define PRIiFAST32 __PRI64 "i"
#define PRIiFAST64 __PRI64 "i"
#define PRIiMAX __PRI64 "i"
#define PRIiPTR __PRIPTR "i"

#define PRIu8 "u"
#define PRIu16 "u"
#define PRIu32 "u"
#define PRIu64 __PRI64 "u"
#define PRIuLEAST8 "u"
#define PRIuLEAST16 "u"
#define PRIuLEAST32 "u"
#define PRIuLEAST64 __PRI64 "u"
#define PRIuFAST8 "u"
#define PRIuFAST16 __PRI64 "u"
#define PRIuFAST32 __PRI64 "u"
#define PRIuFAST64 __PRI64 "u"
#define PRIuMAX __PRI64 "u"
#define PRIuPTR __PRIPTR "u"

#define PRIo8 "o"
#define PRIo16 "o"
#define PRIo32 "o"
#define PRIo64 __PRI64 "o"
#define PRIoMAX __PRI64 "o"
#define PRIoPTR __PRIPTR "o"

#define PRIx8 "x"
#define PRIx16 "x"
#define PRIx32 "x"
#define PRIx64 __PRI64 "x"
#define PRIxLEAST8 "x"
#define PRIxLEAST16 "x"
#define PRIxLEAST32 "x"
#define PRIxLEAST64 __PRI64 "x"
#define PRIxFAST8 "x"
#define PRIxFAST16 __PRI64 "x"
#define PRIxFAST32 __PRI64 "x"
#define PRIxFAST64 __PRI64 "x"
#define PRIxMAX __PRI64 "x"
#define PRIxPTR __PRIPTR "x"

#define PRIX8 "X"
#define PRIX16 "X"
#define PRIX32 "X"
#define PRIX64 __PRI64 "X"
#define PRIXMAX __PRI64 "X"
#define PRIXPTR __PRIPTR "X"

/* The scanning forms. `hh` and `h` really are needed here and not in the printing
 * forms: a variadic *argument* is promoted to `int`, so `%d` reads a whole one,
 * while a variadic *pointer* is not promoted and `scanf` must be told how many
 * bytes to write. Getting this wrong writes four bytes into a one-byte variable. */
#define SCNd8 "hhd"
#define SCNd16 "hd"
#define SCNd32 "d"
#define SCNd64 __PRI64 "d"
#define SCNdMAX __PRI64 "d"
#define SCNdPTR __PRIPTR "d"

#define SCNi8 "hhi"
#define SCNi16 "hi"
#define SCNi32 "i"
#define SCNi64 __PRI64 "i"
#define SCNiMAX __PRI64 "i"
#define SCNiPTR __PRIPTR "i"

#define SCNu8 "hhu"
#define SCNu16 "hu"
#define SCNu32 "u"
#define SCNu64 __PRI64 "u"
#define SCNuMAX __PRI64 "u"
#define SCNuPTR __PRIPTR "u"

#define SCNo8 "hho"
#define SCNo16 "ho"
#define SCNo32 "o"
#define SCNo64 __PRI64 "o"
#define SCNoMAX __PRI64 "o"
#define SCNoPTR __PRIPTR "o"

#define SCNx8 "hhx"
#define SCNx16 "hx"
#define SCNx32 "x"
#define SCNx64 __PRI64 "x"
#define SCNxMAX __PRI64 "x"
#define SCNxPTR __PRIPTR "x"

/* Quotient and remainder of the widest integers, together. */
typedef struct {
    intmax_t quot;
    intmax_t rem;
} imaxdiv_t;

intmax_t imaxabs(intmax_t value);
imaxdiv_t imaxdiv(intmax_t numer, intmax_t denom);
intmax_t strtoimax(const char *s, char **end, int base);
uintmax_t strtoumax(const char *s, char **end, int base);

#ifdef __cplusplus
}
#endif

#endif /* inttypes.h */
