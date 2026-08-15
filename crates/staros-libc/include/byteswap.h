/* byteswap.h — reverse the bytes of an integer.
 *
 * Three macros over compiler builtins, no symbols. The builtins compile to a single
 * `rev16`, `rev32` or `rev` instruction on this target, so there is nothing a
 * function here could add except a call.
 *
 * glibc's companion to <endian.h>, and asked for by the same file — Qt's bundled
 * SHA-3, `brg_endian.h` line 46.
 */
#ifndef _BYTESWAP_H
#define _BYTESWAP_H 1

#include <stdint.h>

#define bswap_16(x) __builtin_bswap16((uint16_t)(x))
#define bswap_32(x) __builtin_bswap32((uint32_t)(x))
#define bswap_64(x) __builtin_bswap64((uint64_t)(x))

#endif /* byteswap.h */
