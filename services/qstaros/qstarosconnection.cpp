#include "qstarosconnection.h"

#include <errno.h>
#include <poll.h>
#include <string.h>
#include <unistd.h>

// The capabilities the kernel installs for a display client, in order. The same
// numbers `services/hello-c` uses, and for the same reason they are constants
// rather than a lookup: the order *is* the ABI, and there is nothing to ask.
enum {
    EpDisplay = 3,
    EpDisplayReply = 4,
    EpEvents = 5,
};

// How long to wait for the server's answer before deciding there is not one.
//
// Not "for ever". A display capability is installed whether or not the machine has
// a framebuffer — the kernel does not know what it booted on — so a plugin that
// blocked here would hang on every headless board, and the hang would be reported
// as Qt failing to start with no further detail. Two seconds is far longer than a
// live server takes and short enough that a person watching a boot sees it end.
static const int ReplyTimeoutMs = 2000;

QStarosConnection::QStarosConnection()
{
    m_requestFd = staros_endpoint_fd(EpDisplay);
    m_replyFd = staros_endpoint_fd(EpDisplayReply);
    if (m_requestFd < 0 || m_replyFd < 0)
        return;

    // Input is optional. A machine with no keyboard still has a screen, and a
    // plugin that refused to start without one would be refusing to draw because
    // nobody can type.
    m_eventFd = staros_endpoint_fd(EpEvents);

    struct staros_message msg;
    memset(&msg, 0, sizeof msg);
    msg.tag = STAROS_DISPLAY_SCREEN;
    if (!call(&msg))
        return;

    m_screenSize = QSize(int(msg.words[0]), int(msg.words[1]));
    m_screenDepth = int(msg.words[2]);

    // The format is checked rather than assumed. The server names it in the reply
    // precisely so that the day a second one exists, the client that does not know
    // it refuses instead of drawing nonsense in the wrong channel order.
    if (msg.words[3] != STAROS_FORMAT_XRGB8888)
        return;
    if (m_screenSize.isEmpty() || m_screenDepth != 32)
        return;

    // The kernel signals this when this task exits, however it exits. Handing it to
    // the display server is what lets a crashed application's windows come off the
    // screen instead of staying there for ever — and it is delegated rather than
    // asked for, because nothing can ask to watch a task that did not offer.
    unsigned int death = staros_death_notification();
    if (death != 0) {
        memset(&msg, 0, sizeof msg);
        msg.tag = STAROS_DISPLAY_WATCH;
        msg.cap = death;
        call(&msg);
    }

    m_valid = true;
}

QStarosConnection::~QStarosConnection()
{
    if (m_valid) {
        struct staros_message msg;
        memset(&msg, 0, sizeof msg);
        msg.tag = STAROS_DISPLAY_BYE;
        call(&msg);
    }
    if (m_eventFd >= 0)
        close(m_eventFd);
    if (m_replyFd >= 0)
        close(m_replyFd);
    if (m_requestFd >= 0)
        close(m_requestFd);
}

bool QStarosConnection::call(struct staros_message *msg)
{
    // Anything outstanding is collected first. The reply endpoint is a queue and
    // every request in this protocol answers, so a `Raise` sent while a commit's
    // answer was still in flight would read the commit's reply as its own — and the
    // symptom of that is not an error but a plausible wrong number: a `createSurface`
    // returning the pixel count of the last frame as a surface id.
    collectCommit();

    if (staros_msg_send(m_requestFd, msg) != 0)
        return false;

    // Wait with a bound. This is what the event loop is for — asking whether an
    // answer is there — and using it here as well means one blocking primitive in
    // the plugin rather than two.
    struct pollfd wait_reply;
    wait_reply.fd = m_replyFd;
    wait_reply.events = POLLIN;
    wait_reply.revents = 0;
    if (poll(&wait_reply, 1, ReplyTimeoutMs) != 1)
        return false;
    if (staros_msg_recv(m_replyFd, msg) != 0)
        return false;
    return msg->tag == STAROS_DISPLAY_OK;
}

quint64 QStarosConnection::createSurface(unsigned int bufferCap, const QSize &size,
                                         const QPoint &at)
{
    struct staros_message msg;
    memset(&msg, 0, sizeof msg);
    msg.tag = STAROS_DISPLAY_CREATE;
    msg.words[0] = (unsigned long long)size.width();
    msg.words[1] = (unsigned long long)size.height();
    msg.words[2] = (unsigned long long)at.x();
    msg.words[3] = (unsigned long long)at.y();
    msg.cap = bufferCap;
    if (!call(&msg))
        return 0;
    return msg.words[0];
}

// Fill in a `Commit` message. One place, because the two senders below differ only
// in whether they wait for the answer, and a packing written twice is a packing that
// is fixed once.
static void packCommit(struct staros_message *msg, quint64 surface, const QRect &damage,
                       unsigned int bufferCap)
{
    memset(msg, 0, sizeof *msg);
    msg->tag = STAROS_DISPLAY_COMMIT;
    msg->words[0] = surface;
    // Damage is packed two 32-bit values to a word because a message has four words
    // and a rectangle plus a surface id needs five.
    msg->words[1] = (unsigned long long)(unsigned)damage.x()
                    | ((unsigned long long)(unsigned)damage.y() << 32);
    msg->words[2] = (unsigned long long)(unsigned)damage.width()
                    | ((unsigned long long)(unsigned)damage.height() << 32);
    msg->cap = bufferCap;
}

quint64 QStarosConnection::commit(quint64 surface, const QRect &damage, unsigned int bufferCap)
{
    struct staros_message msg;
    packCommit(&msg, surface, damage, bufferCap);
    if (!call(&msg))
        return 0;
    return msg.words[0];
}

bool QStarosConnection::postCommit(quint64 surface, const QRect &damage, unsigned int bufferCap)
{
    // The previous one is collected before this one goes out, so that the invariant
    // of one outstanding commit holds at the send rather than being checked after
    // the fact. In the ordinary frame there is nothing to collect here — the window
    // collected it before it painted — and this costs one branch.
    collectCommit();

    struct staros_message msg;
    packCommit(&msg, surface, damage, bufferCap);
    if (staros_msg_send(m_requestFd, &msg) != 0)
        return false;
    m_commitPending = true;
    return true;
}

quint64 QStarosConnection::collectCommit()
{
    if (!m_commitPending)
        return 0;
    // Cleared before the wait, not after it. A timed-out reply is a reply that may
    // still arrive, and this connection has no way to tell that one from the answer
    // to the next question — so the endpoint is treated as poisoned for commits and
    // this window stops posting rather than reading answers one behind for ever.
    m_commitPending = false;

    struct pollfd wait_reply;
    wait_reply.fd = m_replyFd;
    wait_reply.events = POLLIN;
    wait_reply.revents = 0;
    if (poll(&wait_reply, 1, ReplyTimeoutMs) != 1)
        return 0;

    struct staros_message msg;
    if (staros_msg_recv(m_replyFd, &msg) != 0)
        return 0;
    if (msg.tag != STAROS_DISPLAY_OK)
        return 0;
    return msg.words[0];
}

bool QStarosConnection::raise(quint64 surface)
{
    struct staros_message msg;
    memset(&msg, 0, sizeof msg);
    msg.tag = STAROS_DISPLAY_RAISE;
    msg.words[0] = surface;
    return call(&msg);
}

bool QStarosConnection::destroy(quint64 surface)
{
    struct staros_message msg;
    memset(&msg, 0, sizeof msg);
    msg.tag = STAROS_DISPLAY_DESTROY;
    msg.words[0] = surface;
    return call(&msg);
}

bool QStarosConnection::takeFocus()
{
    struct staros_message msg;
    memset(&msg, 0, sizeof msg);
    msg.tag = STAROS_DISPLAY_FOCUS;
    return call(&msg);
}
