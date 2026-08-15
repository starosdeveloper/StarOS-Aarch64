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
int truncate(const char *path, off_t length);

/* The large-file spellings, as real symbols rather than macros.
 *
 * `crates/staros-libc/src/file.rs` exports both names for each of these, so unlike
 * the `stat` family in <sys/stat.h> there is a definition to point a declaration at
 * and no reason to preprocess the name away. Either spelling is the same code —
 * `off_t` here has been 64 bits since the first version of this library.
 *
 * Qt writes `::lseek64` with the scope operator (`qfile.cpp` line 1103), which is
 * what makes a declaration necessary and what a `-D` on the command line could not
 * have fixed. */
off_t lseek64(int fd, off_t offset, int whence);
int ftruncate64(int fd, off_t length);
int truncate64(const char *path, off_t length);
int fsync(int fd);

int dup(int fd);
int dup2(int old_fd, int new_fd);
int pipe(int fds[2]);
/* `pipe2` takes the flags `pipe` would have needed a second call to set. Qt uses it
 * for exactly the reason its own comment gives — it is documented not to return
 * EINTR — and this system delivers no signals, so that is true here twice over.
 * The flags are accepted and ignored: `O_CLOEXEC` means nothing where there is no
 * `exec`, and `O_NONBLOCK` on a pipe end is a property `poll` already answers for. */
int pipe2(int fds[2], int flags);
int isatty(int fd);

/* Read-only filesystem: both refuse with EROFS. Declared because a program that
 * calls them wants a compile error at worst and a named errno at best, not a
 * missing symbol at link time in a file it did not write. */
int unlink(const char *path);
int rmdir(const char *path);
int access(const char *path, int mode);
int chdir(const char *path);
char *getcwd(char *buf, size_t size);
int fchdir(int fd);
ssize_t readlink(const char *path, char *buf, size_t size);
int symlink(const char *target, const char *link);
int link(const char *from, const char *to);

/* The `*at` forms, which take a directory descriptor a relative path is resolved
 * against. They refuse for the same reasons their plain counterparts do — the
 * filesystem is read-only — and they are declared because Qt calls them by name:
 * `qfilesystemengine_unix.cpp` reaches `unlinkat` twice, `linkat` once and
 * `renameat` twice, in code that has no plain-path fallback. */
int unlinkat(int dirfd, const char *path, int flags);
int linkat(int fromfd, const char *from, int tofd, const char *to, int flags);
int renameat(int fromfd, const char *from, int tofd, const char *to);
int renameat2(int fromfd, const char *from, int tofd, const char *to, unsigned int flags);
/* Plain `rename` is in <stdio.h>, where C puts it. `AT_REMOVEDIR`, which `unlinkat`
 * takes, is in <fcntl.h> with the other `AT_` flags. */

int dup3(int old_fd, int new_fd, int flags);

/* The process tree, which does not exist here.
 *
 * `fork` refuses with `ENOSYS` and the `exec` family with `EACCES`. They are
 * declared, and that is not a contradiction: a refusal is an implementation, and it
 * carries the errno that tells a caller which fallback to take. Qt's own
 * `qcore_unix_p.h` references every one of these from inline wrappers, so the
 * alternative to declaring them is that no Qt translation unit including it
 * compiles at all — which would be a stronger statement about this system than the
 * true one, that a process here cannot start another.
 *
 * `docs/LIBC-CONTRACT.md` records each refusal and its reason. */
pid_t fork(void);
pid_t vfork(void);
int execve(const char *path, char *const argv[], char *const envp[]);
int execv(const char *path, char *const argv[]);
int execvp(const char *file, char *const argv[]);

pid_t setsid(void);
pid_t getpgrp(void);

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
