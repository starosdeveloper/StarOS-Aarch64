#include "qstarosscreen.h"

QStarosScreen::QStarosScreen(QStarosConnection *connection) : m_connection(connection)
{
    m_geometry = QRect(QPoint(0, 0), connection->screenSize());
    m_depth = connection->screenDepth();
    // xRGB8888 is `Format_RGB32` in Qt's naming: 32 bits, the top eight unused.
    // The connection refused to become valid if the server named anything else, so
    // this is a restatement rather than an assumption.
    m_format = QImage::Format_RGB32;
}

QSizeF QStarosScreen::physicalSize() const
{
    // 96 DPI, which is Qt's own fallback, expressed as the millimetres that produce
    // it. Nothing in this system knows the panel's real size: `ramfb` reports pixels
    // and so does the Pi's firmware, and EDID — which does carry millimetres — is
    // read by nothing here yet. Inventing a diagonal would put a wrong number
    // somewhere a QML application multiplies its font sizes by.
    return QSizeF(m_geometry.width() * 25.4 / 96.0, m_geometry.height() * 25.4 / 96.0);
}
