/* sys/socket.h — types and constants only. There are no sockets here, and no
 * function in this header is declared.
 *
 * The header exists because `mkspecs/linux-clang/qplatformdefs.h` includes it
 * unconditionally, before Qt knows or cares whether networking is configured in.
 * qtbase for this system is built with `-no-feature-network`, so nothing calls a
 * socket function — and nothing declares one either. A declaration with no
 * definition behind it is what `features.h` forbids and `scripts/header-check.sh`
 * counts, and the point of that rule is exactly this situation: a header that exists
 * to satisfy an include must not also promise an implementation.
 *
 * The consequence is worth stating plainly, because it is the shape of the thing
 * rather than a gap: a program that calls `socket()` here fails to *link*, naming
 * the symbol, at build time on the development machine. It does not build, ship, and
 * then fail on the board. That is the better of the two failures and it is the one
 * this header chooses.
 *
 * When there is a network stack — a service, not a syscall, like everything else
 * here — this header grows function declarations and `libc-progress.sh` gains a
 * column. Not before.
 */
#ifndef _SYS_SOCKET_H
#define _SYS_SOCKET_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef unsigned int socklen_t;
typedef unsigned short sa_family_t;

#define AF_UNSPEC 0
#define AF_UNIX   1
#define AF_LOCAL  1
#define AF_INET   2
#define AF_INET6  10

#define PF_UNSPEC AF_UNSPEC
#define PF_UNIX   AF_UNIX
#define PF_LOCAL  AF_LOCAL
#define PF_INET   AF_INET
#define PF_INET6  AF_INET6

#define SOCK_STREAM    1
#define SOCK_DGRAM     2
#define SOCK_RAW       3
#define SOCK_SEQPACKET 5
#define SOCK_CLOEXEC   02000000
#define SOCK_NONBLOCK  00004000

#define SOL_SOCKET 1

#define SO_REUSEADDR 2
#define SO_TYPE      3
#define SO_ERROR     4
#define SO_BROADCAST 6
#define SO_SNDBUF    7
#define SO_RCVBUF    8
#define SO_KEEPALIVE 9
#define SO_LINGER    13

#define MSG_OOB       0x01
#define MSG_PEEK      0x02
#define MSG_DONTWAIT  0x40
#define MSG_NOSIGNAL  0x4000

#define SHUT_RD   0
#define SHUT_WR   1
#define SHUT_RDWR 2

/* The generic address, sized so that any concrete family's address fits in it —
 * which is the whole trick of the sockets interface and the reason `sockaddr` is
 * always passed by pointer with a length beside it. */
struct sockaddr {
    sa_family_t sa_family;
    char sa_data[14];
};

struct sockaddr_storage {
    sa_family_t ss_family;
    unsigned long __align;
    char __padding[128 - sizeof(sa_family_t) - sizeof(unsigned long)];
};

struct linger {
    int l_onoff;
    int l_linger;
};

#ifdef __cplusplus
}
#endif

#endif /* sys/socket.h */
