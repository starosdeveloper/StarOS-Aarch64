// shell — the first QML program on this system.
//
// `services/qt-hello` proved the layer below this one: a platform plugin, a window,
// a `QPainter` and an event loop. Everything here is what sits on top of that — a
// JavaScript engine, a declarative type system, a scene graph and a renderer for it —
// and none of it would have been debuggable if the layer below were still uncertain.
//
// Three things about this build are properties of *this* system rather than choices,
// and each is worth naming because each is invisible until it fails:
//
//   * **No JIT.** `mprotect` refuses `PROT_EXEC` here, deliberately — the kernel has
//     no way to add execute permission to a page after the fact — so V4 cannot write
//     machine code and run it. qtdeclarative is configured `-no-feature-qml-jit` and
//     the bytecode interpreter runs everything. That is slower and it is the only
//     thing that can work.
//
//   * **No plugin loading.** `dlopen` refuses, qtbase is built `-no-feature-dlopen`,
//     and a QML module in a static build is an ordinary static library whose
//     registration function has to be *referenced* by something. That is what the
//     `Q_IMPORT_QML_PLUGIN` lines below do. Forget one and the program builds and
//     links and then dies at run time with `module "QtQuick" is not installed` —
//     which reads like a missing file and is a missing symbol reference.
//
//   * **No OpenGL.** The scene graph picks its adaptation at run time, and with
//     qtbase built `-no-opengl` the choice is made statically in
//     `qsgcontextplugin.cpp`: with no OpenGL, Vulkan or Metal configured it defaults
//     to the software adaptation with no environment variable involved. `QSG_RENDER_LOOP`
//     is likewise not set here — the software adaptation's own default is the
//     single-threaded `QSGSoftwareRenderLoop`, and only the literal string
//     `"threaded"` moves it off that. Setting either variable would suggest the
//     defaults are wrong, and they are not.

#include <QtGui/QGuiApplication>
#include <QtQml/QQmlEngine>
#include <QtQml/QQmlError>
// For `Q_IMPORT_QML_PLUGIN`, which lives with the plugin base class rather than with
// the engine. Without it the macro is an unknown identifier and the compiler reports
// a missing semicolon, which is a long way from "you forgot an include".
#include <QtQml/QQmlExtensionPlugin>
// `QQuickView::rootObject()` returns a `QQuickItem *`, and `qquickwindow.h` only
// forward-declares that class — so without this the conversion to `QObject *` fails
// on a type the compiler has never seen the definition of.
#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickView>
#include <QtCore/QElapsedTimer>
#include <QtCore/QFile>
#include <QtCore/QMutex>
#include <QtCore/QThread>
#include <QtCore/QTimer>
#include <QtCore/QUrl>
#include <QtCore/QtPlugin>

#include <cstdio>

Q_IMPORT_PLUGIN(QStarosIntegrationPlugin)

// The QML modules, each one a static archive whose type registrations are pulled in
// by naming its plugin class. `QtQuick` is what `Main.qml` imports; the other three
// are what `QtQuick` itself imports, and they are needed for the same reason and
// fail in the same way.
Q_IMPORT_QML_PLUGIN(QtQmlPlugin)
Q_IMPORT_QML_PLUGIN(QtQmlModelsPlugin)
Q_IMPORT_QML_PLUGIN(QtQmlWorkerScriptPlugin)
Q_IMPORT_QML_PLUGIN(QtQuick2Plugin)

// Where the scene is, as the file server names it — the same arrangement as the
// fonts, and for the same reason: this system has one filesystem and it is the
// initramfs.
static const char SCENE_PATH[] = "qml/Main.qml";

// How long the program stays up before quitting by itself.
//
// It runs under `cargo krun`, which has no operator and no patience. A second is
// enough for both animations in `Main.qml` to pass through a full cycle — the
// runner's is 1000 ms, the pulse's 700 ms each way — so a frame counter over this
// window is a rate and not a single sample.
//
// Raised from 1200 ms when the scene grew a button, and then again — to fifteen
// seconds — when the reason for the first raise turned out to be much larger than
// it looked.
//
// The check has to *see* the button before it can click it, and seeing means
// pulling a 640x480 framebuffer over QMP and examining every pixel of it in Python.
// That is a second or more per frame, and the button is not on screen until Qt has
// started, loaded a scene and rendered it. At three seconds the window in which a
// click could land was often already closed: the click went to a machine that had
// powered off, and what the log showed was a scene that drew perfectly and an input
// path that appeared dead.
//
// Fifteen seconds is not patience, it is the shape of the measurement — a run that
// ends before the experiment can be performed cannot fail the experiment, it can
// only fail to run it. `cargo krun` waits that much longer for a scene that is
// visibly doing something; the smoke matrix's ramfb boot has a ninety-second
// budget and uses well under half of it.
static const int RUN_MS = 15000;

int main(int argc, char **argv)
{
    std::printf("[shell] starting\n");
    std::fflush(stdout);

    QGuiApplication app(argc, argv);
    std::printf("[shell] QGuiApplication constructed, platform=%s\n",
                QGuiApplication::platformName().toLocal8Bit().constData());
    std::fflush(stdout);

    // Opened here rather than left to `setSource`, because the two failures are
    // different and Qt reports them the same way. A missing file and a file full of
    // syntax errors both arrive as "the component has errors"; this separates them
    // before the engine is involved, and prints the size, which is the file server
    // answering.
    // Braced, not parenthesised: `QFile scene(QLatin1String(SCENE_PATH))` is a
    // function declaration, and every use of `scene` below then fails with a message
    // about `QFile (QLatin1String)` not being a structure.
    QFile scene{ QLatin1String(SCENE_PATH) };
    if (!scene.open(QIODevice::ReadOnly)) {
        std::printf("[shell] the scene at '%s' could not be opened: %s\n",
                    SCENE_PATH, scene.errorString().toLocal8Bit().constData());
        std::fflush(stdout);
        return 1;
    }
    std::printf("[shell] scene '%s' is %lld bytes\n", SCENE_PATH, scene.size());
    std::fflush(stdout);
    scene.close();

    // The layer between the file and the engine, exercised on its own.
    //
    // `QQmlComponent` starts a `QThread` before it parses anything — QML compiles on
    // a thread of its own, with a stack size chosen to match its parser's recursion
    // limits — and it locks a `QMutex` that the main thread is already holding. Both
    // of those are first-time paths here: nothing before this program had ever
    // started a `QThread`, and nothing had ever *contended* a `QMutex`, which is the
    // case that reaches `QMutexPrivate` and the free list behind it. This build takes
    // that path and not the futex one, because `QT_LINUXBASE` is defined and
    // `qfutex_p.h` excludes it — see `services/qstaros/patches/0002`.
    //
    // Doing it here rather than leaving it to QML means a failure names the layer.
    {
        QMutex mutex;
        mutex.lock();

        struct Contender : QThread
        {
            QMutex *mutex;
            bool got = false;
            void run() override
            {
                // Blocks: the main thread holds it. This is the contended path.
                mutex->lock();
                got = true;
                mutex->unlock();
            }
        };

        Contender contender;
        contender.mutex = &mutex;
        // The same size `QQmlThreadPrivate` asks for, so the ceiling in the kernel's
        // `SpawnThread` is tested by the thing that will need it rather than by QML
        // ten frames deeper.
        contender.setStackSize(8 * 1024 * 1024);
        contender.start();
        QThread::msleep(20);
        mutex.unlock();
        const bool finished = contender.wait(2000);
        std::printf("[shell] threads: a QThread with an 8 MiB stack %s, "
                    "and took a QMutex the main thread was holding: %s\n",
                    finished ? "ran and joined" : "DID NOT FINISH",
                    contender.got ? "yes" : "NO");
        std::fflush(stdout);
    }

    QQuickView view;
    view.setResizeMode(QQuickView::SizeRootObjectToView);
    view.setTitle(QStringLiteral("shell"));
    view.resize(320, 300);

    // Counted at the point the scene graph says a frame is done, not at the point
    // an animation changes a number. The two are different claims: a property can
    // be animated by a running event loop while nothing reaches the screen, which is
    // exactly the failure the software render loop would have if the backing store
    // never flushed.
    int frames = 0;
    QElapsedTimer since;

    // Where a frame's time goes, split by the three things a frame is.
    //
    // The software render loop does them in one call, in this order, and emits a
    // signal at each boundary — so the split costs nothing but a clock read and is
    // not a guess about what Qt is doing:
    //
    //   beforeFrameBegin -> afterSynchronizing   the scene graph catching up with
    //                                            the QML property values animation
    //                                            has just changed
    //   afterSynchronizing -> afterRendering     QPainter rasterising that graph
    //                                            into the shared buffer
    //   afterRendering -> frameSwapped           the backing store's flush: the
    //                                            plugin's `present()`, the commit
    //                                            round trip, and the display
    //                                            server compositing inside it
    //
    // Three stages and not one number, because they have nothing in common. The
    // first is the JavaScript interpreter and the binding graph, the second is
    // software rasterisation, the third is IPC and somebody else's memcpy — and
    // "the frame took 8 ms" tells you which to work on only by accident.
    struct Stages
    {
        qint64 sync = 0;
        qint64 raster = 0;
        qint64 present = 0;
        qint64 worst = 0;
        qint64 counted = 0;
    } stages;
    QElapsedTimer frame;
    qint64 synced = 0;
    qint64 rendered = 0;
    QObject::connect(&view, &QQuickWindow::beforeFrameBegin, &app, [&] { frame.start(); });
    QObject::connect(&view, &QQuickWindow::afterSynchronizing, &app,
                     [&] { synced = frame.nsecsElapsed(); });
    QObject::connect(&view, &QQuickWindow::afterRendering, &app,
                     [&] { rendered = frame.nsecsElapsed(); });
    QObject::connect(&view, &QQuickWindow::frameSwapped, &app, [&] {
        ++frames;
        // A frame that never synchronised is a frame this timer knows nothing about
        // — the loop can swap without a full pass — and folding a zero into the
        // means would report a rendering stage that got faster the more often it was
        // skipped.
        if (!frame.isValid() || synced == 0 || rendered == 0)
            return;
        const qint64 swapped = frame.nsecsElapsed();
        stages.sync += synced;
        stages.raster += rendered - synced;
        stages.present += swapped - rendered;
        stages.worst = qMax(stages.worst, swapped);
        stages.counted++;
        synced = rendered = 0;
    });

    view.setSource(QUrl::fromLocalFile(QLatin1String(SCENE_PATH)));

    // Every error, not just the first. A QML file with an unresolved import produces
    // one error per unresolved name, and reading only `errors().first()` has sent
    // more than one person looking for the wrong missing module.
    if (view.status() == QQuickView::Error) {
        const QList<QQmlError> errors = view.errors();
        std::printf("[shell] the scene failed to load, %lld error(s):\n",
                    static_cast<long long>(errors.size()));
        for (const QQmlError &error : errors)
            std::printf("[shell]   %s\n", error.toString().toLocal8Bit().constData());
        std::fflush(stdout);
        return 1;
    }

    QObject *root = view.rootObject();
    if (root == nullptr) {
        std::printf("[shell] the scene loaded but has no root object\n");
        std::fflush(stdout);
        return 1;
    }
    std::printf("[shell] scene loaded, root is a %s of %.0fx%.0f\n",
                root->metaObject()->className(),
                root->property("width").toDouble(),
                root->property("height").toDouble());
    std::fflush(stdout);

    since.start();
    view.show();
    // And ask for the keyboard, which showing a window does not do here.
    //
    // Raising a window and taking the keystroke somebody is mid-way through typing
    // are separate acts in this display server — see `services/displaysrv` — so the
    // plugin raises on show and claims focus only when Qt asks it to. This is Qt
    // asking. A pointer needs none of it: a click goes to whatever is under it,
    // which is the whole difference between the two and the reason they are routed
    // by different means.
    view.requestActivate();
    std::printf("[shell] view shown and the keyboard claimed\n");
    std::fflush(stdout);

    // The moving property, sampled rather than trusted.
    //
    // "The animation ran" cannot be read off a frame count: a render loop repainting
    // an unchanging scene produces frames at the same rate. So the runner's `x` is
    // read twice, once early and once late, and printed with both values. Equal
    // values with a healthy frame rate is precisely the picture a stopped clock
    // makes, and it is the falsification this phase calls for.
    // By `objectName`, which `Main.qml` sets explicitly: a QML `id` is resolved at
    // compile time and does not exist in the meta-object system, so `findChild`
    // cannot see it.
    QObject *runner = root->findChild<QObject *>(QStringLiteral("runner"));
    if (runner == nullptr) {
        std::printf("[shell] the scene has no object named 'runner'\n");
        std::fflush(stdout);
        return 1;
    }

    double firstX = runner->property("x").toDouble();
    QTimer::singleShot(RUN_MS / 4, &app, [runner, &firstX] {
        firstX = runner->property("x").toDouble();
    });

    QTimer::singleShot(RUN_MS, &app, [&] {
        const qint64 elapsed = since.elapsed();
        const double lastX = runner->property("x").toDouble();
        std::printf("[shell] %d frame(s) in %lld ms\n", frames, elapsed);
        if (stages.counted > 0) {
            const qint64 n = stages.counted;
            std::printf("[shell] frame profile over %lld frame(s): sync %lld us, "
                        "raster %lld us, present %lld us per frame; worst frame %lld us\n",
                        static_cast<long long>(n),
                        static_cast<long long>(stages.sync / n / 1000),
                        static_cast<long long>(stages.raster / n / 1000),
                        static_cast<long long>(stages.present / n / 1000),
                        static_cast<long long>(stages.worst / 1000));
        } else {
            std::printf("[shell] frame profile: no frame carried all three stages; "
                        "nothing measured\n");
        }
        std::printf("[shell] the animated rectangle moved from x=%.1f to x=%.1f\n",
                    firstX, lastX);
        // What arrived from the outside world. Printed as counts because the colour
        // on the screen answers a weaker question: it says a click was delivered at
        // least once, and says nothing about a second one, or about a key — which
        // travels the same wire and is routed by a different rule at the other end.
        std::printf("[shell] input: %d click(s) reached a MouseArea, "
                    "%d key(s) reached the scene, the last was Qt key %d\n",
                    root->property("clicks").toInt(), root->property("keys").toInt(),
                    root->property("lastKey").toInt());
        std::fflush(stdout);
        QGuiApplication::quit();
    });

    std::printf("[shell] entering the event loop\n");
    std::fflush(stdout);
    const int code = app.exec();
    std::printf("[shell] exec returned %d\n", code);
    std::fflush(stdout);
    return code;
}
