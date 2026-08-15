/* sys/file.h — BSD whole-file locking.
 *
 * `flock` is implemented in `crates/staros-libc/src/file.rs` and refuses with
 * `ENOLCK`. That is the honest answer rather than a gap: a lock is a promise about
 * what some *other* process will be prevented from doing, and nothing here can make
 * it — the filesystem is one read-only archive and the file server keeps no lock
 * table, so a second asker would be told yes as readily as the first.
 *
 * Returning 0 is the tempting answer, because it is every caller's happy path, and
 * it is exactly the wrong one: a caller that believes it holds an exclusive lock
 * goes on to do the thing the lock was protecting.
 *
 * Qt's `QLockFile` (`qlockfile_unix.cpp` line 24) is what asked. It reads the
 * failure as "could not acquire", which is true.
 */
#ifndef _SYS_FILE_H
#define _SYS_FILE_H 1

#include <fcntl.h>

#ifdef __cplusplus
extern "C" {
#endif

#define LOCK_SH 1  /* shared */
#define LOCK_EX 2  /* exclusive */
#define LOCK_NB 4  /* or-ed with either: fail rather than block */
#define LOCK_UN 8  /* release */

int flock(int fd, int operation);

#ifdef __cplusplus
}
#endif

#endif /* sys/file.h */
