/* sys/types.h — the integer names POSIX gives its own quantities.
 *
 * Nothing here is a function; it is the vocabulary the other four headers in this
 * directory are written in, and Qt's mkspec includes it before any of them.
 *
 * Every width is glibc's on aarch64, because that is the ABI `crates/staros-libc`
 * matches throughout — `struct stat`, `struct dirent` and `sigset_t` are laid out
 * to glibc's offsets, and a typedef that disagreed by a word would move every field
 * after it while every individual line still looked right.
 */
#ifndef _SYS_TYPES_H
#define _SYS_TYPES_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Guarded because <unistd.h> declares the same four, and a program may include
 * either first. C11 allows a repeated typedef; C++ does not, and libstdc++ pulls
 * both in. */
#ifndef __STAROS_POSIX_TYPES
#define __STAROS_POSIX_TYPES 1
typedef long ssize_t;
typedef long off_t;
typedef int pid_t;
typedef unsigned int uid_t;
typedef unsigned int gid_t;
#endif

typedef unsigned long dev_t;
typedef unsigned long ino_t;
typedef unsigned int mode_t;
typedef unsigned int nlink_t;
typedef long blksize_t;
typedef long blkcnt_t;
typedef unsigned long fsblkcnt_t;
typedef unsigned long fsfilcnt_t;

typedef long time_t;
typedef long suseconds_t;
typedef long clock_t;
/* `useconds_t` is unsigned even though `suseconds_t` is signed. That is not a
 * mistake here; it is POSIX's, and matching it is the point. */
typedef unsigned int useconds_t;

typedef off_t off64_t;
typedef ino_t ino64_t;
typedef blkcnt_t blkcnt64_t;

/* `pthread_t` and its relatives live in <pthread.h>, which is where a program
 * looks for them; repeating them here would be a second definition to keep in
 * step. */

#ifdef __cplusplus
}
#endif

#endif /* sys/types.h */
