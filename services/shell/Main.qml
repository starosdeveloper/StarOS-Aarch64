// The first QML scene on StarOS.
//
// It is read off the filesystem at run time, not compiled into the program: the
// file server serves it exactly as it serves the typefaces, and the path below —
// `qml/Main.qml` in the initramfs — is what `services/shell/main.cpp` opens. That
// is a stronger claim than a string literal in the binary would be, and it means
// this file can be changed without relinking twenty-five megabytes of Qt.
//
// Every element here is chosen because it fails in a different place:
//
//   Rectangle          the scene graph's software renderer reaching the backing
//                      store, which reaches the compositor's shared buffer
//   Text               FreeType and a font database that found real files over IPC
//   NumberAnimation    `ClockNow` and the event loop — an animation is a timer
//                      asking what time it is, sixty times a second
//   the frame counter  that the render loop is a loop and not a single pass
//
// What is deliberately *not* here is `MouseArea`. There is no pointer on this
// system: `services/inputsrv` drives `virtio-keyboard-device` and nothing else, and
// the QPA plugin does not yet hand Qt any input event at all. A `MouseArea` would
// build, run, and silently never fire — a green light for a path that does not
// exist. It goes in the day the input path does.

// `QtQml` explicitly, and not because `QtQuick` fails to depend on it.
//
// `Easing` — the enumeration `easing.type` below names — is registered by the QtQml
// module, and on an ordinary Qt it is in scope after `import QtQuick` alone. Here it
// was not: the scene loaded, every element appeared, and one line in the log said
//
//     file:///qml/Main.qml:84: ReferenceError: Easing is not defined
//
// which is not an error the engine stops for. The animation ran with a default
// easing and everything looked right. That is the shape of failure a static build
// produces — a registration that did not happen is a name that is merely missing,
// and QML resolves a missing name to `undefined` and carries on.
//
// Naming the module is what puts it in scope. It costs one line and it is the line
// that would otherwise have to be discovered again.
import QtQml
import QtQuick

Rectangle {
    id: root

    width: 320
    height: 240
    color: "#181820"

    // The named colour is the point of the first one. `"red"` has to travel through
    // QML's string-to-colour conversion, into a QColor, into the software renderer's
    // fill, into an ARGB32 buffer, and out to a framebuffer that may or may not be
    // in the same channel order. A wrong result here is visible as a wrong colour
    // rather than as nothing at all, which is why the criterion for this phase was
    // written as `Rectangle { color: "red" }` and not as any rectangle.
    Rectangle {
        id: card

        x: 24
        y: 24
        width: 272
        height: 96
        radius: 10
        color: "red"
        border.color: "#f0f0f0"
        border.width: 2

        Text {
            anchors.centerIn: parent
            text: "QML on StarOS"
            color: "#f8f8f8"
            font.family: "IBM Plex Mono"
            font.pixelSize: 20
        }
    }

    // The moving part. `x` is animated rather than opacity or rotation because a
    // position is the one property whose wrongness is unambiguous in a screenshot:
    // it is either at the left edge, or somewhere else, and "somewhere else" cannot
    // be produced by a frozen clock.
    Rectangle {
        id: runner

        // `id` is a QML-only name and does not survive into the meta-object system;
        // `objectName` is the one `QObject::findChild` can see. `main.cpp` reads this
        // rectangle's `x` twice to tell a running animation from a stopped clock, and
        // without this line it would find nothing and report -1 for both samples.
        objectName: "runner"

        y: 160
        width: 32
        height: 32
        radius: 16
        color: "#4a78f0"

        NumberAnimation on x {
            from: 12
            to: root.width - 44
            duration: 1000
            loops: Animation.Infinite
            easing.type: Easing.InOutQuad
        }
    }

    // A second animation on a different property and a different driver, so that
    // "the animation moved" and "the animator is running" are separate observations.
    // A frozen clock stops both; a broken position update stops only the first.
    Rectangle {
        id: pulse

        x: 12
        y: 210
        height: 8
        radius: 4
        color: "#f0c850"

        SequentialAnimation on width {
            loops: Animation.Infinite
            NumberAnimation { from: 8; to: 296; duration: 700 }
            NumberAnimation { from: 296; to: 8; duration: 700 }
        }
    }
}
