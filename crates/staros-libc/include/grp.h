/* grp.h — the group database, the same one row as <pwd.h>.
 *
 * Implemented in `crates/staros-libc/src/proc.rs` beside `getpwuid_r`. Qt reaches it
 * through `mkspecs/linux-clang/qplatformdefs.h` and then through
 * `QFileInfo::group()`.
 */
#ifndef _GRP_H
#define _GRP_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

struct group {
    char *gr_name;
    char *gr_passwd;
    gid_t gr_gid;
    /* NULL-terminated array of member names. Empty here — one user, and it is the
     * only member of its own group — but an empty array is still an array, so the
     * implementation reserves an aligned NULL inside the caller's buffer for this to
     * point at rather than returning null. A caller walking it finds the terminator
     * immediately, which is the difference between "no members" and a null
     * dereference. */
    char **gr_mem;
};

/* By id only. Lookup by name is absent for the reason given in <pwd.h>. */
int getgrgid_r(gid_t gid, struct group *out, char *buf, size_t len,
               struct group **result);
/* The static-buffer form, with the same caveat as `getpwuid`. Qt calls it from
 * `qfilesystemengine_unix.cpp` line 866, for `QFileInfo::group()`. */
struct group *getgrgid(gid_t gid);

#ifdef __cplusplus
}
#endif

#endif /* grp.h */
