// qt-hello — the first Qt program on this system.
//
// It is deliberately the smallest thing that exercises the whole path rather than a
// piece of it: `QGuiApplication` constructs, which loads the platform plugin, which
// connects to `displaysrv`; a `QRasterWindow` is shown, which creates a surface and
// a shared buffer; `paintEvent` draws with `QPainter` into that buffer; and the
// event loop runs, which means the event dispatcher, the timers and the input
// endpoint are all live. If any one of those is wrong the program says so on the
// console instead of hanging.
//
// No QML yet. QML is the next layer up and it would fail for reasons belonging to
// this one, which is the same argument `services/hello-cpp` makes about C++ and Qt.
//
// The plugin is *linked in*, not found: `dlopen` refuses on this system and qtbase
// is configured `-no-feature-dlopen`, so `Q_IMPORT_PLUGIN` below is what puts
// `QStarosIntegrationPlugin` into Qt's static plugin registry. Without that line the
// program builds, runs, and dies with "no platform plugin could be loaded" — the
// failure mode this comment exists to make recognisable.

#include <QtGui/QGuiApplication>
#include <QtGui/QFontInfo>
#include <QtGui/QFontMetrics>
#include <QtGui/QPainter>
#include <QtGui/QRasterWindow>
#include <QtCore/QTimer>
#include <QtCore/QtPlugin>

#include <cstdio>

Q_IMPORT_PLUGIN(QStarosIntegrationPlugin)

// A window that paints something with enough structure to tell "the buffer reached
// the screen" from "the buffer reached the screen and the coordinates are right".
//
// A single flat fill would look identical whether the damage rectangle, the stride
// and the origin were correct or not. The bars and the diagonal do not: a wrong
// stride shears the diagonal, a wrong origin moves the corner marker off the edge,
// and a damage rectangle that is too small leaves part of the fill stale.
class HelloWindow : public QRasterWindow
{
public:
    HelloWindow()
    {
        setTitle(QStringLiteral("qt-hello"));
        resize(320, 240);
    }

protected:
    void paintEvent(QPaintEvent *event) override
    {
        Q_UNUSED(event);
        QPainter painter(this);
        const QRect area(QPoint(0, 0), size());

        painter.fillRect(area, QColor(24, 24, 32));

        // Four vertical bars in known colours. Reading them off the screen left to
        // right is how a channel swap — BGRA read as RGBA — is caught by eye.
        const QColor bars[] = { QColor(220, 60, 60), QColor(60, 200, 90),
                                QColor(70, 120, 240), QColor(240, 210, 80) };
        const int barWidth = area.width() / 8;
        for (int i = 0; i < 4; ++i)
            painter.fillRect(QRect(i * barWidth, 0, barWidth, area.height() / 3), bars[i]);

        // A diagonal, which is the stride check: it is a straight line only if each
        // row starts exactly one stride after the last.
        painter.setPen(QPen(QColor(255, 255, 255), 2));
        painter.drawLine(area.topLeft(), area.bottomRight());

        // A marker in the bottom-right corner, which is the extent check: it is only
        // fully visible if the surface is as large as the window believes.
        painter.fillRect(QRect(area.width() - 12, area.height() - 12, 10, 10),
                         QColor(255, 255, 255));

        painter.setPen(QColor(230, 230, 230));
        painter.drawText(QRect(8, area.height() / 3 + 8, area.width() - 16, 40),
                         Qt::AlignLeft, QStringLiteral("Qt on StarOS"));

        // The text is the one thing on this window that a pixel check cannot claim on
        // its own: bars and a diagonal are drawn by the raster engine with no font
        // involved, and a `drawText` against an empty database silently draws
        // nothing. So say what font was actually resolved and how wide it made the
        // string. A family that is not the one in the initramfs, or a width of zero,
        // is a font database that found nothing — which is what this system did
        // until the plugin stopped looking in the host's directories.
        const QFontMetrics metrics(painter.font());
        const QString label = QStringLiteral("Qt on StarOS");
        std::printf("[qt-hello] painted %dx%d, text in '%s' %d px wide, %d px tall\n",
                    area.width(), area.height(),
                    QFontInfo(painter.font()).family().toLocal8Bit().constData(),
                    metrics.horizontalAdvance(label), metrics.height());
        std::fflush(stdout);
    }
};

int main(int argc, char **argv)
{
    std::printf("[qt-hello] starting\n");
    std::fflush(stdout);

    QGuiApplication app(argc, argv);
    std::printf("[qt-hello] QGuiApplication constructed, platform=%s\n",
                QGuiApplication::platformName().toLocal8Bit().constData());
    std::fflush(stdout);

    HelloWindow window;
    window.show();
    std::printf("[qt-hello] window shown\n");
    std::fflush(stdout);

    // Three markers rather than one, because "nothing was painted" has three
    // different causes and they are indistinguishable from the outside: the event
    // loop never started, it started and delivered nothing, or it delivered the
    // expose and the paint produced no output. Each line below rules one out.
    //
    // A zero-millisecond timer is not a delay — it is a request to be called back on
    // the first pass of the loop, so its line appearing proves the dispatcher is
    // running at all.
    // A repeating timer rather than a single shot, because "the loop ran once" and
    // "the loop is running" are different claims and only the second one is
    // interesting. Each tick that prints is a pass through the blocking wait and
    // back, so a counter that stops tells us the wait is where it stopped.
    auto *heartbeat = new QTimer(&app);
    int ticks = 0;
    QObject::connect(heartbeat, &QTimer::timeout, &app, [&ticks] {
        ++ticks;
        std::printf("[qt-hello] event loop tick %d\n", ticks);
        std::fflush(stdout);
    });
    heartbeat->start(100);

    // Ends by itself. This runs under `cargo krun`, which has no keyboard operator
    // and no patience: a program that waited for a close event would hold the whole
    // smoke run open until QEMU's own timeout, and the log would say nothing about
    // why. Quitting on a timer means the exit code is a result.
    //
    // 400 ms, not 3000: the first attempt used three seconds and the whole machine
    // powered off before it elapsed — every other program in the boot had finished,
    // and this one was still counting. The number has to be shorter than the demo,
    // not shorter than a person's patience.
    QTimer::singleShot(400, &app, [] {
        std::printf("[qt-hello] quitting\n");
        std::fflush(stdout);
        QGuiApplication::quit();
    });

    std::printf("[qt-hello] entering the event loop\n");
    std::fflush(stdout);
    const int code = app.exec();
    std::printf("[qt-hello] exec returned %d\n", code);
    std::fflush(stdout);
    return code;
}
