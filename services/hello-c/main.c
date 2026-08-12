/*
 * hello-c — a C program running in EL0, to prove the C runtime is real.
 *
 * Nothing in this file knows what a syscall is. It calls printf, malloc, open and
 * clock_gettime, and every one of those goes through `crates/staros-libc`: the
 * console through DebugWrite, the heap through MapAnon, the files through IPC to
 * services/fssrv. There is no libc on this machine other than the one in this tree.
 *
 * The point of it being C rather than more Rust is that it links the way Qt will:
 * clang compiles it against the headers below, rust-lld links it against
 * libstaros_libc.a and the same linker script every EL0 program uses, and the
 * kernel loads the resulting ELF with the loader it already had. If that path works
 * for this file it works for a C++ translation unit too, and the next phase is
 * about the C++ *runtime* rather than about the toolchain.
 *
 * It is deliberately picky about what it checks: not "printf ran" but "printf
 * produced these exact bytes", not "malloc returned a pointer" but "the bytes
 * written through it survive a realloc, and the heap is empty at the end".
 */

typedef unsigned long size_t;
typedef long ssize_t;

/* The subset of the C library this program uses, declared rather than #included:
 * there is no /usr/include here, and a header that lied about a signature would
 * produce a program that links and then reads its arguments from the wrong
 * registers. These match crates/staros-libc exactly. */
int printf(const char *fmt, ...);
int snprintf(char *buf, size_t size, const char *fmt, ...);
int puts(const char *s);
void *malloc(size_t size);
void *calloc(size_t count, size_t size);
void *realloc(void *ptr, size_t size);
void free(void *ptr);
size_t strlen(const char *s);
int strcmp(const char *a, const char *b);
char *strcpy(char *dst, const char *src);
void *memset(void *dst, int byte, size_t n);
int memcmp(const void *a, const void *b, size_t n);
int open(const char *path, int flags, int mode);
ssize_t read(int fd, void *buf, size_t count);
long lseek(int fd, long offset, int whence);
int close(int fd);
void staros_heap_live(size_t *bytes, size_t *blocks);

struct timespec {
    long tv_sec;
    long tv_nsec;
};
int clock_gettime(int clock, struct timespec *out);
int nanosleep(const struct timespec *req, struct timespec *rem);

#define SEEK_SET 0
#define SEEK_END 2

static int failures;

/* Report one check. Everything this program claims goes through here, so a run
 * that prints no FAIL line has actually checked what it says it checked. */
static void check(int ok, const char *what)
{
    if (!ok) {
        failures++;
        printf("[hello-c] FAIL: %s\n", what);
    }
}

/* Layer 1: formatting and strings. The cases here are the ones that are wrong in
 * every hand-written printf: padding a negative number, the length modifiers, and
 * the return value when the buffer is too small. */
static void check_format(void)
{
    char buf[64];
    int n = snprintf(buf, sizeof buf, "%05d|%-6s|%#x|%.2f", -42, "qml", 255, 3.14159);
    check(strcmp(buf, "-0042|qml   |0xff|3.14") == 0, "snprintf output");
    check(n == 22, "snprintf length");

    /* Truncation must report the length that *would* have been written — callers
     * size their buffers from it. */
    char small[4];
    n = snprintf(small, sizeof small, "%d", 123456);
    check(n == 6, "snprintf reports the untruncated length");
    check(strcmp(small, "123") == 0, "snprintf terminates what it did write");

    /* %hhd of 200 is -56: C promotes the argument to int, and the modifier says to
     * narrow it back. A printf that ignores the modifier prints 200. */
    snprintf(buf, sizeof buf, "%hhd", 200);
    check(strcmp(buf, "-56") == 0, "length modifiers narrow");
}

/* Layer 2: the heap. */
static void check_heap(void)
{
    size_t before_bytes, before_blocks;
    staros_heap_live(&before_bytes, &before_blocks);

    char *p = malloc(32);
    check(p != 0, "malloc returned memory");
    strcpy(p, "written through malloc'd memory");

    /* Growing past the block must move the contents, not just the pointer. */
    p = realloc(p, 4096);
    check(p != 0, "realloc returned memory");
    check(strcmp(p, "written through malloc'd memory") == 0, "realloc kept the bytes");

    /* calloc zeroes; a calloc that only allocates is the classic source of a bug
     * that appears once memory has been reused. */
    unsigned char *z = calloc(256, 1);
    int zeroed = 1;
    for (int i = 0; i < 256; i++)
        if (z[i] != 0)
            zeroed = 0;
    check(zeroed, "calloc zeroed its memory");

    /* A hundred small allocations, freed in the reverse order: enough to make an
     * allocator that never coalesces run out of the first region. */
    void *many[100];
    for (int i = 0; i < 100; i++) {
        many[i] = malloc(64);
        memset(many[i], i, 64);
    }
    for (int i = 99; i >= 0; i--)
        free(many[i]);

    free(p);
    free(z);

    size_t after_bytes, after_blocks;
    staros_heap_live(&after_bytes, &after_blocks);
    check(after_bytes == before_bytes, "every byte was returned");
    check(after_blocks == before_blocks, "every block was returned");
    printf("[hello-c] heap: 103 allocations, %lu bytes live at the end\n",
           (unsigned long)after_bytes);
}

/* Layer 3: time. Two readings and a sleep between them. */
static void check_time(void)
{
    struct timespec a, b, nap;
    check(clock_gettime(0, &a) == 0, "clock_gettime");
    nap.tv_sec = 0;
    nap.tv_nsec = 20 * 1000 * 1000; /* 20 ms */
    check(nanosleep(&nap, 0) == 0, "nanosleep");
    check(clock_gettime(0, &b) == 0, "clock_gettime again");

    long long elapsed = (long long)(b.tv_sec - a.tv_sec) * 1000000000LL
                        + (b.tv_nsec - a.tv_nsec);
    /* Only the lower bound is checked: the upper one measures the whole
     * park/wake/schedule round trip, which on a loaded machine says nothing about
     * the clock. */
    check(elapsed >= 20 * 1000 * 1000, "the sleep lasted at least its deadline");
    printf("[hello-c] clock: %lld ns across a 20 ms nanosleep\n", elapsed);
}

/* Layer 4: files, which are IPC to the file server and nothing else. */
static void check_files(void)
{
    const char *path = "greeting.txt";
    int fd = open(path, 0, 0);
    if (fd < 0) {
        puts("[hello-c] no file server on this machine; skipping the file checks");
        return;
    }

    long size = lseek(fd, 0, SEEK_END);
    check(size > 0, "lseek to the end reports a size");
    check(lseek(fd, 0, SEEK_SET) == 0, "lseek back to the start");

    char text[64];
    ssize_t got = read(fd, text, sizeof text - 1);
    check(got == size, "read returned the whole file");
    text[got > 0 ? got : 0] = 0;

    /* Read the tail again from an offset, and check it against what the first read
     * produced. A file server that ignored the offset would return the head twice
     * and this comparison is what notices. */
    check(lseek(fd, 6, SEEK_SET) == 6, "lseek to an offset");
    char tail[64];
    ssize_t tail_got = read(fd, tail, sizeof tail - 1);
    check(tail_got == size - 6, "the offset shortened the read");
    check(memcmp(tail, text + 6, (size_t)tail_got) == 0, "the offset returned the tail");

    check(close(fd) == 0, "close");
    check(open("no-such-file", 0, 0) < 0, "opening a missing file fails");

    printf("[hello-c] read '%s' through fssrv with libc's open/read/lseek: %s",
           path, text);
}

int main(void)
{
    puts("[hello-c] a C program in EL0: printf, malloc, clock and files, no syscall in sight");

    check_format();
    check_heap();
    check_time();
    check_files();

    if (failures == 0)
        puts("[hello-c] C RUNTIME OK - every check passed");
    else
        printf("[hello-c] C RUNTIME BROKEN - %d checks failed\n", failures);
    return failures;
}
