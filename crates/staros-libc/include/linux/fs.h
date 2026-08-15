/* linux/fs.h — the filesystem ioctl numbers, and only those.
 *
 * The first header in this sysroot under `linux/`, and the directory is worth a word
 * because it means something different from the rest. Everything else here is *this*
 * system's C library. `linux/` is the kernel's own UAPI namespace: numbers and
 * structures a program shares with a Linux kernel. Presenting a Linux ABI means
 * these numbers have to be Linux's exactly, and nothing in them is ours to choose.
 *
 * What is here is what Qt asks for. `qfilesystemengine_unix.cpp` line 1170 tries
 * `ioctl(dst, FICLONE, src)` before it copies a file byte by byte — a reflink, where
 * the filesystem shares the blocks instead of duplicating them. This system has no
 * such thing, `ioctl` refuses with `ENOTTY`, and Qt takes its ordinary copy path.
 * That is the whole intended behaviour: the request exists so it can be refused by a
 * caller that knows how to carry on.
 *
 * The number is built with the `_IOC` macros in <sys/ioctl.h> and comes out as
 * `0x40049409`, which is what it is on Linux. Qt would define it itself if this
 * header lacked it — and would then need those same macros, which is the real reason
 * both files changed together.
 */
#ifndef _LINUX_FS_H
#define _LINUX_FS_H 1

#include <sys/ioctl.h>

/* Share this file's blocks with another file's. Type byte 0x94 is the VFS's. */
#define FICLONE _IOW(0x94, 9, int)

/* The partial form, which takes a range instead of the whole file. */
struct file_clone_range {
    long long src_fd;
    unsigned long long src_offset;
    unsigned long long src_length;
    unsigned long long dest_offset;
};

#define FICLONERANGE _IOW(0x94, 13, struct file_clone_range)

#endif /* linux/fs.h */
