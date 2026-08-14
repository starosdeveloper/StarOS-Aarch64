/* libintl.h — message translation, on a system with no message catalogues.
 *
 * Found by QML: libstdc++'s `std::messages` facet is built on gettext for a glibc
 * target, and `<locale>` reaches it through QV4's includes. A libstdc++ configured
 * with `--enable-clocale=generic` would not need this at all — which is what a
 * proper cross toolchain does, and is the right fix when Qt is actually built.
 *
 * Until then these are the honest implementations rather than absent ones:
 * translation of a string, when no catalogue exists, is the string. That is not a
 * stub — it is what gettext itself returns when it finds no translation, and a
 * program cannot tell the difference because there is none to tell.
 *
 * `bindtextdomain` returns null: there is no directory of catalogues, and a caller
 * that checks the result learns so. Answering with the path it was given would
 * claim a catalogue had been located.
 */
#ifndef _LIBINTL_H
#define _LIBINTL_H 1

#ifdef __cplusplus
extern "C" {
#endif

char *gettext(const char *msgid);
char *dgettext(const char *domain, const char *msgid);
char *dcgettext(const char *domain, const char *msgid, int category);
char *ngettext(const char *singular, const char *plural, unsigned long n);
char *textdomain(const char *domain);
char *bindtextdomain(const char *domain, const char *dirname);

#ifdef __cplusplus
}
#endif

#endif /* libintl.h */
