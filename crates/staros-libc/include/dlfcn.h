/* dlfcn.h — the dynamic loader, which does not exist, declared so that programs can
 * find that out the way they are written to.
 *
 * There is no runtime loading here and there cannot be without a phase of work: the
 * kernel's ELF loader gives every `PT_LOAD` segment its final permissions at load
 * time and EL0 has no way to make a page executable afterwards. So `dlopen` returns
 * null and `dlerror` explains why in a sentence, which is exactly the contract every
 * caller of `dlopen` already handles — it is an interface whose failure path is the
 * normal one.
 *
 * This is why Qt is built static and why the platform plugin is linked in with
 * `Q_IMPORT_PLUGIN` rather than found in a directory at startup.
 *
 * `crates/staros-libc/src/proc.rs` holds the implementations.
 */
#ifndef _DLFCN_H
#define _DLFCN_H 1

#ifdef __cplusplus
extern "C" {
#endif

/* Accepted and ignored — there is nothing to bind lazily or globally. Present
 * because callers write them and a missing macro is a compile error rather than the
 * runtime refusal that is intended. */
#define RTLD_LAZY   0x00001
#define RTLD_NOW    0x00002
#define RTLD_GLOBAL 0x00100
#define RTLD_LOCAL  0x00000
#define RTLD_NODELETE 0x01000
#define RTLD_NOLOAD 0x00004

/* Handles for `dlsym` with no object of one's own. Also accepted and also answered
 * with null: this program's symbols were resolved at link time and there is no
 * symbol table left to search at run time. */
#define RTLD_DEFAULT ((void *)0)
#define RTLD_NEXT    ((void *)-1l)

/* What `dladdr` fills in. Never written to — `dladdr` returns 0, its own "not
 * found" — but the struct has to exist for callers to declare one. */
typedef struct {
    const char *dli_fname;
    void *dli_fbase;
    const char *dli_sname;
    void *dli_saddr;
} Dl_info;

void *dlopen(const char *path, int flags);
void *dlsym(void *handle, const char *symbol);
int dlclose(void *handle);
/* Null when nothing has failed since the last call, as POSIX requires — the
 * one-shot behaviour is real here, not approximated. */
const char *dlerror(void);
int dladdr(const void *addr, Dl_info *info);

#ifdef __cplusplus
}
#endif

#endif /* dlfcn.h */
