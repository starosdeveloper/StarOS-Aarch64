#include "qstarosbackingstore.h"

#include "qstaroswindow.h"

#include <QtGui/qwindow.h>

QStarosBackingStore::QStarosBackingStore(QWindow *window) : QPlatformBackingStore(window)
{
    resize(window->size(), QRegion());
}

QStarosWindow *QStarosBackingStore::platformWindow() const
{
    return static_cast<QStarosWindow *>(window()->handle());
}

void QStarosBackingStore::resize(const QSize &size, const QRegion &)
{
    QStarosWindow *w = platformWindow();
    if (w == nullptr || !w->isValid() || w->bufferSize() != size) {
        // The window owns the buffers and resizes them when Qt sets its geometry.
        // If they do not match yet, the image stays null rather than pointing at
        // the old ones — a QImage over a buffer of the wrong size paints past its
        // end, and the fault lands in whatever else that memory belongs to.
        m_image = QImage();
        return;
    }
    // A view over the window's *back* buffer. Not a copy: QPainter writes straight
    // into memory the display server can read, which is what the client owning its
    // own pixels was for.
    m_image = QImage(w->paintBuffer(), size.width(), size.height(), size.width() * 4,
                     QImage::Format_RGB32);
}

void QStarosBackingStore::beginPaint(const QRegion &)
{
    // The last moment before Qt writes into the back buffer, and therefore the place
    // the previous frame's commit is waited for: the answer means the server has let
    // go of the buffer whose partner is about to be painted, and the restore that
    // depends on it happens here too.
    //
    // Not in `flush()`, where it used to be. Everything between a flush and the next
    // paint — the event loop, animation, the polish pass — is time the server can
    // composite in, and waiting at the bottom of the flush threw all of it away.
    QStarosWindow *w = platformWindow();
    if (w != nullptr)
        w->settle();
}

void QStarosBackingStore::flush(QWindow *, const QRegion &region, const QPoint &offset)
{
    QStarosWindow *w = platformWindow();
    if (w == nullptr || m_image.isNull())
        return;
    // The offset is Qt telling this store the window moved under it. Nothing here
    // supports a backing store shared between windows, so an offset can only mean
    // the region is expressed somewhere else — and translating is cheaper than
    // being subtly wrong about which pixels changed.
    QRegion damage = offset.isNull() ? region : region.translated(offset);
    w->present(damage);
    // The window swapped its buffers; the image must follow, or the next frame is
    // painted into the one now on screen — which is precisely the tearing double
    // buffering exists to remove.
    resize(m_image.size(), QRegion());
}
