// The connection to `services/displaysrv`, and the only place in this plugin that
// speaks the protocol.
//
// Everything else here — screen, window, backing store — asks this object. One
// place that knows the wire format is the same decision the display server made on
// its side, and for the same reason: the last number in this system that lived in
// two places disagreed with itself and a server spent its life rejecting messages
// it had no business receiving.
//
// The calls are synchronous request/reply. Qt's event loop runs on top of `poll`
// (see `QStarosIntegration::createEventDispatcher`), and input arrives on a
// *different* descriptor — so a `createWindow` waiting for its answer here cannot
// swallow a key press, which is exactly why the display server gives each client
// two endpoints rather than one.

#pragma once

#include <QtCore/qglobal.h>
#include <QtCore/qpoint.h>
#include <QtCore/qrect.h>
#include <QtCore/qsize.h>

#include <staros.h>

class QStarosConnection
{
public:
    QStarosConnection();
    ~QStarosConnection();

    // Whether there is a display server at all. A machine with no framebuffer has
    // none — the kernel does not know what QEMU was started with, so the capability
    // is installed either way — and a plugin that blocked waiting for an answer
    // would hang on every headless board. `QStarosIntegration` refuses to start
    // when this is false, which is the failure Qt knows how to report.
    bool isValid() const { return m_valid; }

    QSize screenSize() const { return m_screenSize; }
    int screenDepth() const { return m_screenDepth; }

    // Create a surface from a buffer the caller allocated with
    // `staros_shared_create`. Returns the server's surface id, or 0.
    //
    // The client owning the pixels is the server's decision and this plugin's
    // convenience: the pages are Qt's, the capability is Qt's to delegate, and
    // dropping it takes the window away with no bookkeeping on either side.
    quint64 createSurface(unsigned int bufferCap, const QSize &size, const QPoint &at);

    // Commit a damage rectangle, optionally swapping in a different buffer.
    //
    // `bufferCap` of 0 means "the one you already have". Passing a new one is how
    // double buffering works here: draw into the buffer that is not on screen and
    // hand it over with the commit, which is what QBackingStore does everywhere
    // else and what stops a window tearing while Qt paints it.
    //
    // Returns the pixels the server actually wrote. Qt has no use for the number;
    // this plugin does, because it is the one place a `flush()` that quietly sends
    // the whole window instead of the damage rectangle shows up as a number rather
    // than as a frame rate.
    quint64 commit(quint64 surface, const QRect &damage, unsigned int bufferCap = 0);

    // The same commit, sent without waiting for the answer.
    //
    // A frame's round trip was measured at 3 762 us against 1 060 us of compositing
    // inside it (G8.1): two thirds of it is the client parked in `poll` while the
    // scheduler runs the server and comes back. None of that waiting has to happen
    // inside a frame — the answer is only needed before the *next* frame is painted,
    // because that is when the buffer the server is reading gets written to again.
    //
    // What the answer means is therefore a release: "the server is no longer looking
    // at the buffer you gave me before this one". `collectCommit` is where that is
    // waited for, and `QStarosWindow` calls it at the top of a paint rather than at
    // the bottom of a present.
    //
    // At most one commit is ever outstanding. The reply endpoint is a queue, so two
    // unanswered requests would mean the next `call()` reading somebody else's
    // answer — every request in this protocol replies, and a plugin that lost track
    // of which reply belonged to which question would act on a surface id that came
    // back from a `Screen`.
    bool postCommit(quint64 surface, const QRect &damage, unsigned int bufferCap = 0);

    // Wait for an outstanding `postCommit`, and return the pixels the server wrote.
    //
    // Zero when there was nothing outstanding, which is not an error and is how the
    // first frame of a window behaves. A refused commit also gives zero, one frame
    // later than a synchronous one would — that is the price of the asynchrony, and
    // it is paid in a diagnostic's timing rather than in a pixel.
    quint64 collectCommit();

    bool raise(quint64 surface);
    bool destroy(quint64 surface);

    // Claim the keyboard. Focus is asked for, not inherited from being on top:
    // raising a window and taking the keystroke someone is mid-way through typing
    // are different acts, and the display server exposes them separately.
    bool takeFocus();

    // The descriptor input arrives on, for the event dispatcher to watch. -1 when
    // there is none.
    int eventDescriptor() const { return m_eventFd; }

    // The descriptor requests go out on. Exposed so the dispatcher can keep it out
    // of its wait set: a reply is not an event, and a dispatcher that woke for one
    // would deliver it to nobody.
    int requestDescriptor() const { return m_requestFd; }

private:
    bool call(struct staros_message *msg);

    int m_requestFd = -1;
    int m_replyFd = -1;
    int m_eventFd = -1;
    // Whether a commit is out there with its answer still unread. One bit and not a
    // count, because the invariant is that there is never more than one; see
    // `postCommit`.
    bool m_commitPending = false;
    bool m_valid = false;
    QSize m_screenSize;
    int m_screenDepth = 32;
};
