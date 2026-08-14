// `qstaros` — the QPA plugin: the layer that knows what machine Qt is on.
//
// Everything under it exists and has been exercised without Qt. The display server
// composites several surfaces with honest damage and takes a crashed client's
// windows down; input is routed by the compositor to whichever window claimed the
// focus; `staros.h` is the C interface both compilers have compiled; the C library
// is measured against a real Qt link *and* against what a C++ compile demands. This
// class is where those meet Qt's names for them.
//
// What is deliberately not here: OpenGL (`-no-opengl`, and there is no GPU driver),
// a theme, a cursor, a clipboard. The last three are optional by QPA's own contract,
// which is why they can be absent rather than stubbed — an optional interface Qt
// asks for and does not get is a feature it turns off, and a stub that answers
// wrongly is a feature it uses.

#pragma once

#include <qpa/qplatformintegration.h>

#include "qstarosconnection.h"

class QStarosScreen;

class QStarosIntegration : public QPlatformIntegration
{
public:
    QStarosIntegration();
    ~QStarosIntegration() override;

    bool hasCapability(QPlatformIntegration::Capability cap) const override;
    QPlatformWindow *createPlatformWindow(QWindow *window) const override;
    QPlatformBackingStore *createPlatformBackingStore(QWindow *window) const override;
    QAbstractEventDispatcher *createEventDispatcher() const override;
    QPlatformFontDatabase *fontDatabase() const override;

    // Whether this plugin can run at all. False on a machine with no display
    // server, which is a fact about the machine — the kernel installs the
    // capability either way, because it does not know what it booted on.
    bool isValid() const { return m_connection.isValid(); }

private:
    mutable QStarosConnection m_connection;
    QStarosScreen *m_screen = nullptr;
    QPlatformFontDatabase *m_fontDatabase = nullptr;
};
