#include "qstarosinput.h"

#include "qstaroswindow.h"

#include <QtCore/qsocketnotifier.h>
#include <QtGui/qguiapplication.h>
#include <QtGui/qwindow.h>
#include <qpa/qwindowsysteminterface.h>

#include <poll.h>
#include <staros.h>
#include <string.h>
#include <unistd.h>

namespace {

// Linux key codes this plugin knows how to name.
//
// A table and not a formula: the input layer's numbering follows the rows of a
// keyboard, and Qt's follows Unicode, so nothing connects them but this. It covers
// the keys a program can expect a person to press — letters, digits, the row of
// punctuation, the editing and arrow keys, the modifiers. Everything else arrives
// as key code 0, which Qt treats as "a key with no name", and the text is still
// delivered if there is any.
//
// The alternative was `QEvdevKeyboardHandler`, which is Qt's own and does far more
// — keymaps, compose sequences, dead keys. It reads `/dev/input` directly, and
// there is no `/dev` here; taking the table out of it and leaving the device
// handling behind is the part that transfers.
struct KeyEntry
{
    unsigned short code;
    int key;
    char plain;
    char shifted;
};

const KeyEntry KeyTable[] = {
    { 1, Qt::Key_Escape, 0, 0 },
    { 2, Qt::Key_1, '1', '!' },
    { 3, Qt::Key_2, '2', '@' },
    { 4, Qt::Key_3, '3', '#' },
    { 5, Qt::Key_4, '4', '$' },
    { 6, Qt::Key_5, '5', '%' },
    { 7, Qt::Key_6, '6', '^' },
    { 8, Qt::Key_7, '7', '&' },
    { 9, Qt::Key_8, '8', '*' },
    { 10, Qt::Key_9, '9', '(' },
    { 11, Qt::Key_0, '0', ')' },
    { 12, Qt::Key_Minus, '-', '_' },
    { 13, Qt::Key_Equal, '=', '+' },
    { 14, Qt::Key_Backspace, 0, 0 },
    { 15, Qt::Key_Tab, '\t', '\t' },
    { 16, Qt::Key_Q, 'q', 'Q' },
    { 17, Qt::Key_W, 'w', 'W' },
    { 18, Qt::Key_E, 'e', 'E' },
    { 19, Qt::Key_R, 'r', 'R' },
    { 20, Qt::Key_T, 't', 'T' },
    { 21, Qt::Key_Y, 'y', 'Y' },
    { 22, Qt::Key_U, 'u', 'U' },
    { 23, Qt::Key_I, 'i', 'I' },
    { 24, Qt::Key_O, 'o', 'O' },
    { 25, Qt::Key_P, 'p', 'P' },
    { 26, Qt::Key_BracketLeft, '[', '{' },
    { 27, Qt::Key_BracketRight, ']', '}' },
    { 28, Qt::Key_Return, '\r', '\r' },
    { 29, Qt::Key_Control, 0, 0 },
    { 30, Qt::Key_A, 'a', 'A' },
    { 31, Qt::Key_S, 's', 'S' },
    { 32, Qt::Key_D, 'd', 'D' },
    { 33, Qt::Key_F, 'f', 'F' },
    { 34, Qt::Key_G, 'g', 'G' },
    { 35, Qt::Key_H, 'h', 'H' },
    { 36, Qt::Key_J, 'j', 'J' },
    { 37, Qt::Key_K, 'k', 'K' },
    { 38, Qt::Key_L, 'l', 'L' },
    { 39, Qt::Key_Semicolon, ';', ':' },
    { 40, Qt::Key_Apostrophe, '\'', '"' },
    { 41, Qt::Key_QuoteLeft, '`', '~' },
    { 42, Qt::Key_Shift, 0, 0 },
    { 43, Qt::Key_Backslash, '\\', '|' },
    { 44, Qt::Key_Z, 'z', 'Z' },
    { 45, Qt::Key_X, 'x', 'X' },
    { 46, Qt::Key_C, 'c', 'C' },
    { 47, Qt::Key_V, 'v', 'V' },
    { 48, Qt::Key_B, 'b', 'B' },
    { 49, Qt::Key_N, 'n', 'N' },
    { 50, Qt::Key_M, 'm', 'M' },
    { 51, Qt::Key_Comma, ',', '<' },
    { 52, Qt::Key_Period, '.', '>' },
    { 53, Qt::Key_Slash, '/', '?' },
    { 54, Qt::Key_Shift, 0, 0 },
    { 56, Qt::Key_Alt, 0, 0 },
    { 57, Qt::Key_Space, ' ', ' ' },
    { 58, Qt::Key_CapsLock, 0, 0 },
    { 102, Qt::Key_Home, 0, 0 },
    { 103, Qt::Key_Up, 0, 0 },
    { 105, Qt::Key_Left, 0, 0 },
    { 106, Qt::Key_Right, 0, 0 },
    { 107, Qt::Key_End, 0, 0 },
    { 108, Qt::Key_Down, 0, 0 },
    { 110, Qt::Key_Insert, 0, 0 },
    { 111, Qt::Key_Delete, 0, 0 },
    { 125, Qt::Key_Meta, 0, 0 },
    { 126, Qt::Key_Meta, 0, 0 },
};

const KeyEntry *lookup(unsigned short code)
{
    for (const KeyEntry &entry : KeyTable) {
        if (entry.code == code)
            return &entry;
    }
    return nullptr;
}

// Which modifier a key *is*, if it is one. Returned separately from the key it
// produces because a modifier is both: Qt is told that Shift was pressed as a key
// event, and every event after it carries Shift in its modifier state.
Qt::KeyboardModifier modifierFor(int key)
{
    switch (key) {
    case Qt::Key_Shift:
        return Qt::ShiftModifier;
    case Qt::Key_Control:
        return Qt::ControlModifier;
    case Qt::Key_Alt:
        return Qt::AltModifier;
    case Qt::Key_Meta:
        return Qt::MetaModifier;
    default:
        return Qt::NoModifier;
    }
}

// The 32-bit halves of a packed pair, as the display protocol packs them.
int low(quint64 word)
{
    return int(word & 0xffffffffu);
}

int high(quint64 word)
{
    return int(word >> 32);
}

} // namespace

QStarosInput::QStarosInput(int fd, QObject *parent) : QObject(parent), m_fd(fd)
{
    if (m_fd < 0)
        return;
    m_notifier = new QSocketNotifier(m_fd, QSocketNotifier::Read, this);
    QObject::connect(m_notifier, &QSocketNotifier::activated, this, [this]() { readReady(); });
}

QStarosInput::~QStarosInput() = default;

void QStarosInput::readReady()
{
    // Drain, rather than take one and return.
    //
    // The notifier is level-triggered, so one event per wake-up would still make
    // progress — and it would make it one event per pass of the event loop, which
    // for a pointer moving at sixty positions a second is a queue that grows while
    // the scene is being rendered. What the user sees is a cursor lagging further
    // behind the longer they move it.
    for (;;) {
        // Ask before taking. `staros_msg_recv` blocks, and blocking here would park
        // the GUI thread inside a notifier callback with the whole event loop behind
        // it — a program that stops repainting until the next keystroke. `poll` with
        // no timeout to wait is the documented way to ask an endpoint whether it has
        // anything, and it is the same call the dispatcher is built on.
        struct pollfd ready;
        ready.fd = m_fd;
        ready.events = POLLIN;
        ready.revents = 0;
        if (poll(&ready, 1, 0) != 1)
            return;

        struct staros_message msg;
        memset(&msg, 0, sizeof msg);
        if (staros_msg_recv(m_fd, &msg) != 0)
            return;
        switch (msg.tag) {
        case STAROS_INPUT_KEY:
            deliverKey(msg.words[0], msg.words[1]);
            break;
        case STAROS_INPUT_POINTER:
            deliverPointer(QPoint(low(msg.words[0]), high(msg.words[0])),
                           QPoint(low(msg.words[1]), high(msg.words[1])), msg.words[2],
                           msg.words[3]);
            break;
        default:
            // A tag this plugin does not know. Ignored rather than guessed at: the
            // compositor is entitled to grow a message this Qt build predates.
            break;
        }
    }
}

void QStarosInput::deliverKey(quint64 code, quint64 value)
{
    // Keys go to the focus window, because the focus is what the compositor routed
    // by: it sent this key here on the strength of this process having claimed the
    // keyboard, and which of its own windows is focused is Qt's own bookkeeping.
    QWindow *window = QGuiApplication::focusWindow();
    if (window == nullptr)
        return;

    const KeyEntry *entry = lookup((unsigned short)code);
    const int key = entry != nullptr ? entry->key : 0;
    const bool pressed = value != 0;

    // A modifier updates the state *before* the event carrying it is sent, so that
    // Shift's own key event already says Shift is down — which is what every other
    // platform reports, and what a program checking `event->modifiers()` on a
    // modifier press expects.
    const Qt::KeyboardModifier mod = modifierFor(key);
    if (mod != Qt::NoModifier) {
        m_modifiers.setFlag(mod, pressed);
    }

    QString text;
    if (entry != nullptr) {
        const char ch = m_modifiers.testFlag(Qt::ShiftModifier) ? entry->shifted : entry->plain;
        // Control characters are not text. `Qt::Key_Return` carries a carriage
        // return in the table because a text widget wants one, but a control-key
        // combination must not: Ctrl+C is a shortcut, and delivering "c" as its text
        // is how a key combination ends up typed into a field as well as acted on.
        if (ch != 0 && !m_modifiers.testFlag(Qt::ControlModifier))
            text = QString(QChar::fromLatin1(ch));
    }

    // `value == 2` is auto-repeat in the Linux input layer, and it arrives here
    // as a press that was never released. Reported as a repeat rather than as a
    // fresh press: a program that counts key presses would otherwise count a held
    // key thirty times a second.
    QWindowSystemInterface::handleKeyEvent(window, pressed ? QEvent::KeyPress : QEvent::KeyRelease,
                                           key, m_modifiers, text, value == 2);
}

void QStarosInput::deliverPointer(const QPoint &local, const QPoint &global, quint64 buttons,
                                  quint64 surface)
{
    QWindow *window = windowFor(surface);
    if (window == nullptr)
        return;

    Qt::MouseButtons state = Qt::NoButton;
    if (buttons & STAROS_BUTTON_LEFT)
        state |= Qt::LeftButton;
    if (buttons & STAROS_BUTTON_RIGHT)
        state |= Qt::RightButton;
    if (buttons & STAROS_BUTTON_MIDDLE)
        state |= Qt::MiddleButton;

    // Which button changed, and in which direction. The compositor sends the whole
    // state — deliberately, so that a lost message is recovered from rather than
    // remembered wrongly — and Qt wants the transition, so the difference is taken
    // here, where the previous state is the one this plugin last told Qt about.
    const Qt::MouseButtons changed = state ^ m_buttons;
    m_buttons = state;

    if (changed == Qt::NoButton) {
        QWindowSystemInterface::handleMouseEvent(window, QPointF(local), QPointF(global), state,
                                                 Qt::NoButton, QEvent::MouseMove, m_modifiers);
        return;
    }

    // One event per button that changed. Two buttons changing in one message is
    // possible — a message can be lost, or two can be pressed between two positions
    // — and collapsing them into one event loses a press that a program is waiting
    // for.
    static const Qt::MouseButton Buttons[] = { Qt::LeftButton, Qt::RightButton,
                                               Qt::MiddleButton };
    for (Qt::MouseButton button : Buttons) {
        if (!(changed & button))
            continue;
        const bool down = state & button;
        QWindowSystemInterface::handleMouseEvent(
            window, QPointF(local), QPointF(global), state, button,
            down ? QEvent::MouseButtonPress : QEvent::MouseButtonRelease, m_modifiers);
    }
}

QWindow *QStarosInput::windowFor(quint64 surface)
{
    if (surface == 0)
        return nullptr;
    const QWindowList windows = QGuiApplication::allWindows();
    for (QWindow *window : windows) {
        QPlatformWindow *handle = window->handle();
        if (handle != nullptr && quint64(handle->winId()) == surface)
            return window;
    }
    return nullptr;
}
