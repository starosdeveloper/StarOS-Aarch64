// A window: one surface at the display server, and the pixels behind it.
//
// The buffer belongs to the *window* rather than to the backing store, which is the
// opposite of where Qt puts it and is deliberate. A surface is created once, with a
// buffer, and the server keeps that pairing; a backing store is created and
// destroyed as Qt pleases, and one that owned the pixels would have to tear the
// surface down and build it again on every resize — a flicker with a protocol round
// trip in it.

#pragma once

#include <QtCore/qelapsedtimer.h>
#include <qpa/qplatformwindow.h>

#include "qstarosscreen.h"

class QStarosWindow : public QPlatformWindow
{
public:
    QStarosWindow(QWindow *window, QStarosScreen *screen);
    ~QStarosWindow() override;

    void setGeometry(const QRect &rect) override;
    void setVisible(bool visible) override;
    void raise() override;
    void requestActivateWindow() override;
    WId winId() const override { return WId(m_surface); }

    // The buffer the backing store paints into, and the one it does not.
    //
    // Two, swapped on every commit. A client drawing into the buffer the server is
    // compositing produces a torn window and nothing on the server's side fixes it:
    // the pages are shared and there is no fence. This is the whole reason `Commit`
    // may carry a capability.
    unsigned char *paintBuffer() const { return m_pixels[m_back]; }
    QSize bufferSize() const { return m_bufferSize; }

    // Hand the painted buffer to the server and make the other one current.
    //
    // The commit is *posted*: it does not wait for the server's answer. What is
    // owed afterwards is finished by `settle()`.
    void present(const QRegion &region);

    // Finish what the last `present()` left owed: wait for the commit's answer and
    // restore the damage rectangle into the new back buffer.
    //
    // Called from the backing store at the top of a paint, which is the last moment
    // it can happen and the best one: everything between the two — the event loop,
    // animation, Qt's polish pass — runs while the server composites, and on every
    // frame measured so far the answer is already there by the time it is asked for.
    //
    // Waiting is not optional and not a nicety. The answer means the server has let
    // go of the buffer this call writes into; without it, the copy below races the
    // compositor over shared pages with no fence between them, and the failure is a
    // window that tears under load and nowhere else.
    void settle();

    bool isValid() const { return m_surface != 0; }

private:
    bool allocate(const QSize &size);
    void release();
    void reportProfile() const;

    // What one frame costs this side of the wire, in the three parts that are three
    // different things to fix.
    //
    // `post` is the send: the message into the endpoint and back, with no server in
    // it. `await` is what is left of the round trip after the frame stopped waiting
    // inside itself — the compositing the client did not manage to overlap, and the
    // number that says whether posting early bought anything. `restore` is this
    // plugin's own memcpy, the price of double buffering with damage tracking.
    //
    // `post + await` is comparable with the round trip the synchronous version
    // measured, which is what makes the two profiles readable against each other.
    struct Profile
    {
        qint64 frames = 0;
        qint64 pixels = 0;
        qint64 postNs = 0;
        qint64 awaitNs = 0;
        qint64 restoreNs = 0;
        qint64 worstPostNs = 0;
        qint64 worstAwaitNs = 0;
        qint64 worstRestoreNs = 0;
    };

    QStarosScreen *m_screen;
    quint64 m_surface = 0;
    unsigned int m_caps[2] = { 0, 0 };
    unsigned char *m_pixels[2] = { nullptr, nullptr };
    int m_back = 0;
    // The damage rectangle a posted commit still owes a restore for; empty when
    // nothing is owed. It doubles as the record of whether a commit is outstanding,
    // because the two are set and cleared together and a second flag would be a
    // second thing to get wrong.
    QRect m_pendingRestore;
    QSize m_bufferSize;
    bool m_visible = false;
    Profile m_profile;
    // One timer for both stages: started once at the top of `present()` and read
    // twice, rather than started and stopped around each. Every read is a
    // `clock_gettime`, and on this system that is a syscall — three per frame is the
    // fewest that can separate two stages, and a fourth would be measuring the
    // measurement.
    QElapsedTimer m_clock;
};
