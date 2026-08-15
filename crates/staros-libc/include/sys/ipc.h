/* sys/ipc.h — the System V key, and the reason the rest of System V refuses.
 *
 * This system has both of the things System V IPC is for. They are not reachable
 * this way, and the reason is architectural rather than missing work.
 *
 * Shared memory here is a *capability*: `CreateShared` produces one, `MapShared`
 * places it, and a task can only map what was delegated to it. A System V key is a
 * number in a global namespace — any process that guessed 0x1234 would have the
 * memory. The two models cannot be reconciled by implementing `shmget`; a faithful
 * `shmget` would be a hole in the thing the kernel is built around.
 *
 * So `shmget` and its relatives fail with `ENOSYS`, which says "this interface does
 * not exist here" rather than "this call failed". `ftok` is implemented, because it
 * is pure computation over what `stat` reports and a caller is entitled to a key
 * even when nothing will accept one.
 *
 * Implementations in `crates/staros-libc/src/proc.rs`.
 */
#ifndef _SYS_IPC_H
#define _SYS_IPC_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int key_t;

#define IPC_PRIVATE ((key_t)0)

#define IPC_CREAT  01000
#define IPC_EXCL   02000
#define IPC_NOWAIT 04000

#define IPC_RMID 0
#define IPC_SET  1
#define IPC_STAT 2

/* The permissions header every System V object carries. Declared because callers
 * embed it in `struct shmid_ds`; never filled in, since nothing returns one. */
struct ipc_perm {
    key_t __key;
    uid_t uid;
    gid_t gid;
    uid_t cuid;
    gid_t cgid;
    unsigned short mode;
    unsigned short __seq;
};

key_t ftok(const char *path, int project);

#ifdef __cplusplus
}
#endif

#endif /* sys/ipc.h */
