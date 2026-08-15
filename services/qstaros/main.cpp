// The plugin's front door: the class Qt's factory instantiates when it is asked for
// the platform named "staros".
//
// It is thin on purpose. Everything it does is decide whether this plugin is the one
// being asked for and, if so, hand back an integration — and it returns null in two
// separate cases that are worth telling apart:
//
//   * the name does not match, which is Qt trying each plugin in turn and is not a
//     failure at all;
//   * the name matches and the integration could not connect to the display server,
//     which is a failure, and returning null is how QPA is told so. Qt then prints
//     its own diagnostic naming the plugin, which is more use than a plugin that
//     constructs successfully and paints nothing.
//
// On this system the plugin is linked into the application rather than loaded from a
// file — `dlopen` refuses here and qtbase is configured `-no-feature-dlopen` — so
// what makes this class reachable is `Q_IMPORT_PLUGIN(QStarosIntegrationPlugin)` in
// the program, not a search of a plugins directory. The metadata below is still
// read: Qt's static plugin registry carries the same JSON, and the "staros" key in
// it is what `QT_QPA_PLATFORM=staros` matches against.

#include <qpa/qplatformintegrationplugin.h>

#include "qstarosintegration.h"

QT_BEGIN_NAMESPACE

using namespace Qt::StringLiterals;

class QStarosIntegrationPlugin : public QPlatformIntegrationPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID QPlatformIntegrationFactoryInterface_iid FILE "staros.json")
public:
    QPlatformIntegration *create(const QString &system, const QStringList &params) override;
};

QPlatformIntegration *QStarosIntegrationPlugin::create(const QString &system,
                                                      const QStringList &params)
{
    Q_UNUSED(params);
    if (system.compare("staros"_L1, Qt::CaseInsensitive) != 0)
        return nullptr;

    auto *integration = new QStarosIntegration;
    if (!integration->isValid()) {
        // No display server on this machine. The kernel installs the capability
        // regardless of what it booted on, so this is discovered by trying rather
        // than by asking — see QStarosConnection.
        delete integration;
        return nullptr;
    }
    return integration;
}

QT_END_NAMESPACE

#include "main.moc"
