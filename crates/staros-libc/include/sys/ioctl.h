/* sys/ioctl.h — the device control call, and a short list of the requests callers
 * name.
 *
 * `crates/staros-libc/src/file.rs` implements `ioctl` as a refusal with `ENOTTY`,
 * and that is not a placeholder: the descriptors this system hands out are files in
 * an initramfs archive and pipes, none of which is a terminal or a device. `ENOTTY`
 * is precisely the errno for "this descriptor is not the kind of thing you are
 * asking about", so a caller probing for a terminal gets the right answer and
 * takes its non-terminal path.
 *
 * The variadic third argument is what the interface is: the type of the argument
 * depends on the request, which is why `ioctl` cannot be type-checked and why the
 * requests are macros with the sizes encoded in them.
 */
#ifndef _SYS_IOCTL_H
#define _SYS_IOCTL_H 1

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* How a request number is built.
 *
 * An `ioctl` number is not arbitrary: it packs the direction, the size of the
 * argument, a per-driver "type" byte and an index into 32 bits, so that a driver
 * handed a request meant for a different driver can tell. Code that defines its own
 * requests writes them with these macros rather than as constants — Qt's
 * `qfilesystemengine_unix.cpp` line 69 does, for `FICLONE`, when the kernel headers
 * are too old to have it.
 *
 * The layout is Linux's, which is the ABI this presents: size at bit 16, direction
 * at bit 30. Nothing here decodes them — `ioctl` refuses every request — but a
 * request built with these has the same number it would have on Linux, which is what
 * makes a program's own `#define` agree with a driver's. */
#define _IOC_NRBITS   8
#define _IOC_TYPEBITS 8
#define _IOC_SIZEBITS 14
#define _IOC_DIRBITS  2

#define _IOC_NRSHIFT   0
#define _IOC_TYPESHIFT (_IOC_NRSHIFT + _IOC_NRBITS)
#define _IOC_SIZESHIFT (_IOC_TYPESHIFT + _IOC_TYPEBITS)
#define _IOC_DIRSHIFT  (_IOC_SIZESHIFT + _IOC_SIZEBITS)

/* Direction as seen by the *program*: `_IOW` means the program writes and the
 * driver reads. It is the opposite of what the name suggests to most readers, and
 * getting it backwards produces a number no driver recognises. */
#define _IOC_NONE  0U
#define _IOC_WRITE 1U
#define _IOC_READ  2U

#define _IOC(dir, type, nr, size)             \
    (((dir) << _IOC_DIRSHIFT) |               \
     ((type) << _IOC_TYPESHIFT) |             \
     ((nr) << _IOC_NRSHIFT) |                 \
     ((size) << _IOC_SIZESHIFT))

#define _IO(type, nr)          _IOC(_IOC_NONE, (type), (nr), 0)
#define _IOR(type, nr, argtype)  _IOC(_IOC_READ, (type), (nr), sizeof(argtype))
#define _IOW(type, nr, argtype)  _IOC(_IOC_WRITE, (type), (nr), sizeof(argtype))
#define _IOWR(type, nr, argtype) _IOC(_IOC_READ | _IOC_WRITE, (type), (nr), sizeof(argtype))

/* The terminal-size request, which is the only one anything in a Qt build asks:
 * `qlogging.cpp` uses it to decide whether to colour its output. The number is
 * Linux's, because this presents a Linux ABI — see the note in
 * `services/qstaros/staros-toolchain.cmake` on CMAKE_SYSTEM_NAME. */
#define TIOCGWINSZ 0x5413
#define TIOCSWINSZ 0x5414
#define FIONREAD   0x541B
#define FIONBIO    0x5421

/* What TIOCGWINSZ would fill in. Declared because callers declare one on the stack
 * and pass its address; never written, since the call refuses. */
struct winsize {
    unsigned short ws_row;
    unsigned short ws_col;
    unsigned short ws_xpixel;
    unsigned short ws_ypixel;
};

int ioctl(int fd, unsigned long request, ...);

#ifdef __cplusplus
}
#endif

#endif /* sys/ioctl.h */
