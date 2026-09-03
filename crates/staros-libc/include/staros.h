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

/* Receive one message, giving up after `timeout_ms` milliseconds. Returns 0 on
 * delivery, -EAGAIN if the time passed with nothing to take, or another -errno.
 *
 * `poll` is still the right answer when several sources are in play — that is what
 * an event loop is — but a client waiting for the answer to one request does not
 * need a loop, and until this existed the only bounded wait was a notification
 * bound to the endpoint plus `poll`: three calls to say "wait, but not forever".
 *
 * The bound is honest under preemption: milliseconds here become an absolute
 * deadline at the syscall, so a caller descheduled between asking and waiting does
 * not quietly wait longer than it meant to. */
int staros_msg_recv_timeout(int fd, struct staros_message *msg, int timeout_ms);

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

/* How long this process has been asleep *inside* `poll`, in nanoseconds, and how
 * many times it parked there. Either pointer may be null.
 *
 * Time asleep, not time in the call: a `poll` that finds a descriptor already ready
 * never parks and adds nothing. That distinction is the whole point. A loop that
 * spends sixteen milliseconds between frames waiting for its next animation tick is
 * working as designed; a loop *busy* for the same milliseconds is a defect, and from
 * inside a toolkit the two look identical — both are "the dispatcher has not
 * returned yet".
 *
 * The counters are process-wide and never reset. Sample twice and subtract, which
 * is what makes them usable per frame. */
void staros_poll_wait(unsigned long long *asleep_ns, unsigned long long *parks);

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
#define STAROS_DISPLAY_FOCUS   8u  /* claim the keyboard; input goes here until
                                    * another client claims it or this one dies.
                                    * Separate from RAISE on purpose: putting a
                                    * window in front and taking the keystroke
                                    * someone is mid-way through typing are
                                    * different acts. */
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
/* Events arrive from services/displaysrv on the event endpoint — never from the
 * driver directly. The compositor is the only process that knows which window is
 * where, so it is the only one that can say which of these is yours.
 *
 * A key goes to whoever claimed STAROS_DISPLAY_FOCUS. A pointer event goes to
 * whatever is under the pointer, which is routinely a different window: clicking
 * on a window that is not focused is what that means. */

#define STAROS_INPUT_KEY 1u     /* words[0] = Linux key code, words[1] = 1 down /
                                 * 0 up. The code is the driver's, undisturbed:
                                 * renumbering here would put a translation table
                                 * in every client for a mapping that exists. */
#define STAROS_INPUT_POINTER 4u /* words[0] = x | y<<32, in *this surface's* pixels
                                 * words[1] = x | y<<32, on the screen
                                 * words[2] = which buttons are down, as a mask
                                 * words[3] = the surface it is over
                                 *
                                 * Both coordinate spaces, because a client acts in
                                 * its own and drags in the screen's, and it has
                                 * never been told where its window is.
                                 *
                                 * The buttons are the whole state and not a change:
                                 * a client that receives "button 1 went down" has
                                 * to remember the rest to know whether this is a
                                 * drag, and a client that missed one message stays
                                 * wrong for ever. This one recovers on the next. */

/* Bits of the button mask in words[2] of a pointer message. */
#define STAROS_BUTTON_LEFT   1u
#define STAROS_BUTTON_RIGHT  2u
#define STAROS_BUTTON_MIDDLE 4u

/* ---- CPU affinity ------------------------------------------------------- */

/* The core this thread is on at the instant of the call.
 *
 * A snapshot. An unpinned thread may be somewhere else before the value is
 * returned, and no amount of locking would change that — "which core am I on" has
 * no stable answer for a thread that has not said where it wants to be. */
int staros_cpu_id(void);

/* Restrict this thread to the cores in `mask` (bit n = core n) and return the
 * mask of cores that are ONLINE. A mask of 0 sets nothing and asks only for that
 * second number, so the usual shape is:
 *
 *     unsigned long long cores = (unsigned long long)staros_set_affinity(0);
 *     if (cores & (1ull << 2)) staros_set_affinity(1ull << 2);
 *
 * Returns -1 if the mask names no online core, leaving this thread's affinity
 * unchanged: a thread pinned to a core that never came up never runs again, and
 * this call is the last place anyone can be told.
 *
 * On success with a non-zero mask the call does not return until this thread is on
 * a permitted core, so staros_cpu_id() straight afterwards reads the new one.
 *
 * Not spelled sched_setaffinity: that one takes a cpu_set_t, and a 1024-bit set
 * with a macro family around it would claim a portability this system does not
 * have. MAX_CPUS here is eight, so the mask is one integer. */
long long staros_set_affinity(unsigned long long mask);

/* ---- Scheduling classes ------------------------------------------------- */

/* Which band this thread is picked in when several runnable threads want the same
 * core. Affinity says WHERE a thread may run; this says IN WHAT ORDER. */
#define STAROS_CLASS_QUERY   0  /* change nothing, report the current class */
#define STAROS_CLASS_LATENCY 1  /* picked first: a compositor, an input drain */
#define STAROS_CLASS_NORMAL  2  /* the default, and where anything unsure belongs */
#define STAROS_CLASS_BULK    3  /* picked when nothing above wants the core */

/* Put this thread in `band` and return the class it was in, or -1 for a value
 * that names no class (the class is then unchanged). STAROS_CLASS_QUERY only
 * reports, the way a zero mask does in staros_set_affinity.
 *
 * Strict bands with one exception: a latency thread that is runnable is picked
 * before a normal one on the same core, every time, EXCEPT on one pick in every
 * eight per core, which serves the lowest band that has anything runnable. That
 * exception is why a thread that declares itself latency-class and then never
 * blocks cannot stop the machine — the bottom band keeps an eighth of every core
 * no matter what anything above it claims.
 *
 * Per thread, not per process, and not inherited: a program with a render thread
 * and a worker thread is exactly the program whose threads want different
 * answers.
 *
 * The parameter is spelled `band` and not `class` on purpose: this header is
 * included from C++ too, where `class` is a keyword and a declaration using it
 * does not compile. */
int staros_set_class(int band);

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
