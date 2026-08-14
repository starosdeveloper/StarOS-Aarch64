#include "qstarosintegration.h"

#include "qstarosbackingstore.h"
#include "qstarosscreen.h"
#include "qstaroswindow.h"

#include <QtCore/private/qeventdispatcher_unix_p.h>
#include <QtGui/private/qfreetypefontdatabase_p.h>
#include <qpa/qwindowsysteminterface.h>

QStarosIntegration::QStarosIntegration()
{
    if (!m_connection.isValid())
        return;
    m_screen = new QStarosScreen(&m_connection);
    QWindowSystemInterface::handleScreenAdded(m_screen);
}

QStarosIntegration::~QStarosIntegration()
{
    delete m_fontDatabase;
    if (m_screen != nullptr)
        QWindowSystemInterface::handleScreenRemoved(m_screen);
}

bool QStarosIntegration::hasCapability(QPlatformIntegration::Capability cap) const
{
    switch (cap) {
    // Every window is a shared buffer the compositor reads, so several of them cost
    // nothing this plugin has to arrange.
    case MultipleWindows:
        return true;
    // No GPU driver and no OpenGL: `-no-opengl` is a build flag and this is the
    // run-time half of the same statement. Answering true would have Qt Quick pick
    // its hardware path and fail somewhere with no connection to this decision.
    //
    // `RasterGLSurface` is *not* in this list even though it is false, because Qt
    // has deprecated the enumerator and naming it is a warning. The base class
    // already answers false for it.
    case OpenGL:
    case ThreadedOpenGL:
        return false;
    // Painting from another thread is fine — the buffer is memory like any other —
    // but it is not *proved*, and QPA capabilities are promises Qt acts on rather
    // than hints. It goes true the day something tests it.
    case ThreadedPixmaps:
        return false;
    default:
        return QPlatformIntegration::hasCapability(cap);
    }
}

QPlatformWindow *QStarosIntegration::createPlatformWindow(QWindow *window) const
{
    return new QStarosWindow(window, m_screen);
}

QPlatformBackingStore *QStarosIntegration::createPlatformBackingStore(QWindow *window) const
{
    return new QStarosBackingStore(window);
}

QAbstractEventDispatcher *QStarosIntegration::createEventDispatcher() const
{
    // `QEventDispatcherUNIX` is a `poll` over a set of descriptors with a timeout,
    // and that is exactly what this system provides — including for its own IPC,
    // which `staros_endpoint_fd` turns into something `poll` can wait on. Nothing
    // is subclassed here: input arrives on an ordinary descriptor and is read by a
    // socket notifier like any other source, which is the whole reason the endpoint
    // was made to look like one.
    return new QEventDispatcherUNIX;
}

QPlatformFontDatabase *QStarosIntegration::fontDatabase() const
{
    if (m_fontDatabase == nullptr) {
        // FreeType over the fonts in the initramfs. `QFreeTypeFontDatabase` opens
        // files by path and reads them whole, which is what the file server serves
        // and what `services/hello-c` proved for a 133 KB font: byte for byte
        // through a 4 KiB bounce buffer, twice, by two different paths.
        const_cast<QStarosIntegration *>(this)->m_fontDatabase = new QFreeTypeFontDatabase;
    }
    return m_fontDatabase;
}
