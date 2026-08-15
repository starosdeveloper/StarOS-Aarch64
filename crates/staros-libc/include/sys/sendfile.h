/* sys/sendfile.h — copy between two descriptors without the data reaching the
 * caller.
 *
 * Implemented in `crates/staros-libc/src/file.rs`. On Linux this is a kernel-side
 * copy and the point of it is that the bytes never enter the process; here it is a
 * loop through a bounded stack buffer, one page at a time, inside the C library.
 *
 * That difference is worth being plain about, because the name promises something
 * this version does not deliver: the copy is *not* zero-copy, and a caller choosing
 * `sendfile` over a read/write loop for that reason gains nothing. What it does
 * deliver is the interface's other guarantees, which are the ones QFile::copy
 * actually depends on — an explicit `offset` leaves the input descriptor's own
 * position undisturbed, and the return value is however much the write accepted, so
 * a short copy is resumable rather than lost.
 *
 * Zero-copy here would mean handing a page from the file server to the writer as a
 * capability, which is a real thing this kernel could do and a different piece of
 * work from this function.
 */
#ifndef _SYS_SENDFILE_H
#define _SYS_SENDFILE_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* `offset` may be null, meaning "use and advance the input descriptor's position".
 * When it is not null it is read for the starting offset and written back with the
 * position reached — and the descriptor's own position is left alone. */
ssize_t sendfile(int out_fd, int in_fd, off_t *offset, size_t count);
/* The large-file spelling, a real symbol and the same code — `off_t` is 64 bits
 * here either way. */
ssize_t sendfile64(int out_fd, int in_fd, off_t *offset, size_t count);

#ifdef __cplusplus
}
#endif

#endif /* sys/sendfile.h */
