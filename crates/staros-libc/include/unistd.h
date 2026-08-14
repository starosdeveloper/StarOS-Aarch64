/* unistd.h — the POSIX calls this system has, and no others.
 *
 * Another header the library had implemented and never declared. Every name below
 * is a symbol `crates/staros-libc` defines today; the list was produced by asking
 * the built archive rather than by copying a glibc header, which is why it is short
 * and why nothing in it is a promise.
 *
 * Several of these refuse. `unlink` and `rmdir` answer `EROFS` because the
 * filesystem really is read-only; `readlink` answers `EINVAL` because nothing here
 * is a symlink; `chdir` answers `ENOENT`. A refusal with the errno that names the
 * reason is a fact a caller can act on — `docs/LIBC-CONTRACT.md` lists each with
 * its reason. What is *not* here is `fork`, `exec` and the process-tree calls: they
 * exist in the library and refuse, and they are declared where a caller expecting
 * them will look for them, not smuggled in through this header.
 */
#ifndef _UNISTD_H
#define _UNISTD_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef long ssize_t;
typedef long off_t;
typedef int pid_t;
typedef unsigned int uid_t;
typedef unsigned int gid_t;

/* `lseek`'s three origins. Spelled out because a program that hard-codes 0, 1, 2
 * is right by accident on every system and wrong on the first one that is not. */
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2

/* The standard descriptors. `stdin` is a console this system has no read side for,
 * so reading it yields nothing rather than blocking for input that cannot come. */
#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

/* `access` modes. */
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4

int close(int fd);
ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
off_t lseek(int fd, off_t offset, int whence);
int ftruncate(int fd, off_t length);
int fsync(int fd);

int dup(int fd);
int dup2(int old_fd, int new_fd);
int pipe(int fds[2]);
int isatty(int fd);

/* Read-only filesystem: both refuse with EROFS. Declared because a program that
 * calls them wants a compile error at worst and a named errno at best, not a
 * missing symbol at link time in a file it did not write. */
int unlink(const char *path);
int rmdir(const char *path);
int access(const char *path, int mode);
int chdir(const char *path);
char *getcwd(char *buf, size_t size);
ssize_t readlink(const char *path, char *buf, size_t size);
int symlink(const char *target, const char *link);

pid_t getpid(void);
pid_t getppid(void);
uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);

long sysconf(int name);
int getpagesize(void);

/* Ends the process without flushing or running handlers — the difference from
 * `exit`, and the reason both exist. */
void _exit(int status) __attribute__((__noreturn__));

#ifdef __cplusplus
}
#endif

#endif /* unistd.h */
