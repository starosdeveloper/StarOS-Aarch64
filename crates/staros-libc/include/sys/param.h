/* sys/param.h — the BSD grab bag.
 *
 * It has no definition of its own. Historically it is where BSD kept whatever did
 * not fit elsewhere, and what survives into modern code is `MIN`, `MAX`, and the
 * expectation that including it brings in <limits.h> and the basic types. Code
 * includes it for the macros and gets the rest by accident, so a header that omitted
 * them would compile until it didn't.
 *
 * No functions, no symbols.
 */
#ifndef _SYS_PARAM_H
#define _SYS_PARAM_H 1

#include <limits.h>
#include <sys/types.h>

/* The double evaluation is real: `MIN(i++, j)` increments twice. It is what every
 * system's <sys/param.h> does, and code written against them relies on nothing more,
 * so a safer statement-expression version here would differ from every other
 * platform in a way that only shows up as a behaviour change. C++ callers have
 * `std::min`. */
#ifndef MIN
#define MIN(a, b) ((a) < (b) ? (a) : (b))
#endif
#ifndef MAX
#define MAX(a, b) ((a) > (b) ? (a) : (b))
#endif

/* Round up to a power-of-two boundary. `size` must be a power of two, which the
 * masking assumes and does not check. */
#define roundup(x, size) ((((x) + ((size) - 1)) / (size)) * (size))
#define howmany(x, size) (((x) + ((size) - 1)) / (size))

/* The page size, which on this system is fixed rather than asked for at run time.
 * `sysconf(_SC_PAGESIZE)` gives the same 4096 from the same fact: the kernel's page
 * tables are built for 4 KiB pages and there is no configuration in which they are
 * not. */
#define PAGE_SIZE  4096
#define PAGE_SHIFT 12

/* Byte order, for code that expects `sys/param.h` to supply it. <endian.h> is where
 * it is decided; this is the same answer, not a second one. */
#include <endian.h>

#endif /* sys/param.h */
