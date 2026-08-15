/* sys/sysmacros.h — taking a device number apart.
 *
 * Three macros, no symbols. A `dev_t` packs a major and a minor number, and the
 * packing is not obvious — Linux splits both fields in two and scatters them across
 * 64 bits, so `major(dev)` cannot be written as a shift by anyone who has not looked
 * it up. That is the entire reason this header exists.
 *
 * On this system every `st_dev` is 0: there is one filesystem, the initramfs
 * archive, and nothing to distinguish. So `major` and `minor` of anything this
 * library reports are 0 as well, which is correct rather than degenerate — there is
 * no device, and 0 is what "no device" is numbered.
 *
 * The layout is still Linux's, because a `dev_t` may arrive from somewhere else and
 * because getting the arithmetic right costs nothing.
 */
#ifndef _SYS_SYSMACROS_H
#define _SYS_SYSMACROS_H 1

#include <sys/types.h>

/* Linux's split: major is bits 8–19 and 32–63, minor is 0–7 and 20–31. The
 * interleaving is historical — the fields were widened in place, twice, without
 * moving what was already there. */
#define major(dev)                                                             \
    ((unsigned int)((((unsigned long long)(dev) >> 32) & 0xfffff000u) |        \
                    (((unsigned long long)(dev) >> 8) & 0x00000fffu)))

#define minor(dev)                                                             \
    ((unsigned int)((((unsigned long long)(dev) >> 12) & 0xffffff00u) |        \
                    ((unsigned long long)(dev) & 0x000000ffu)))

#define makedev(ma, mi)                                                        \
    ((dev_t)((((unsigned long long)((ma) & 0xfffff000u)) << 32) |              \
             (((unsigned long long)((ma) & 0x00000fffu)) << 8) |               \
             (((unsigned long long)((mi) & 0xffffff00u)) << 12) |              \
             ((unsigned long long)((mi) & 0x000000ffu))))

#endif /* sys/sysmacros.h */
