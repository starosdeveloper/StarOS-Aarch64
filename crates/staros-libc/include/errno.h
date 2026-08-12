/* errno.h — the numbers, and the honest note that nothing sets most of them.
 *
 * `errno` is a real thread-shared word behind `__errno_location`, and the values
 * below are Linux's so that code testing for `EAGAIN` compares against the number
 * this system's `pthread_create` actually returns. What is *not* claimed is that
 * every failing call sets one: most report failure through their return value and
 * leave `errno` alone, which is written down in docs/LIBC-CONTRACT.md.
 */
#ifndef _STAROS_ERRNO_H
#define _STAROS_ERRNO_H 1

#ifdef __cplusplus
extern "C" {
#endif

int *__errno_location(void);
#define errno (*__errno_location())

/* Linux's numbers, in full. The whole list is here because C++'s <system_error>
 * names every one of them when it builds `std::errc`, and because a subset would
 * be a second thing to keep in step with the first. The values match Linux so that
 * code comparing against `EAGAIN` sees the number `pthread_create` really returns. */
#define EPERM 1
#define ENOENT 2
#define ESRCH 3
#define EINTR 4
#define EIO 5
#define ENXIO 6
#define E2BIG 7
#define ENOEXEC 8
#define EBADF 9
#define ECHILD 10
#define EAGAIN 11
#define EWOULDBLOCK EAGAIN
#define ENOMEM 12
#define EACCES 13
#define EFAULT 14
#define ENOTBLK 15
#define EBUSY 16
#define EEXIST 17
#define EXDEV 18
#define ENODEV 19
#define ENOTDIR 20
#define EISDIR 21
#define EINVAL 22
#define ENFILE 23
#define EMFILE 24
#define ENOTTY 25
#define ETXTBSY 26
#define EFBIG 27
#define ENOSPC 28
#define ESPIPE 29
#define EROFS 30
#define EMLINK 31
#define EPIPE 32
#define EDOM 33
#define ERANGE 34
#define EDEADLK 35
#define ENAMETOOLONG 36
#define ENOLCK 37
#define ENOSYS 38
#define ENOTEMPTY 39
#define ELOOP 40
#define ENOMSG 42
#define EIDRM 43
#define ENOSTR 60
#define ENODATA 61
#define ETIME 62
#define ENOSR 63
#define ENOLINK 67
#define EPROTO 71
#define EMULTIHOP 72
#define EBADMSG 74
#define EOVERFLOW 75
#define EILSEQ 84
#define ENOTSOCK 88
#define EDESTADDRREQ 89
#define EMSGSIZE 90
#define EPROTOTYPE 91
#define ENOPROTOOPT 92
#define EPROTONOSUPPORT 93
#define EOPNOTSUPP 95
#define ENOTSUP EOPNOTSUPP
#define EAFNOSUPPORT 97
#define EADDRINUSE 98
#define EADDRNOTAVAIL 99
#define ENETDOWN 100
#define ENETUNREACH 101
#define ENETRESET 102
#define ECONNABORTED 103
#define ECONNRESET 104
#define ENOBUFS 105
#define EISCONN 106
#define ENOTCONN 107
#define ETIMEDOUT 110
#define ECONNREFUSED 111
#define EHOSTUNREACH 113
#define EALREADY 114
#define EINPROGRESS 115
#define ESTALE 116
#define EDQUOT 122
#define ECANCELED 125
#define EOWNERDEAD 130
#define ENOTRECOVERABLE 131

#ifdef __cplusplus
}
#endif

#endif /* errno.h */
