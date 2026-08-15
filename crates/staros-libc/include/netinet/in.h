/* netinet/in.h — the internet address families, types and constants only.
 *
 * Same standing as <sys/socket.h>, and for the same reason: Qt's mkspec includes it
 * unconditionally and qtbase here is built with `-no-feature-network`. No function is
 * declared, because none is implemented.
 *
 * The byte-order conversions are the one thing that could honestly live here as
 * code — they are pure computation on a value, needing no network at all — and they
 * are macros over the compiler's own builtins rather than functions, so they carry
 * no link-time promise. On this target, which is little-endian, they are a byte
 * reversal; the builtin makes that the compiler's opinion about the target instead
 * of this header's.
 */
#ifndef _NETINET_IN_H
#define _NETINET_IN_H 1

#include <stdint.h>
#include <sys/socket.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef uint16_t in_port_t;
typedef uint32_t in_addr_t;

#define INADDR_ANY       ((in_addr_t)0x00000000)
#define INADDR_BROADCAST ((in_addr_t)0xffffffff)
#define INADDR_LOOPBACK  ((in_addr_t)0x7f000001)
#define INADDR_NONE      ((in_addr_t)0xffffffff)

#define IPPROTO_IP   0
#define IPPROTO_ICMP 1
#define IPPROTO_TCP  6
#define IPPROTO_UDP  17
#define IPPROTO_IPV6 41

#define INET_ADDRSTRLEN  16
#define INET6_ADDRSTRLEN 46

struct in_addr {
    in_addr_t s_addr;
};

struct in6_addr {
    union {
        uint8_t __u6_addr8[16];
        uint16_t __u6_addr16[8];
        uint32_t __u6_addr32[4];
    } __in6_u;
};
#define s6_addr __in6_u.__u6_addr8

struct sockaddr_in {
    sa_family_t sin_family;
    in_port_t sin_port;
    struct in_addr sin_addr;
    /* Pads `sockaddr_in` out to the size of `sockaddr`, which is what lets one be
     * passed where the other is expected. Not decoration — the sockets interface is
     * built on the two being interchangeable through a cast. */
    unsigned char sin_zero[8];
};

struct sockaddr_in6 {
    sa_family_t sin6_family;
    in_port_t sin6_port;
    uint32_t sin6_flowinfo;
    struct in6_addr sin6_addr;
    uint32_t sin6_scope_id;
};

/* Host to network and back. Network order is big-endian; this target is
 * little-endian, so all four reverse bytes. Written through the compiler's builtins
 * so that the endianness is the target's answer rather than an assumption baked into
 * this file, and as macros so that nothing here needs a symbol at link time. */
#define htons(x) __builtin_bswap16((uint16_t)(x))
#define ntohs(x) __builtin_bswap16((uint16_t)(x))
#define htonl(x) __builtin_bswap32((uint32_t)(x))
#define ntohl(x) __builtin_bswap32((uint32_t)(x))

#ifdef __cplusplus
}
#endif

#endif /* netinet/in.h */
