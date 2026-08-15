#include "qstarosintegration.h"

#include "qstarosbackingstore.h"
#include "qstarosscreen.h"
#include "qstaroswindow.h"

#include <QtGui/private/qunixeventdispatcher_qpa_p.h>
#include <QtGui/private/qfreetypefontdatabase_p.h>
#include <qpa/qwindowsysteminterface.h>

#include <QtCore/qdir.h>
#include <QtCore/qfile.h>

// Where the fonts are, as the file server names them.
//
// Not a search path and not a guess: this system has one filesystem, it is the
// initramfs, and `fonts/` is where the build puts the typefaces. Qt's own default —
// `QLibraryInfo::path(LibrariesPath) + "/fonts"` — points into the host directory
// the toolchain was configured with, which exists on the build machine and on no
// booted image, so the stock database found nothing and said so on every run.
static const char STAROS_FONT_DIR[] = "fonts";

namespace {

// The font database, pointed at the one directory that has fonts in it.
//
// `QFreeTypeFontDatabase::populateFontDatabase` is not called at all here, because
// everything it does is search paths that do not exist on this system. What is left
// is the part that matters: read each file whole and hand it to FreeType. Reading
// whole rather than memory-mapping is not a shortcut — there is no `mmap` of a file
// here, the file server hands over bytes over IPC, and `services/hello-c` measures
// exactly this path for a 133 KB typeface.
class QStarosFontDatabase : public QFreeTypeFontDatabase
{
public:
    void populateFontDatabase() override
    {
        // Four faces, not every file in the directory.
        //
        // The initramfs carries the whole IBM Plex Mono family — sixteen weights and
        // their italics, 1.6 MB of it — and reading all of it is what a database
        // that globs `*.ttf` does. Here that is not merely wasteful: every file
        // arrives over IPC through a 4 KiB bounce buffer, so the family costs some
        // four hundred round trips to the file server before the first window is
        // painted, and `fssrv` stops answering at `MAX_REQUESTS`. The first Qt
        // program to draw text died that way — blocked on a reply from a file server
        // that had already printed its tally and exited.
        //
        // These four are what a toolkit resolves for regular, bold, italic and their
        // combination. A program that asks for a weight not listed here gets the
        // nearest of them, which is what a font database is for.
        // Named files, opened directly — not a directory listing filtered down.
        //
        // The two are not equivalent here. A listing has to succeed before a filter
        // can be applied to it, and an image that carries only some of these faces is
        // an ordinary case: the smoke matrix builds an initramfs with just the
        // regular one. Opening each name and skipping what is absent works on both
        // images and needs nothing from `readdir`.
        const QStringList wanted = { QStringLiteral("IBMPlexMono-Regular.ttf"),
                                     QStringLiteral("IBMPlexMono-Bold.ttf"),
                                     QStringLiteral("IBMPlexMono-Italic.ttf"),
                                     QStringLiteral("IBMPlexMono-BoldItalic.ttf") };
        const QDir dir{ QLatin1String(STAROS_FONT_DIR) };
        for (const QString &name : wanted) {
            QFile file(dir.filePath(name));
            if (!file.open(QIODevice::ReadOnly))
                continue;
            const QByteArray data = file.readAll();
            if (data.isEmpty())
                continue;
            addTTFile(data, file.fileName().toUtf8());
        }
    }
};

} // namespace

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
    // `poll` over a set of descriptors with a timeout is exactly what this system
    // provides — including for its own IPC, which `staros_endpoint_fd` turns into
    // something `poll` can wait on. Input therefore arrives on an ordinary
    // descriptor and is read by a socket notifier like any other source, which is
    // the whole reason the endpoint was made to look like one.
    //
    // What is *not* ordinary is that a QPA plugin's events do not come from a
    // descriptor at all. `QWindowSystemInterface::handleExposeEvent` and its
    // relatives put events on a queue of Qt's own, and something has to drain it by
    // calling `sendWindowSystemEvents`. `QEventDispatcherUNIX` does not — it knows
    // about descriptors and timers and nothing else — and `QUnixEventDispatcherQPA`
    // is the subclass that exists precisely to add that one call.
    //
    // This returned the base class first, and the symptom was a program that ran:
    // the plugin loaded, the window was created, the surface reached the compositor
    // and the event loop started — a zero-millisecond timer fired to prove it — and
    // then nothing was ever painted, because the expose event queued by `setVisible`
    // sat on a queue nobody read.
    return new QUnixEventDispatcherQPA;
}

QPlatformFontDatabase *QStarosIntegration::fontDatabase() const
{
    if (m_fontDatabase == nullptr) {
        // FreeType over the fonts in the initramfs. `QFreeTypeFontDatabase` opens
        // files by path and reads them whole, which is what the file server serves
        // and what `services/hello-c` proved for a 133 KB font: byte for byte
        // through a 4 KiB bounce buffer, twice, by two different paths.
        const_cast<QStarosIntegration *>(this)->m_fontDatabase = new QStarosFontDatabase;
    }
    return m_fontDatabase;
}
