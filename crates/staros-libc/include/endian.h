/* endian.h — which end the bytes come out of, and the conversions.
 *
 * No functions and no symbols: every name here is a macro over a compiler builtin or
 * over a macro the compiler already defines for the target. That is deliberate and
 * it is what makes this header correct by construction — the endianness is the
 * compiler's opinion about the machine it is generating code for, and this file only
 * gives it the names C code expects.
 *
 * Qt's bundled SHA-3 asked for it: `brg_endian.h` line 44 includes <endian.h> and
 * then decides its whole word-loading strategy from `__BYTE_ORDER`. A hash whose
 * implementation guesses the byte order wrong still produces digests — the wrong
 * ones, consistently — which is the kind of failure that survives a smoke test and
 * appears much later as a checksum mismatch against another machine.
 *
 * AArch64 as this system runs it is little-endian. That is not hard-coded below;
 * `__BYTE_ORDER__` is, and if this were ever built big-endian the definitions would
 * follow.
 */
#ifndef _ENDIAN_H
#define _ENDIAN_H 1

#include <stdint.h>

#define __LITTLE_ENDIAN 1234
#define __BIG_ENDIAN    4321
#define __PDP_ENDIAN    3412

/* The compiler's own answer, renamed. `__BYTE_ORDER__` and `__ORDER_*_ENDIAN__` are
 * defined by clang and gcc for every target; `__BYTE_ORDER` without the trailing
 * underscores is glibc's spelling and the one code reaches for. */
#if defined(__BYTE_ORDER__) && defined(__ORDER_LITTLE_ENDIAN__)
#  if __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
#    define __BYTE_ORDER __BIG_ENDIAN
#  else
#    define __BYTE_ORDER __LITTLE_ENDIAN
#  endif
#else
#  error "the compiler did not say which byte order this target uses"
#endif

/* The unprefixed spellings, which BSD-descended code uses. */
#define LITTLE_ENDIAN __LITTLE_ENDIAN
#define BIG_ENDIAN    __BIG_ENDIAN
#define PDP_ENDIAN    __PDP_ENDIAN
#define BYTE_ORDER    __BYTE_ORDER

/* The conversions. On a little-endian target the `le*` forms are identity and the
 * `be*` forms reverse; big-endian is the mirror. Written out per direction rather
 * than derived from one another, because the derivation is the part that gets
 * inverted by accident and the four lines are cheaper to read than to check. */
#if __BYTE_ORDER == __LITTLE_ENDIAN
#  define htole16(x) ((uint16_t)(x))
#  define htole32(x) ((uint32_t)(x))
#  define htole64(x) ((uint64_t)(x))
#  define le16toh(x) ((uint16_t)(x))
#  define le32toh(x) ((uint32_t)(x))
#  define le64toh(x) ((uint64_t)(x))
#  define htobe16(x) __builtin_bswap16((uint16_t)(x))
#  define htobe32(x) __builtin_bswap32((uint32_t)(x))
#  define htobe64(x) __builtin_bswap64((uint64_t)(x))
#  define be16toh(x) __builtin_bswap16((uint16_t)(x))
#  define be32toh(x) __builtin_bswap32((uint32_t)(x))
#  define be64toh(x) __builtin_bswap64((uint64_t)(x))
#else
#  define htole16(x) __builtin_bswap16((uint16_t)(x))
#  define htole32(x) __builtin_bswap32((uint32_t)(x))
#  define htole64(x) __builtin_bswap64((uint64_t)(x))
#  define le16toh(x) __builtin_bswap16((uint16_t)(x))
#  define le32toh(x) __builtin_bswap32((uint32_t)(x))
#  define le64toh(x) __builtin_bswap64((uint64_t)(x))
#  define htobe16(x) ((uint16_t)(x))
#  define htobe32(x) ((uint32_t)(x))
#  define htobe64(x) ((uint64_t)(x))
#  define be16toh(x) ((uint16_t)(x))
#  define be32toh(x) ((uint32_t)(x))
#  define be64toh(x) ((uint64_t)(x))
#endif

#endif /* endian.h */
