/* limits.h — the system's limits, on top of the compiler's.
 *
 * clang ships a <limits.h> holding everything the *compiler* knows: `INT_MAX`,
 * `CHAR_BIT`, the width of every integer type. Those are properties of the target,
 * not of this library, and it would be wrong to restate them here — a second copy is
 * a second chance to disagree.
 *
 * Which of the two files an `#include <limits.h>` reaches depends on search order,
 * and the order here is the reverse of the usual one. On a distribution, the
 * compiler's header comes first and ends with `#include_next <limits.h>` to pick up
 * the C library's. Here `-isystem` puts this sysroot *ahead* of clang's resource
 * directory, so this file is found first and has to reach forward instead — which is
 * what the `#include_next` below does. Writing it the other way round compiles until
 * something asks for `INT_MAX`, and then the error names a constant no header in
 * this tree was ever going to define.
 */
#include_next <limits.h>

/* What follows is only what the *system* decides, and each number is a real one:
 * `PATH_MAX` is what this library's own path handling accepts, `OPEN_MAX` is the
 * descriptor table's length. A program that sizes a buffer from one of these gets a
 * buffer that is actually big enough, which is the entire reason the header exists.
 *
 * Qt found it in `qfilesystemengine_unix.cpp` line 1966, sizing the buffer it hands
 * to `realpath`.
 */
#ifndef _LIMITS_H_STAROS
#define _LIMITS_H_STAROS 1

/* The longest path this library will accept, and the number its own buffers are
 * sized to. glibc's value, and matched on purpose: a program that allocated
 * `PATH_MAX` bytes against a Linux sysroot and passes the buffer here must not find
 * this library willing to write more. */
#define PATH_MAX 4096
/* One component of a path. */
#define NAME_MAX 255

/* How many descriptors can be open at once: the 32-entry pool in
 * `crates/staros-libc/src/fd.rs` plus the three standard streams, which exist
 * without occupying a pool entry. `getrlimit(RLIMIT_NOFILE)` reports the same
 * number from the same constant — it used to report 32, three short, and a program
 * that trusted it would have stopped opening files with three still available. */
#define OPEN_MAX 35
/* POSIX's floor for the same thing, which a program may use instead. */
#define _POSIX_OPEN_MAX 20

/* Atomic pipe write. There are no pipes with a capacity here, so this is the
 * minimum POSIX permits rather than a measurement. */
#define PIPE_BUF 4096

/* Argument list and environment. There is no `exec`, so no argument list is ever
 * assembled for one; the value is POSIX's minimum, present because programs use it
 * to size buffers. */
#define ARG_MAX 4096

/* Symbolic-link chasing. Nothing here is a symbolic link, so a chase of any length
 * terminates immediately; the constant exists for loops written against it. */
#define SYMLOOP_MAX 8

/* Login and terminal names. No logins and no terminals, but `getlogin_r` and
 * `ttyname_r` take a size and callers get it from here. */
#define LOGIN_NAME_MAX 256
#define TTY_NAME_MAX 32

/* Host name length, as `uname`'s `nodename` field allows. Not glibc's 64: the field
 * this library fills is 65 bytes including the terminator, and this is the length
 * that actually fits. */
#define HOST_NAME_MAX 64

#endif /* limits.h */
