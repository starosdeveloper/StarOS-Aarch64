// Input, from the display server into Qt.
//
// This is the half of the plugin that was missing while a QML scene animated on
// screen and could not be touched. The descriptor was open from the first version —
// `staros_endpoint_fd` turns the event endpoint into something `poll` can wait on,
// which is the whole reason the bridge exists — and nothing read it. A `MouseArea`
// would have compiled, started, and never fired: a green light on a path that did
// not exist.
//
// A `QSocketNotifier` and not a thread. Qt's event dispatcher here is
// `QUnixEventDispatcherQPA`, which is `poll` over descriptors, so an endpoint that
// looks like a descriptor needs nothing else — the notifier fires on the same wait
// that already blocks for timers and for the file server. A reader thread would have
// to hand events across to the GUI thread, and the mechanism for that is a queued
// connection, which is a wake-up on the same descriptor set with extra steps.
//
// What arrives is already routed: the compositor decided which window a key or a
// click belongs to before it sent it. This class does not choose, and must not — it
// is not the process that knows where the windows are.

#pragma once

#include <QtCore/qobject.h>
#include <QtCore/qpoint.h>
#include <Qt>

QT_BEGIN_NAMESPACE
class QSocketNotifier;
class QWindow;
QT_END_NAMESPACE

class QStarosConnection;

class QStarosInput : public QObject
{
    Q_OBJECT
public:
    // `fd` is the event descriptor from the connection, or -1 on a machine with no
    // input at all — a board with a panel and no keyboard is an ordinary machine,
    // not a broken one, so this constructs and does nothing rather than refusing.
    explicit QStarosInput(int fd, QObject *parent = nullptr);
    ~QStarosInput() override;

private:
    void readReady();

    // One message: dispatched by tag to Qt's window-system interface.
    void deliverKey(quint64 code, quint64 value);
    void deliverPointer(const QPoint &local, const QPoint &global, quint64 buttons,
                        quint64 surface);

    // The window a surface id names, or nullptr if it has already been destroyed.
    //
    // Searched among the live windows rather than kept in a map here. The map would
    // be a second record of which windows exist, and the failure it produces is a
    // pointer to a window that was deleted between the compositor sending the event
    // and this reading it — a use-after-free with a message queue in the middle of
    // it. Qt's own list cannot go stale that way.
    static QWindow *windowFor(quint64 surface);

    int m_fd = -1;
    QSocketNotifier *m_notifier = nullptr;
    // Which buttons were down last time. The compositor sends the whole state, so
    // *which* button changed is a difference — and that is what Qt wants, along with
    // the state, in the same call.
    Qt::MouseButtons m_buttons = Qt::NoButton;
    // Which modifiers are held. Tracked from key events because nothing else reports
    // them: this system has no notion of a keyboard state to ask for.
    Qt::KeyboardModifiers m_modifiers = Qt::NoModifier;
};
