/* pwd.h — the user database, which on this system has exactly one row.
 *
 * `crates/staros-libc/src/proc.rs` has answered these since layer 3. Qt reaches them
 * through `mkspecs/linux-clang/qplatformdefs.h`, which includes this header
 * unconditionally, and then through `QDir::homePath()` — which asks `getpwuid_r`
 * before it will fall back to `$HOME`.
 *
 * The single row is not a stub. There is one user here, uid 0, and its home is `/`
 * because the whole filesystem is one initramfs archive; a lookup for any other uid
 * returns 0 with a null `result`, which is POSIX for "no such user, and that is not
 * an error". The distinction matters to a caller that checks `errno`.
 */
#ifndef _PWD_H
#define _PWD_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Field order and names are glibc's. A program compiled against a Linux sysroot
 * reads these by name, and `QFileSystemEngine` reads `pw_name` and `pw_dir`. */
struct passwd {
    char *pw_name;
    char *pw_passwd;
    uid_t pw_uid;
    gid_t pw_gid;
    char *pw_gecos;
    char *pw_dir;
    char *pw_shell;
};

/* The reentrant forms, and the only ones implemented.
 *
 * The non-reentrant `getpwuid`/`getpwnam` return a pointer into a static buffer that
 * the next call overwrites. Qt calls these from whichever thread asks for a home
 * directory, so the static-buffer forms would be a data race that appears as a
 * corrupted path rather than as a crash. Declaring only the `_r` forms means a
 * caller that wants the other one fails at the compile rather than at run time.
 *
 * Lookup by *name* is absent for a different reason: nothing asks. Qt never calls
 * `getpwnam` — checked across `src/corelib` and `src/gui` in 6.11.1 — and a
 * declaration with nothing behind it is what `features.h` forbids and
 * `scripts/header-check.sh` counts. */
int getpwuid_r(uid_t uid, struct passwd *out, char *buf, size_t len,
               struct passwd **result);

/* And the static-buffer form after all, because Qt calls it:
 * `qfilesystemengine_unix.cpp` line 827, for `QFileInfo::owner()`.
 *
 * The returned pointer is into storage this library owns and the next call from any
 * thread overwrites it. The race that makes this interface notorious is unusually
 * toothless here — the answer never changes, so two threads racing write identical
 * bytes — but that is a reason it is tolerable, not a reason it is good. Code with a
 * choice should use `getpwuid_r`. */
struct passwd *getpwuid(uid_t uid);

#ifdef __cplusplus
}
#endif

#endif /* pwd.h */
