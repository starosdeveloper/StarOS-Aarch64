# The QPA plugin, built inside qtbase's own tree.
#
# It lives here in this repository and is symlinked into
# `qtbase-everywhere-src-6.11.1/src/plugins/platforms/staros` — see
# `services/qstaros/README.md`. Building it in-tree rather than against an installed
# Qt is what gives it `Qt::GuiPrivate`: a QPA plugin implements Qt's *private*
# platform interfaces, which are versioned with the build and not part of the
# installed public API, so an out-of-tree plugin would have to reach into headers Qt
# does not promise to keep. In-tree is the arrangement every other platform plugin
# uses, and it is the reason this file is a `qt_internal_add_plugin` and not an
# ordinary `add_library`.

qt_internal_add_plugin(QStarosIntegrationPlugin
    OUTPUT_NAME qstaros
    PLUGIN_TYPE platforms
    # Makes this the plugin Qt uses when nothing names another, provided configure
    # was told to build it — which `-qpa staros` does. Without this a program has to
    # be started with `-platform staros` or `QT_QPA_PLATFORM=staros`, and on a
    # machine whose only display path is this one that is a step with no alternative
    # to choose between.
    DEFAULT_IF "staros" IN_LIST QT_QPA_PLATFORMS
    SOURCES
        main.cpp
        qstarosconnection.cpp qstarosconnection.h
        qstarosscreen.cpp qstarosscreen.h
        qstaroswindow.cpp qstaroswindow.h
        qstarosbackingstore.cpp qstarosbackingstore.h
        qstarosinput.cpp qstarosinput.h
        qstarosintegration.cpp qstarosintegration.h
    DEFINES
        QT_NO_FOREACH
    LIBRARIES
        Qt::Core
        Qt::CorePrivate
        Qt::Gui
        Qt::GuiPrivate
)

# FreeType, for the font database. The plugin returns `QFreeTypeFontDatabase`, which
# is Qt's own implementation and lives in QtGui — but it is only compiled in when
# the feature is on, so the dependency is stated where the condition is.
qt_internal_extend_target(QStarosIntegrationPlugin CONDITION QT_FEATURE_freetype
    LIBRARIES
        WrapFreetype::WrapFreetype
)
