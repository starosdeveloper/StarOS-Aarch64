/* setjmp.h — the non-local jump, which is real here and written in assembly.
 *
 * `crates/staros-libc/src/proc.rs`, module `jump`. It could not be written in Rust:
 * saving and restoring the callee-saved registers *is* the operation, and a compiler
 * is entitled to assume a function returns exactly once.
 *
 * The buffer is glibc's `jmp_buf` — 22 words, 176 bytes, holding `x19`–`x28`, `x29`,
 * `x30`, `sp` and `d8`–`d15`, with glibc's pointer-mangling slot left unused. The
 * size and the offsets are the ABI: an object compiled against a Linux sysroot
 * allocated its buffer from *this* `sizeof`, and a smaller one here would have
 * `setjmp` write past the end of a caller's stack slot. The assembly and this
 * declaration have to agree, and the place they agree is the number below.
 *
 * libpng is what asked for it. Its error handling is `setjmp`/`longjmp` and nothing
 * else — `pngconf.h` line 50 — which is also why this path matters more here than on
 * a system with exceptions: qtbase for StarOS is built `-fno-exceptions`, so a
 * `longjmp` out of a decoder is the only unwinding there is.
 */
#ifndef _SETJMP_H
#define _SETJMP_H 1

#ifdef __cplusplus
extern "C" {
#endif

/* An array type, deliberately, and that is not a stylistic choice: because it is an
 * array, `jmp_buf` decays to a pointer when passed, so `setjmp(env)` writes into the
 * caller's buffer without the caller writing `&`. Every program that uses this is
 * written expecting that. */
typedef unsigned long __jmp_buf[22];

typedef struct {
    __jmp_buf __jb;
    /* glibc keeps the saved signal mask and a flag for whether one was saved. This
     * library saves neither — nothing here delivers a signal, so there is no mask
     * whose restoration could matter — but the members occupy their glibc offsets so
     * that the structure's size stays the ABI's. */
    unsigned long __fl;
    unsigned long __ss[128 / sizeof(long)];
} __jmp_buf_tag;

typedef __jmp_buf_tag jmp_buf[1];
typedef __jmp_buf_tag sigjmp_buf[1];

int setjmp(jmp_buf env);
/* Never returns, and the attribute is load-bearing rather than documentation: the
 * compiler must not keep the instructions after a `longjmp` call reachable, and
 * without this it emits a fall-through path that executes with the registers of a
 * frame that no longer exists. */
void longjmp(jmp_buf env, int value) __attribute__((__noreturn__));

/* The signal-mask forms. Identical here, because there is no mask to save — the
 * `save_mask` argument is accepted and ignored. `sigsetjmp` is a macro onto
 * `__sigsetjmp` because that is the name glibc's own header expands to, and objects
 * built elsewhere reference it. */
int __sigsetjmp(sigjmp_buf env, int save_mask);
#define sigsetjmp(env, save_mask) __sigsetjmp(env, save_mask)
void siglongjmp(sigjmp_buf env, int value) __attribute__((__noreturn__));

/* The underscore forms, which differ from the plain ones only in not touching the
 * signal mask — and nothing here touches it either way. */
int _setjmp(jmp_buf env);
void _longjmp(jmp_buf env, int value) __attribute__((__noreturn__));

#ifdef __cplusplus
}
#endif

#endif /* setjmp.h */
