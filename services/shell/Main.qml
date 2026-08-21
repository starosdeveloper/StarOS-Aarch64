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
//   MouseArea          the whole input path: a tablet on a virtqueue, a driver in
//                      EL0, the compositor's hit test, the plugin's translation,
//                      and Qt Quick's own delivery to the item under the pointer
//   Keys.onPressed     the same path for the keyboard, which is routed by focus
//                      rather than by position and is therefore a different
//                      decision in the compositor, not a different wire
//
// `MouseArea` was left out of the first version of this file on purpose, and the
// reason is worth keeping: there was no pointer on this system then, so it would
// have built, run and silently never fired — a green light for a path that did not
// exist. It is here now because the path is.

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
    height: 300
    color: "#181820"

    // How many clicks and keystrokes have arrived. Read from C++ at the end of the
    // run by `objectName`, so the log carries a number and not an impression — a
    // colour on the screen says a click was delivered *at least* once, and a count
    // says whether the second one arrived too.
    property int clicks: 0
    property int keys: 0
    property int lastKey: 0

    // Keys come to the root because the window has the focus, not the item: the
    // compositor routes a key to whichever *process* claimed the keyboard, and which
    // item inside it receives is Qt's own business.
    focus: true
    Keys.onPressed: function (event) {
        root.keys += 1;
        root.lastKey = event.key;
        typed.text = event.text.length > 0 ? "typed: " + event.text : "key " + event.key;
        event.accepted = true;
    }

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

    // The button. Everything above it is output; this is the first thing in the
    // scene that is an *answer*.
    //
    // Below the animated items rather than beside them, because the check clicks
    // where it finds this colour and then looks for the new one in the same place —
    // and the circle sliding across it would be a third colour arriving in that
    // rectangle at a moment nothing controls.
    //
    // Two colours far apart in every channel. A click that half-works — the event
    // delivered to the window but not to the item — leaves this orange, and orange
    // is not nearly green in any channel order the framebuffer might be in.
    Rectangle {
        id: button

        objectName: "button"

        x: 24
        y: 236
        width: 272
        height: 48
        radius: 8
        color: root.clicks > 0 ? "#00ff00" : "#ff8000"

        Text {
            id: typed

            anchors.centerIn: parent
            text: root.clicks > 0 ? "clicked" : "click me"
            color: "#101010"
            font.family: "IBM Plex Mono"
            font.pixelSize: 18
        }

        // `anchors.fill`, so the area is exactly the rectangle that changes colour.
        // An area larger than what it recolours is a check that passes when the hit
        // test is off by a few pixels, which is the error worth catching: a
        // compositor that routed by window position instead of by surface would be
        // wrong by exactly the window's offset on screen.
        MouseArea {
            anchors.fill: parent
            onClicked: root.clicks += 1
        }
    }
}
