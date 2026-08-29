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
    // Before the buffers go, not after: `settle()` copies between them, and it is
    // owed whenever a resize or a close lands between a present and the next paint.
    // Running it against released pointers would be a write through null on the
    // ordinary path of closing a window.
    settle();

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

    // Anything still owed from the previous frame is finished first. In an ordinary
    // frame the backing store already did this before Qt painted, and this costs a
    // branch; it is here for the frames that are not ordinary — two flushes with no
    // paint between them — where the alternative is overwriting a buffer the server
    // is reading.
    settle();

    m_clock.start();

    // Hand over the buffer just painted and make the other one current, *without*
    // waiting for the server to say it composited. The wait is the expensive half —
    // 2 702 us of a 3 762 us round trip is the client parked while the scheduler
    // runs the server and comes back — and nothing in this frame needs the answer.
    // What needs it is the next paint, because that is when this buffer's partner
    // gets written to again; `settle()` is where it is waited for.
    if (!m_screen->connection()->postCommit(m_surface, damage, m_caps[m_back])) {
        // Nothing went out, so the server still holds the buffer it had and the
        // back buffer is unchanged. Swapping here would hand Qt the pixels that are
        // on screen.
        return;
    }
    m_back = 1 - m_back;

    // The restore is owed, not done. It writes into the buffer the server has just
    // been handed the *other* half of — safe only once the commit is answered, and
    // that answer is what `settle()` waits for.
    m_pendingRestore = damage;

    const qint64 posted = m_clock.nsecsElapsed();
    m_profile.frames++;
    m_profile.pixels += qint64(damage.width()) * qint64(damage.height());
    m_profile.postNs += posted;
    m_profile.worstPostNs = qMax(m_profile.worstPostNs, posted);
}

void QStarosWindow::settle()
{
    if (m_pendingRestore.isEmpty())
        return;
    const QRect damage = m_pendingRestore;
    m_pendingRestore = QRect();

    m_clock.start();
    // The reply is a release: the server is done with the buffer it was reading
    // before the last commit, which is the one about to be written to below. On a
    // frame that took longer to rasterise than the server took to composite — every
    // frame measured so far — this returns without parking at all, and that is the
    // whole point of having posted the commit early.
    m_screen->connection()->collectCommit();
    const qint64 awaited = m_clock.nsecsElapsed();

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

    const qint64 restored = m_clock.nsecsElapsed() - awaited;
    m_profile.awaitNs += awaited;
    m_profile.restoreNs += restored;
    m_profile.worstAwaitNs = qMax(m_profile.worstAwaitNs, awaited);
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
    //
    // `commit` is still reported, as `post + await`, because it is the figure the
    // synchronous version printed and the one the display server's half subtracts
    // from. Dropping it would make the two profiles unreadable against each other
    // across the change that was made to improve exactly that number.
    const qint64 commitNs = m_profile.postNs + m_profile.awaitNs;
    // Every key word appears once before any "worst", so a reader — human or the
    // awk in `scripts/frame-profile.sh` — can take the first occurrence of each and
    // be right. The previous wording put the means in parentheses, which made the
    // first "post" in the line the token "(post" and the first bare one the worst
    // frame's: a rule that reads correctly and parses to the wrong number.
    std::printf("[qstaros] present: %lld frame(s), %lld px - commit %lld us/frame, "
                "post %lld us/frame, await %lld us/frame, restore %lld us/frame - "
                "worst post %lld us, worst await %lld us, worst restore %lld us, "
                "%lld ns/px committed\n",
                static_cast<long long>(m_profile.frames),
                static_cast<long long>(m_profile.pixels),
                static_cast<long long>(commitNs / m_profile.frames / 1000),
                static_cast<long long>(m_profile.postNs / m_profile.frames / 1000),
                static_cast<long long>(m_profile.awaitNs / m_profile.frames / 1000),
                static_cast<long long>(m_profile.restoreNs / m_profile.frames / 1000),
                static_cast<long long>(m_profile.worstPostNs / 1000),
                static_cast<long long>(m_profile.worstAwaitNs / 1000),
                static_cast<long long>(m_profile.worstRestoreNs / 1000),
                static_cast<long long>(m_profile.pixels > 0 ? commitNs / m_profile.pixels : 0));
    std::fflush(stdout);
}
