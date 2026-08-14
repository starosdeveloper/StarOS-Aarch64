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
    bool m_valid = false;
    QSize m_screenSize;
    int m_screenDepth = 32;
};
