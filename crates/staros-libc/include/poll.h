/* poll.h — waiting on a set of descriptors.
 *
 * `crates/staros-libc/src/fd.rs` has implemented `poll` and `ppoll` since layer 6,
 * and nothing declared them: `services/hello-c` wrote its own `struct pollfd` by
 * hand, which works and hides the fact that the header was missing. Qt found it —
 * `QEventDispatcherUNIX` is a `poll` over a descriptor set, so this is the header
 * the whole event loop stands on.
 *
 * The structure's layout is Linux's, because that is the ABI this library matches
 * throughout: `fd` then two shorts, and the flag values are the ones every program
 * compiled against a Linux sysroot has already been given by its own headers.
 */
#ifndef _POLL_H
#define _POLL_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* What a descriptor is watched for and what happened to it. */
struct pollfd {
    int fd;         /* negative means "skip this entry", not "descriptor zero" */
    short events;   /* requested */
    short revents;  /* returned */
};

/* How many descriptors a call may name. `unsigned long` on this target, and named
 * because POSIX names it — a program that declares its own array bound in terms of
 * this type keeps working if it ever changes. */
typedef unsigned long nfds_t;

#define POLLIN   0x001  /* there is data to read */
#define POLLPRI  0x002  /* urgent data — nothing here ever reports it */
#define POLLOUT  0x004  /* writing would not block */
#define POLLERR  0x008  /* an error: always reported, never requested */
#define POLLHUP  0x010  /* the other end closed: likewise */
#define POLLNVAL 0x020  /* the descriptor names nothing: likewise */

/* Wait until one of `fds` is ready or `timeout_ms` passes.
 *
 * A negative timeout waits for ever and zero does not wait at all. Confusing the
 * two gives an event loop that either spins or hangs, which is why both are spelled
 * out here rather than left to the reader.
 *
 * Returns how many entries have a non-zero `revents`, 0 on timeout, or -1. */
int poll(struct pollfd *fds, nfds_t nfds, int timeout_ms);

/* As `poll`, with the timeout as a `timespec` and a signal mask.
 *
 * The mask is accepted and ignored: this system delivers no signals, so there is
 * nothing to block for the duration of the wait. That is written down rather than
 * hidden, because a program using `ppoll` specifically to close the race between
 * checking a flag and sleeping is a program relying on the part that is absent. */
struct timespec;
int ppoll(struct pollfd *fds, nfds_t nfds, const struct timespec *timeout,
          const void *sigmask);

#ifdef __cplusplus
}
#endif

#endif /* poll.h */
