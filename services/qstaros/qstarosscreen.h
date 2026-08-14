// The screen, as Qt sees it.
//
// Everything here is one round trip to `displaysrv`, made once at start-up. Qt asks
// for the geometry before it creates a window and again whenever it lays one out,
// and a `QPlatformScreen` that sent a message for each answer would put an IPC round
// trip inside every layout pass.
//
// The stride is deliberately not part of this. The display server does not report
// it, because it is the server's business and a client that knew it would sooner or
// later assume its own buffer had one; Qt's `QImage` over a client buffer is packed,
// and packed is what the protocol promises.

#pragma once

#include <qpa/qplatformscreen.h>

#include "qstarosconnection.h"

class QStarosScreen : public QPlatformScreen
{
public:
    explicit QStarosScreen(QStarosConnection *connection);

    QRect geometry() const override { return m_geometry; }
    int depth() const override { return m_depth; }
    QImage::Format format() const override { return m_format; }

    // Physical size, and why it is a guess rather than a measurement.
    //
    // Qt divides the pixel size by this to get a DPI, and a QML application scales
    // its fonts by it. Nothing in this system knows the physical size of the panel:
    // `ramfb` reports pixels and the Raspberry Pi's firmware reports pixels, and
    // EDID — which does carry millimetres — is not read by anything here yet. So
    // this returns the size that makes the DPI come out at 96, which is the number
    // Qt itself falls back to, and says so rather than inventing a panel.
    QSizeF physicalSize() const override;

    QDpi logicalDpi() const override { return QDpi(96, 96); }
    qreal refreshRate() const override { return 60; }

    // No sub-screens, no rotation. Both are protocol changes before they are Qt
    // changes, and a plugin that claimed either would be describing a display
    // server that cannot do it.
    Qt::ScreenOrientation nativeOrientation() const override { return Qt::LandscapeOrientation; }
    Qt::ScreenOrientation orientation() const override { return Qt::LandscapeOrientation; }

    QStarosConnection *connection() const { return m_connection; }

private:
    QStarosConnection *m_connection;
    QRect m_geometry;
    int m_depth = 32;
    QImage::Format m_format = QImage::Format_RGB32;
};
