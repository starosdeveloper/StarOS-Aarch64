/* sys/stat.h — what a path is, and what may be done to it.
 *
 * `struct stat` is laid out to glibc's aarch64 offsets, byte for byte. That is not
 * a courtesy: `crates/staros-libc` fills it in from Rust with the same offsets
 * asserted at compile time, and a field moved by one word here would put `st_size`
 * where `st_blksize` is — every program still compiling, every file the wrong
 * length. `services/hello-c` checks the values that come back.
 *
 * Everything that *changes* the filesystem refuses with `EROFS`. The archive is
 * read-only and that is a property of the system rather than a gap in the library,
 * which is why the answer is `EROFS` and not `ENOSYS`: a program seeing `ENOSYS`
 * may conclude the libc is unfinished and try another path, while `EROFS` says no
 * path will work.
 */
#ifndef _SYS_STAT_H
#define _SYS_STAT_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

struct stat {
    dev_t st_dev;
    ino_t st_ino;
    mode_t st_mode;
    nlink_t st_nlink;
    uid_t st_uid;
    gid_t st_gid;
    dev_t st_rdev;
    unsigned long __pad1;
    off_t st_size;
    int st_blksize;
    int __pad2;
    long st_blocks;
    /* The three timestamps, as seconds and nanoseconds. glibc names them through
     * macros so that `st_atime` is the seconds field of `st_atim`; both spellings
     * appear in real code and both work below. */
    time_t st_atim_sec;
    long st_atim_nsec;
    time_t st_mtim_sec;
    long st_mtim_nsec;
    time_t st_ctim_sec;
    long st_ctim_nsec;
    unsigned int __unused[2];
};

/* The large-file names, all of them, as macros onto the plain ones.
 *
 * Large-file support is not a variant here: this target's `off_t` has been 64 bits
 * from the start, so `struct stat64` and `struct stat` describe one layout and
 * `fstat64` and `fstat` are one function. musl does exactly this, and the reason to
 * follow it rather than declare four more symbols is that a macro cannot drift —
 * there is no second declaration to keep in step with the first.
 *
 * Qt's mkspec defines `QT_USE_XOPEN_LFS_EXTENSIONS` and then writes `::fstat64`
 * with the scope operator, which is why the struct macro alone was not enough:
 * `qsysinfo.cpp` lines 251 and 331 stopped at `no member named 'fstat64' in the
 * global namespace`. The macro rewrites the name before the compiler looks it up, so
 * `::fstat64(fd, &st)` becomes `::fstat(fd, &st)` and resolves.
 *
 * `crates/staros-libc/src/file.rs` also exports the `64` names as real symbols, for
 * anything that reaches them without this header — a prebuilt object, a program that
 * declares them itself. They are aliases of the same code. */
#define stat64   stat
#define fstat64  fstat
#define lstat64  lstat
#define fstatat64 fstatat

#define st_atime st_atim_sec
#define st_mtime st_mtim_sec
#define st_ctime st_ctim_sec

/* File type, in `st_mode`. */
#define S_IFMT   0170000
#define S_IFSOCK 0140000
#define S_IFLNK  0120000
#define S_IFREG  0100000
#define S_IFBLK  0060000
#define S_IFDIR  0040000
#define S_IFCHR  0020000
#define S_IFIFO  0010000

#define S_ISREG(m)  (((m) & S_IFMT) == S_IFREG)
#define S_ISDIR(m)  (((m) & S_IFMT) == S_IFDIR)
#define S_ISLNK(m)  (((m) & S_IFMT) == S_IFLNK)
#define S_ISCHR(m)  (((m) & S_IFMT) == S_IFCHR)
#define S_ISBLK(m)  (((m) & S_IFMT) == S_IFBLK)
#define S_ISFIFO(m) (((m) & S_IFMT) == S_IFIFO)
#define S_ISSOCK(m) (((m) & S_IFMT) == S_IFSOCK)

/* Permission bits. Present because programs build modes out of them and compare
 * against them; nothing here enforces any of it, since there is one process
 * identity and a read-only filesystem. */
#define S_ISUID 04000
#define S_ISGID 02000
#define S_ISVTX 01000
#define S_IRWXU 00700
#define S_IRUSR 00400
#define S_IWUSR 00200
#define S_IXUSR 00100
#define S_IRWXG 00070
#define S_IRGRP 00040
#define S_IWGRP 00020
#define S_IXGRP 00010
#define S_IRWXO 00007
#define S_IROTH 00004
#define S_IWOTH 00002
#define S_IXOTH 00001

int stat(const char *path, struct stat *out);
int fstat(int fd, struct stat *out);
/* No symlinks exist here, so this is `stat` and says so rather than refusing:
 * a caller using `lstat` to avoid following a link gets the same answer either
 * way when there are none to follow. */
int lstat(const char *path, struct stat *out);
int fstatat(int dirfd, const char *path, struct stat *out, int flags);

/* All refuse with EROFS. Declared because a program that calls them deserves the
 * named errno rather than a missing symbol in a file it did not write. */
int mkdir(const char *path, mode_t mode);
int mkdirat(int dirfd, const char *path, mode_t mode);
int chmod(const char *path, mode_t mode);
int fchmod(int fd, mode_t mode);

#ifdef __cplusplus
}
#endif

#endif /* sys/stat.h */
