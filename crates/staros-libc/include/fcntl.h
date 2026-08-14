/* fcntl.h — opening files, and the flags that say how.
 *
 * The flag values are Linux's on aarch64. They are not arbitrary here: `open` in
 * `crates/staros-libc` inspects them, and every program compiled against a Linux
 * sysroot has already been handed these numbers by its own headers — a different
 * `O_CREAT` would mean a program asking to create a file and this library reading
 * it as something else.
 *
 * Everything that would *write* is accepted at the call and refused at the answer,
 * with `EROFS`. The flag exists so the request can be expressed and named; the
 * filesystem being read-only is what makes it fail.
 */
#ifndef _FCNTL_H
#define _FCNTL_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Access mode: the low two bits, and they are a small integer rather than a bit
 * set. `O_RDONLY | O_WRONLY` is not `O_RDWR`, which is the mistake this comment
 * exists to prevent. */
#define O_RDONLY 00
#define O_WRONLY 01
#define O_RDWR   02
#define O_ACCMODE 03

#define O_CREAT     0100
#define O_EXCL      0200
#define O_NOCTTY    0400
#define O_TRUNC    01000
#define O_APPEND   02000
#define O_NONBLOCK 04000
#define O_DIRECTORY 040000
#define O_NOFOLLOW 0100000
#define O_CLOEXEC  02000000

/* `O_LARGEFILE` is zero and `open64` is `open`: `off_t` has been 64 bits on this
 * target from the start, so there is no small-file mode to opt out of. */
#define O_LARGEFILE 0
#define open64 open

/* The special "directory" for the `*at` family: relative to the working
 * directory. There is one directory here — the archive is flat and `chdir`
 * refuses — so every `*at` call takes this or refuses. */
#define AT_FDCWD (-100)
#define AT_SYMLINK_NOFOLLOW 0x100
#define AT_REMOVEDIR 0x200
#define AT_EMPTY_PATH 0x1000
/* `statx` asks for fields by mask. Nothing here consults it — the whole structure
 * is filled — and the constant exists so a caller can write what it means. */
#define AT_STATX_SYNC_AS_STAT 0x0000

/* `fcntl` commands. Only the descriptor-flag pair does anything; the locking
 * commands refuse, because a lock nothing enforces is worse than no lock. */
#define F_DUPFD  0
#define F_GETFD  1
#define F_SETFD  2
#define F_GETFL  3
#define F_SETFL  4
#define F_GETLK  5
#define F_SETLK  6
#define F_SETLKW 7
#define FD_CLOEXEC 1

int open(const char *path, int flags, ...);
int openat(int dirfd, const char *path, int flags, ...);
int fcntl(int fd, int cmd, ...);

#ifdef __cplusplus
}
#endif

#endif /* fcntl.h */
