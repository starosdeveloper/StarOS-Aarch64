/* staros.h — the calls that are this system rather than any system.
 *
 * Nothing here is portable and nothing here pretends to be. Every other header in
 * this sysroot declares something a C program could expect anywhere; this one
 * declares what a *platform plugin* needs, which is by definition the layer that
 * knows what machine it is on.
 *
 * The alternative to declaring it here is a plugin that issues `svc #0` itself,
 * which puts syscall numbers in two places. The last time a number in this system
 * lived in two places the two disagreed, and a server spent its life rejecting
 * messages it had no business receiving.
 *
 * Everything declared here is implemented in crates/staros-libc, or it is not
 * declared. A header that promised a function nobody wrote would move the failure
 * from the link, where it names the symbol, to run time, where it does not.
 */
#ifndef _STAROS_H
#define _STAROS_H 1

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- IPC ---------------------------------------------------------------- */

/* A message, exactly as the kernel carries it. Four words, not six: getting that
 * wrong puts `cap` sixteen bytes past where the kernel writes it, and the symptom
 * is a server that receives real messages and calls every one of them malformed. */
struct staros_message {
    unsigned long long tag;
    unsigned long long words[4];
    unsigned int cap;
};

/* Wrap an IPC endpoint capability in a descriptor `poll` can wait on.
 *
 * This is what lets a POSIX event loop see this system's own communication
 * primitive. Without it a program can serve messages or run an event loop, never
 * both. `poll` reports POLLIN while the endpoint's queue is not empty; a send-only
 * capability still yields a descriptor, which can be written and can never become
 * readable. Returns a negative errno on failure. */
int staros_endpoint_fd(unsigned int cap);

/* Send one message, carrying `msg->cap` if it is not zero. The kernel resolves the
 * handle out of this process's table and installs it in the receiver's, so the two
 * numbers differ and neither side may assume the other's. Returns 0 or -errno. */
int staros_msg_send(int fd, const struct staros_message *msg);

/* Receive one message, blocking until one arrives. A capability it carried is
 * already installed here and its new handle is in `msg->cap`.
 *
 * Blocking is the whole behaviour. A caller that does not want to block asks
 * `poll` first, which is where a timeout belongs and where every other source it
 * might wait on already is. Returns 0 or -errno. */
int staros_msg_recv(int fd, struct staros_message *msg);

/* ---- Shared buffers ----------------------------------------------------- */

/* Allocate `bytes` of memory that can be shared with another process, rounded up
 * to whole pages, and return its capability handle. Returns 0 on failure — 0 is
 * never a live handle, so a caller that forgets to check gets a refusal from the
 * first call that uses it rather than a wrong buffer.
 *
 * The pages are not mapped by this call: a program may hand a buffer to a server
 * without looking at it, and where a mapping lands is the kernel's answer rather
 * than something to assume. */
unsigned int staros_shared_create(size_t bytes);

/* Map a shared buffer here and return where it landed, or NULL.
 *
 * Mapping the same buffer twice returns the same address and costs nothing: the
 * kernel keeps one placement per object. A plugin that has lost a pointer may
 * simply ask again, which is cheaper than the bookkeeping that avoids asking. */
void *staros_shared_map(unsigned int cap);

/* Create a notification the kernel signals when this task exits, however it exits,
 * and return its capability handle (0 on failure).
 *
 * Meant to be delegated: put it in a message's `cap` and send it to a service that
 * must clean up after this process. A server cannot ask to watch a task that did
 * not offer, so the authority to be told about a death is something the dying party
 * hands out — and a service given one can take a crashed client's windows off the
 * screen instead of leaving them there for ever.
 *
 * Per *task*. Call it from the thread whose death means the program is over; a
 * worker thread ending is not the program ending. */
unsigned int staros_death_notification(void);

/* How many bytes a shared buffer holds; 0 for a handle that is not ours.
 *
 * A server must ask this rather than believe the message that delegated the
 * buffer. A size a client can lie about is one that makes the *server* run off the
 * end of a mapping and take the fault, which is the wrong process punished for the
 * client's arithmetic. */
size_t staros_shared_bytes(unsigned int cap);

/* ---- The display protocol ----------------------------------------------- */
/* Spoken over an endpoint descriptor to services/displaysrv. The reply tag is
 * STAROS_DISPLAY_OK with the result in words[0], or STAROS_DISPLAY_ERROR with a
 * reason there: "refused" and "did nothing" are different answers, and a client
 * that cannot tell them apart will one day show a blank window and call it a slow
 * frame. */

#define STAROS_DISPLAY_ERROR   0u  /* reply: words[0] = reason */
#define STAROS_DISPLAY_CREATE  1u  /* words[0..3] = w, h, x, y; cap = the pixels
                                    * reply: words[0] = surface id */
#define STAROS_DISPLAY_OK      2u  /* reply: words[0] = result */
#define STAROS_DISPLAY_COMMIT  3u  /* words[0] = surface, words[1] = damage x | y<<32,
                                    * words[2] = damage w | h<<32
                                    * reply: words[0] = pixels written */
#define STAROS_DISPLAY_RAISE   4u  /* words[0] = surface */
#define STAROS_DISPLAY_DESTROY 5u  /* words[0] = surface */
#define STAROS_DISPLAY_SCREEN  6u  /* reply: words[0..3] = width, height, bpp, format */
#define STAROS_DISPLAY_WATCH   7u  /* cap = staros_death_notification(); the server
                                    * takes this client's windows down if it dies */
#define STAROS_DISPLAY_BYE     9u  /* this client is done; its surfaces go with it */

/* Refusal reasons, in words[0] of a STAROS_DISPLAY_ERROR reply. */
#define STAROS_DISPLAY_ERR_MALFORMED  1u  /* unknown tag, no buffer, zero geometry,
                                           * or damage outside the surface */
#define STAROS_DISPLAY_ERR_NO_SURFACE 2u  /* never created, or already destroyed */
#define STAROS_DISPLAY_ERR_BUFFER     3u  /* smaller than the geometry claims */
#define STAROS_DISPLAY_ERR_FULL       4u  /* no room for another surface */

/* The only format composited today, in words[3] of a SCREEN reply. Named in the
 * reply so a client is told rather than assuming, and so the day a second format
 * exists the old clients are the ones that keep working. */
#define STAROS_FORMAT_XRGB8888 1u

/* ---- Input -------------------------------------------------------------- */
/* Decoded events arrive from services/inputsrv as messages: the tag is the event
 * kind, words[0] the code and words[1] the value. The numbers are Linux's, which
 * virtio-input passes through unchanged, so a consumer that already knows EV_KEY
 * needs no translation table. */

#define STAROS_INPUT_KEY 1u
#define STAROS_INPUT_REL 2u
#define STAROS_INPUT_ABS 3u

/* ---- Measurements ------------------------------------------------------- */

/* Bytes that munmap and mremap accounted for and the kernel still has mapped.
 * Zero means nothing has leaked yet, not that nothing can. Worth an assertion in
 * a resize loop: this system has no way to give pages back. */
size_t staros_mmap_retained(void);

/* Live heap bytes and blocks, for the same reason. */
void staros_heap_live(size_t *bytes, size_t *blocks);

#ifdef __cplusplus
}
#endif

#endif /* _STAROS_H */
