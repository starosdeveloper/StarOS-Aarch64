/* sys/shm.h — System V shared memory, which refuses. See <sys/ipc.h> for why.
 *
 * Short version: shared memory on this system is a delegated capability, and a
 * System V key is a global number. `shmget` returns -1 with `ENOSYS`.
 *
 * What a program that wants shared memory here uses instead is in <staros.h>:
 * `staros_shared_create` and `staros_shared_map`, over the kernel's `CreateShared`
 * and `MapShared`. That is what `services/displaysrv` and the QPA plugin use to move
 * a framebuffer between processes without copying it.
 */
#ifndef _SYS_SHM_H
#define _SYS_SHM_H 1

#include <sys/ipc.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SHM_RDONLY 010000
#define SHM_RND    020000
#define SHM_REMAP  040000

/* Never filled in — `shmctl` refuses before it would write one — but callers
 * declare one on the stack and pass its address, so it has to have a size. */
struct shmid_ds {
    struct ipc_perm shm_perm;
    size_t shm_segsz;
    time_t shm_atime;
    time_t shm_dtime;
    time_t shm_ctime;
    pid_t shm_cpid;
    pid_t shm_lpid;
    unsigned long shm_nattch;
};

int shmget(key_t key, size_t size, int flags);
/* `(void *)-1` on failure, not null — the same reasoning as `MAP_FAILED` in
 * <sys/mman.h>, and the value callers actually compare against. */
void *shmat(int id, const void *addr, int flags);
int shmdt(const void *addr);
int shmctl(int id, int cmd, struct shmid_ds *buf);

#ifdef __cplusplus
}
#endif

#endif /* sys/shm.h */
