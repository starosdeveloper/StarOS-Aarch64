// A window: one surface at the display server, and the pixels behind it.
//
// The buffer belongs to the *window* rather than to the backing store, which is the
// opposite of where Qt puts it and is deliberate. A surface is created once, with a
// buffer, and the server keeps that pairing; a backing store is created and
// destroyed as Qt pleases, and one that owned the pixels would have to tear the
// surface down and build it again on every resize — a flicker with a protocol round
// trip in it.

#pragma once

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
    void present(const QRegion &region);

    bool isValid() const { return m_surface != 0; }

private:
    bool allocate(const QSize &size);
    void release();

    QStarosScreen *m_screen;
    quint64 m_surface = 0;
    unsigned int m_caps[2] = { 0, 0 };
    unsigned char *m_pixels[2] = { nullptr, nullptr };
    int m_back = 0;
    QSize m_bufferSize;
    bool m_visible = false;
};
