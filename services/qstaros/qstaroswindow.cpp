#include "qstaroswindow.h"

#include <QtGui/qwindow.h>
#include <qpa/qwindowsysteminterface.h>

#include <cstdio>

QStarosWindow::QStarosWindow(QWindow *window, QStarosScreen *screen)
    : QPlatformWindow(window), m_screen(screen)
{
    // A window with no size is a window Qt is about to give a size to. Allocating
    // for it would mean allocating twice and creating a surface the server would
    // immediately have to be told about again.
    QRect rect = window->geometry();
    if (rect.width() > 0 && rect.height() > 0)
        allocate(rect.size());
}

QStarosWindow::~QStarosWindow()
{
    reportProfile();
    release();
}

bool QStarosWindow::allocate(const QSize &size)
{
    release();

    const size_t bytes = size_t(size.width()) * size_t(size.height()) * 4;
    for (int i = 0; i < 2; i++) {
        m_caps[i] = staros_shared_create(bytes);
        if (m_caps[i] == 0)
            return false;
        m_pixels[i] = static_cast<unsigned char *>(staros_shared_map(m_caps[i]));
        if (m_pixels[i] == nullptr)
            return false;
    }
    m_bufferSize = size;
    m_back = 0;

    const QPoint at = QPlatformWindow::geometry().topLeft();
    m_surface = m_screen->connection()->createSurface(m_caps[0], size, at);
    return m_surface != 0;
}

void QStarosWindow::release()
{
    if (m_surface != 0) {
        m_screen->connection()->destroy(m_surface);
        m_surface = 0;
    }
    // The buffers themselves stay mapped. This system has no unmap — the pages a
    // process takes are its own until it exits — so pretending to release them
    // would be a lie with a leak behind it. `staros_mmap_retained()` is where that
    // shows up as a number, and a window that resizes in a loop is where it
    // eventually matters.
    m_pixels[0] = m_pixels[1] = nullptr;
    m_caps[0] = m_caps[1] = 0;
    m_bufferSize = QSize();
}

void QStarosWindow::setGeometry(const QRect &rect)
{
    const QRect old = geometry();
    QPlatformWindow::setGeometry(rect);
    if (rect.size() != old.size() || m_surface == 0) {
        // A resize is a new surface: the server pairs a surface with the buffer it
        // was created from, and there is no request that changes one without the
        // other. Rebuilding is honest and visibly costly, which is the right way
        // round for something a toolkit should not do every frame.
        allocate(rect.size());
    }
    QWindowSystemInterface::handleGeometryChange(window(), rect);
}

void QStarosWindow::setVisible(bool visible)
{
    if (visible == m_visible)
        return;
    m_visible = visible;
    if (visible && m_surface != 0) {
        // Raise on show, so a newly shown window is in front — which is what every
        // toolkit means by showing one. Focus is *not* taken here: raising and
        // stealing the keystroke someone is typing are different acts, and this
        // server keeps them apart. `requestActivateWindow` is where focus belongs.
        m_screen->connection()->raise(m_surface);
    }
    QWindowSystemInterface::handleExposeEvent(window(), visible ? QRect(QPoint(), geometry().size())
                                                               : QRect());
}

void QStarosWindow::raise()
{
    if (m_surface != 0)
        m_screen->connection()->raise(m_surface);
}

void QStarosWindow::requestActivateWindow()
{
    if (m_surface == 0)
        return;
    m_screen->connection()->takeFocus();
    QWindowSystemInterface::handleFocusWindowChanged(window());
}

void QStarosWindow::present(const QRegion &region)
{
    if (m_surface == 0 || m_bufferSize.isEmpty())
        return;

    // The damage rectangle, honestly. `region.boundingRect()` and not the window's
    // size: a full-frame copy at 1080p is 8 MB, which is the difference between
    // sixty frames a second and a slide show, and the server reports how many pixels
    // it wrote so a `flush` that quietly sends everything shows up as a number.
    QRect damage = region.boundingRect().intersected(QRect(QPoint(), m_bufferSize));
    if (damage.isEmpty())
        return;

    m_clock.start();

    // Hand over the buffer just painted and make the other one current. The server
    // swaps it in before compositing, so what is on screen is never the buffer Qt
    // is about to draw into next.
    m_screen->connection()->commit(m_surface, damage, m_caps[m_back]);
    m_back = 1 - m_back;

    const qint64 committed = m_clock.nsecsElapsed();

    // The new back buffer holds the frame before last. Qt's backing store repaints
    // only the damaged region, so everything outside it has to already be there —
    // copying the whole buffer across would undo the damage tracking this exists
    // for, and not copying at all shows the frame before last outside the damage.
    // One copy of the damage rectangle is the smallest thing that is correct.
    const int stride = m_bufferSize.width() * 4;
    unsigned char *from = m_pixels[1 - m_back];
    unsigned char *to = m_pixels[m_back];
    for (int y = damage.top(); y <= damage.bottom(); y++) {
        memcpy(to + y * stride + damage.left() * 4, from + y * stride + damage.left() * 4,
               size_t(damage.width()) * 4);
    }

    const qint64 restored = m_clock.nsecsElapsed() - committed;
    m_profile.frames++;
    m_profile.pixels += qint64(damage.width()) * qint64(damage.height());
    m_profile.commitNs += committed;
    m_profile.restoreNs += restored;
    m_profile.worstCommitNs = qMax(m_profile.worstCommitNs, committed);
    m_profile.worstRestoreNs = qMax(m_profile.worstRestoreNs, restored);
}

void QStarosWindow::reportProfile() const
{
    if (m_profile.frames == 0)
        return;
    // Printed when the window goes away rather than every frame. A line per frame
    // would go out over the same UART the compositor reports on, at a cost per
    // character that is a substantial fraction of a frame here — the profile would
    // then be dominated by the printing of the profile.
    //
    // The pixel count is the damage rectangle Qt asked for, which is what makes the
    // per-pixel figure comparable with the display server's: both sides are counting
    // the same rectangle, from opposite ends of the wire.
    std::printf("[qstaros] present: %lld frame(s), %lld px - commit %lld us/frame "
                "(worst %lld us), restore %lld us/frame (worst %lld us), %lld ns/px committed\n",
                static_cast<long long>(m_profile.frames),
                static_cast<long long>(m_profile.pixels),
                static_cast<long long>(m_profile.commitNs / m_profile.frames / 1000),
                static_cast<long long>(m_profile.worstCommitNs / 1000),
                static_cast<long long>(m_profile.restoreNs / m_profile.frames / 1000),
                static_cast<long long>(m_profile.worstRestoreNs / 1000),
                static_cast<long long>(m_profile.pixels > 0 ? m_profile.commitNs / m_profile.pixels
                                                            : 0));
    std::fflush(stdout);
}
