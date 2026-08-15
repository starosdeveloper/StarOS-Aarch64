/* sys/statfs.h — what the filesystem says about itself.
 *
 * Implemented in `crates/staros-libc/src/file.rs`, and every field is measured
 * rather than invented. The archive is finite and in memory, so the file count and
 * the byte count come from walking it; the free counts are zero because nothing can
 * be written; `f_flags` carries `ST_RDONLY`, which is the reason.
 *
 * A caller asking "how much space is left" gets 0 and can act on it. That is the
 * whole value of answering honestly here: a library that reported a large free space
 * would have Qt choose this filesystem for a cache and fail on the first write.
 *
 * `struct statfs` is 120 bytes in the AArch64 layout, and `file.rs` asserts both the
 * size and two field offsets — `f_files` at 40, `f_namelen` at 64 — because this is
 * a structure a caller allocates and this library fills.
 */
#ifndef _SYS_STATFS_H
#define _SYS_STATFS_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* `RAMFS_MAGIC`, which is what this is: an archive unpacked into memory. Code that
 * switches on `f_type` recognises it. */
#define RAMFS_MAGIC 0x858458f6

/* `f_flags`. `ST_RDONLY` is always set here and is not a policy — there is no write
 * path to the archive at all. */
#define ST_RDONLY 1
#define ST_NOSUID 2

typedef struct {
    int __val[2];
} fsid_t;

struct statfs {
    long f_type;
    long f_bsize;
    unsigned long f_blocks;
    unsigned long f_bfree;
    unsigned long f_bavail;
    unsigned long f_files;
    unsigned long f_ffree;
    fsid_t f_fsid;
    long f_namelen;
    long f_frsize;
    long f_flags;
    long f_spare[4];
};

int statfs(const char *path, struct statfs *out);
int fstatfs(int fd, struct statfs *out);

/* The large-file spellings, as macros — the same arrangement as the `stat` family in
 * <sys/stat.h>, and chosen here for a reason declarations could not have covered.
 *
 * Qt uses `statfs64` as a *type*: `qfilesystemengine_unix.cpp` declares
 * `struct statfs64 st` on the stack. A function declaration alone leaves that
 * incomplete, and the error names a struct nobody defined —
 * `variable has incomplete type 'struct statfs64'` — followed by a second error
 * about assigning from it. The macro covers the struct and the function together,
 * which is the only form that answers both.
 *
 * Every field here is already 64 bits wide, so there is nothing for a separate
 * large-file variant to do. `crates/staros-libc/src/file.rs` exports the `64` names
 * as real symbols too, for objects that reach them without this header. */
#define statfs64  statfs
#define fstatfs64 fstatfs

#ifdef __cplusplus
}
#endif

#endif /* sys/statfs.h */
