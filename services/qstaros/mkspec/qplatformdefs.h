// qplatformdefs.h — the mkspec header, which is the file a Qt port writes.
//
// Qt's `qcore_unix_p.h` includes this by plain name and expects it to have pulled
// in the platform's headers and defined the `QT_*` aliases QtCore calls things
// through. Every port has one; `mkspecs/linux-g++/qplatformdefs.h` is Linux's, and
// it includes `sys/ipc.h`, `sys/shm.h`, `sys/socket.h`, `netinet/in.h`, `grp.h` and
// `pwd.h` — four subsystems this system does not have and two databases it has no
// file for.
//
// So this is written rather than borrowed, and every alias points at something that
// exists. Where Linux's version names a function this system does not implement,
// the alias is simply absent: QtCore guards its uses on the feature being
// configured, and a macro pointing at a symbol nobody wrote fails at the link with
// a name — which is the good failure, but a failure this file can avoid by not
// making the claim.
//
// Large-file support needs no variant. `off_t` has been 64 bits on this target from
// the first line of the C library, so `QT_STATBUF` is `struct stat` and `QT_LSEEK`
// is `::lseek`; the `*64` spellings exist in the sysroot as the same names.

#ifndef QPLATFORMDEFS_H
#define QPLATFORMDEFS_H

#include "qglobal.h"

#include <dirent.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// The file-system aliases. QtCore calls files through these names so that one
// header decides whether a platform uses the 64-bit variants; here there is only
// one of each.
#define QT_STATBUF              struct stat
#define QT_STAT                 ::stat
#define QT_FSTAT                ::fstat
#define QT_LSTAT                ::lstat
#define QT_STAT_MASK            S_IFMT
#define QT_STAT_REG             S_IFREG
#define QT_STAT_DIR             S_IFDIR
#define QT_STAT_LNK             S_IFLNK

#define QT_OFF_T                off_t
#define QT_OPEN                 ::open
#define QT_CLOSE                ::close
#define QT_LSEEK                ::lseek
#define QT_READ                 ::read
#define QT_WRITE                ::write
#define QT_ACCESS               ::access
#define QT_GETCWD               ::getcwd
#define QT_CHDIR                ::chdir
#define QT_MKDIR                ::mkdir
#define QT_RMDIR                ::rmdir
#define QT_OPEN_LARGEFILE       0
#define QT_OPEN_RDONLY          O_RDONLY
#define QT_OPEN_WRONLY          O_WRONLY
#define QT_OPEN_RDWR            O_RDWR
#define QT_OPEN_CREAT           O_CREAT
#define QT_OPEN_TRUNC           O_TRUNC
#define QT_OPEN_APPEND          O_APPEND
#define QT_OPEN_EXCL            O_EXCL

#define QT_FILENO               ::fileno
#define QT_FOPEN                ::fopen
#define QT_FSEEK                ::fseeko
#define QT_FTELL                ::ftello
#define QT_FGETPOS              ::fgetpos
#define QT_FSETPOS              ::fsetpos
#define QT_FPOS_T               fpos_t
#define QT_MMAP                 ::mmap
#define QT_FTRUNCATE            ::ftruncate

#define QT_SNPRINTF             ::snprintf
#define QT_VSNPRINTF            ::vsnprintf

// Directory traversal. `QT_READDIR_R` is deliberately not defined: `readdir_r` is
// deprecated everywhere and absent here, and QtCore falls back to `readdir` when it
// is missing — which is what this system provides and what `crates/staros-libc/
// src/dir.rs` implements over a flat archive.
#define QT_DIR                  DIR
#define QT_OPENDIR              ::opendir
#define QT_CLOSEDIR             ::closedir
#define QT_DIRENT               struct dirent
#define QT_READDIR              ::readdir

// Sockets are absent, and this is the only honest way to say so: QtCore guards
// every use behind the network feature, which a Qt configured for this system has
// turned off. Defining aliases for a `::socket` nobody wrote would move the failure
// from configuration time to link time for no gain.
#define QT_NO_SOCKET_H

#endif // QPLATFORMDEFS_H
