/* sys/select.h — the older way to wait, over the newer one.
 *
 * `select` here is implemented on `poll` (see `crates/staros-libc/src/fd.rs`),
 * which is what it is on every modern system: a worse interface over the same
 * wait. Writing it any other way would mean a second implementation of the
 * readiness rules that could disagree with the first.
 *
 * Two of its properties are worth naming in the header, because both are why it is
 * the worse interface. The sets are **rewritten in place**, so a loop around
 * `select` must rebuild them every time round — the ones that forget spin. And
 * `nfds` is not a count of descriptors but the highest one **plus one**; a caller
 * passing the descriptor itself waits on everything except the one it meant.
 */
#ifndef _SYS_SELECT_H
#define _SYS_SELECT_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* glibc's size and layout: 1024 bits in 64-bit words. Part of the ABI, because a
 * caller's `fd_set` is this many bytes and a smaller one would be written past. */
#define FD_SETSIZE 1024

typedef struct {
    unsigned long __bits[FD_SETSIZE / 64];
} fd_set;

#define FD_ZERO(s)                                                             \
    do {                                                                       \
        unsigned long *__b = (s)->__bits;                                      \
        for (int __i = 0; __i < FD_SETSIZE / 64; __i++)                        \
            __b[__i] = 0;                                                      \
    } while (0)
#define FD_SET(fd, s)   ((s)->__bits[(fd) / 64] |= 1UL << ((fd) % 64))
#define FD_CLR(fd, s)   ((s)->__bits[(fd) / 64] &= ~(1UL << ((fd) % 64)))
#define FD_ISSET(fd, s) (((s)->__bits[(fd) / 64] >> ((fd) % 64)) & 1UL)

struct timeval;

int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds,
           struct timeval *timeout);

#ifdef __cplusplus
}
#endif

#endif /* sys/select.h */
