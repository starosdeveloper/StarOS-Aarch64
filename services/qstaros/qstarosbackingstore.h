// The backing store: a QImage over the window's shared pixels.
//
// No copy. `QImage` is constructed over the buffer the display server can read, so
// `QPainter` writes straight into shared memory and `flush()` is one message rather
// than a blit. The whole point of the client owning its pixels is that this layer
// has nothing to move.

#pragma once

#include <qpa/qplatformbackingstore.h>

#include <QtGui/qimage.h>

class QStarosWindow;

class QStarosBackingStore : public QPlatformBackingStore
{
public:
    explicit QStarosBackingStore(QWindow *window);

    QPaintDevice *paintDevice() override { return &m_image; }
    void resize(const QSize &size, const QRegion &staticContents) override;
    void flush(QWindow *window, const QRegion &region, const QPoint &offset) override;
    QImage toImage() const override { return m_image; }

private:
    QStarosWindow *platformWindow() const;

    // Not a buffer of its own — a view. The bytes belong to the window, and a
    // QImage that owned them would mean `QPainter` painting into memory the
    // display server cannot see, followed by a copy nobody needs.
    QImage m_image;
};
