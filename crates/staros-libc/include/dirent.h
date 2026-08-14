/* dirent.h — walking a directory that does not exist.
 *
 * The initramfs is a CPIO archive, and CPIO stores *paths*, not directories.
 * `docs` is a directory because `docs/readme.txt` is a member; there is no entry
 * for it anywhere. So `readdir` here is a filter over the archive's member list —
 * the names one level below a prefix, each reported once — and `crates/staros-libc/
 * src/dir.rs` has the reasoning and the host tests.
 *
 * The consequence a caller can see: `d_ino` is an index into the archive, not an
 * inode. It is unique and stable for one boot and means nothing beyond that, which
 * is exactly what a program using it as a cache key needs to know.
 *
 * `struct dirent`'s offsets are glibc's — `d_reclen` at 16, `d_type` at 18,
 * `d_name` at 19 — because they are asserted at compile time on the Rust side and
 * a program reading `d_type` from the wrong byte finds every file a directory.
 */
#ifndef _DIRENT_H
#define _DIRENT_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

struct dirent {
    ino_t d_ino;
    off_t d_off;
    unsigned short d_reclen;
    unsigned char d_type;
    char d_name[256];
};

/* Large-file spelling of the same structure, for the same reason as `stat64`. */
#define dirent64 dirent

/* `d_type` values. Only two occur here: the archive holds files, and directories
 * exist because paths mention them. The rest are defined so a `switch` over them
 * compiles. */
#define DT_UNKNOWN 0
#define DT_FIFO 1
#define DT_CHR  2
#define DT_DIR  4
#define DT_BLK  6
#define DT_REG  8
#define DT_LNK  10
#define DT_SOCK 12

/* Opaque: the traversal state is a position in the archive plus the small list of
 * names already reported, and a program that could see it would come to depend on
 * a representation the flat-archive story is likely to change. */
typedef struct __staros_dir DIR;

DIR *opendir(const char *path);
struct dirent *readdir(DIR *dir);
struct dirent *readdir64(DIR *dir);
void rewinddir(DIR *dir);
int closedir(DIR *dir);
/* Refuses with `EBADF`. A directory here is not a descriptor — it is a filter over
 * a member list — so there is no number to hand back, and inventing one would let
 * a caller `fstat` it and be told about something else. */
int dirfd(DIR *dir);

#ifdef __cplusplus
}
#endif

#endif /* dirent.h */
