/* sys/utsname.h — what the system calls itself.
 *
 * Implemented in `crates/staros-libc/src/proc.rs`, which fills every field with a
 * true answer: `sysname` is StarOS, `machine` is aarch64, `release` is this kernel's
 * version. Qt reads all of it — `QSysInfo::kernelType()`, `kernelVersion()` and
 * `machineHostName()` are `uname` and nothing else — and it ends up in
 * `qVersion()`-adjacent output and in the QML `Qt.platform` object.
 *
 * The field width is glibc's 65 and the six-field layout is glibc's too, including
 * `domainname`, which POSIX does not have. That is deliberate for the reason given
 * throughout this sysroot: a program compiled against a Linux sysroot reads these by
 * offset once the compiler is done, and a five-field struct would put `machine`
 * where the caller expects `version`. The static assertion on the total size in the
 * Rust side is what holds the two halves together.
 */
#ifndef _SYS_UTSNAME_H
#define _SYS_UTSNAME_H 1

#ifdef __cplusplus
extern "C" {
#endif

#define _UTSNAME_LENGTH 65

struct utsname {
    char sysname[_UTSNAME_LENGTH];
    char nodename[_UTSNAME_LENGTH];
    char release[_UTSNAME_LENGTH];
    char version[_UTSNAME_LENGTH];
    char machine[_UTSNAME_LENGTH];
    /* Not in POSIX; in glibc, and therefore in the ABI this presents. */
    char domainname[_UTSNAME_LENGTH];
};

int uname(struct utsname *out);

#ifdef __cplusplus
}
#endif

#endif /* sys/utsname.h */
