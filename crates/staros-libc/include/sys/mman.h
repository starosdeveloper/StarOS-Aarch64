/* sys/mman.h — pages rather than bytes.
 *
 * `crates/staros-libc/src/mmap.rs` has implemented these since layer 2 and nothing
 * declared them. Qt's bundled PCRE2 found it: its JIT allocator asks for anonymous
 * memory before it asks for anything else.
 *
 * What this layer can and cannot do is unusual enough to be worth stating where a
 * caller reads it:
 *
 *   * Anonymous mapping is real, over the kernel's one memory syscall.
 *   * Mapping a **file** is refused with `ENODEV`. Faulting a file in on demand
 *     needs the kernel to know a page belongs to the file server, which is a phase
 *     of its own; reading the whole file into anonymous memory would work until
 *     something wrote to a `MAP_SHARED` mapping and expected the file to change.
 *   * `munmap` succeeds and **the pages stay**. This system has no unmap, and that
 *     is the one place in this library where the answer is not the whole truth — so
 *     it is measured rather than hidden: `staros_mmap_retained()` in <staros.h>
 *     reports how many bytes have been "unmapped" and are still there.
 *   * `mprotect` refuses `PROT_EXEC` with `EPERM`, and the refusal is load-bearing.
 *     The loader gives EL0 no way to make a page executable at run time, so a JIT —
 *     PCRE2's and QML's included — has to be told *no* at the call rather than
 *     discovering it as an instruction fetch from a non-executable page.
 */
#ifndef _SYS_MMAN_H
#define _SYS_MMAN_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

#define PROT_NONE  0x0
#define PROT_READ  0x1
#define PROT_WRITE 0x2
#define PROT_EXEC  0x4

#define MAP_SHARED    0x01
#define MAP_PRIVATE   0x02
#define MAP_FIXED     0x10
#define MAP_ANONYMOUS 0x20
#define MAP_ANON      MAP_ANONYMOUS
#define MAP_NORESERVE 0x4000

/* What `mmap` returns on failure, and it is -1 rather than null: address zero is a
 * legitimate mapping on some systems, so null could not be distinguished from
 * success. A caller comparing against null instead of this gets a wrong answer only
 * in the failing case, which is the one it was checking for. */
#define MAP_FAILED ((void *)-1)

/* `madvise` advice. All accepted, none acted on — advice is advice, and this
 * system has no page cache to advise about. */
#define MADV_NORMAL     0
#define MADV_RANDOM     1
#define MADV_SEQUENTIAL 2
#define MADV_WILLNEED   3
#define MADV_DONTNEED   4
#define MADV_FREE       8

/* `mremap` flags. */
#define MREMAP_MAYMOVE 1

void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset);
/* The large-file spelling, a real symbol and the same code. */
void *mmap64(void *addr, size_t length, int prot, int flags, int fd, off_t offset);
int munmap(void *addr, size_t length);
int mprotect(void *addr, size_t length, int prot);
int madvise(void *addr, size_t length, int advice);
void *mremap(void *addr, size_t old_length, size_t new_length, int flags, ...);

#ifdef __cplusplus
}
#endif

#endif /* sys/mman.h */
