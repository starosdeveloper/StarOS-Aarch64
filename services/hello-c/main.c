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

/* Threads. The opaque types are 32 bytes each and all-zero is a valid initialised
 * state, so a static mutex or condition variable needs no constructor — which is
 * what PTHREAD_MUTEX_INITIALIZER means and what the library was built to match. */
typedef unsigned long pthread_t;
typedef struct { long _opaque[4]; } pthread_mutex_t;
typedef struct { long _opaque[4]; } pthread_cond_t;
typedef struct { long _opaque[4]; } pthread_attr_t;
int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*start)(void *), void *arg);
int pthread_join(pthread_t thread, void **retval);
pthread_t pthread_self(void);
int pthread_mutex_lock(pthread_mutex_t *m);
int pthread_mutex_unlock(pthread_mutex_t *m);
int pthread_cond_wait(pthread_cond_t *c, pthread_mutex_t *m);
int pthread_cond_broadcast(pthread_cond_t *c);
int pthread_key_create(int *key, void *dtor);
int pthread_setspecific(int key, const void *value);
void *pthread_getspecific(int key);
int pthread_once(unsigned *once, void (*routine)(void));
int sched_yield(void);
unsigned long staros_thread_pointer(void);
unsigned long staros_threads_live(void);

/* Waitable descriptors: what Qt's event loop is built on. */
struct pollfd {
    int fd;
    short events;
    short revents;
};
#define POLLIN 0x001
#define POLLOUT 0x004
int poll(struct pollfd *fds, unsigned long nfds, int timeout_ms);
int eventfd(int initial, int flags);
int eventfd_read(int fd, unsigned long *value);
int eventfd_write(int fd, unsigned long value);
int pipe(int fds[2]);
ssize_t write(int fd, const void *buf, size_t count);

/* The mathematics. Qt reaches these through every transform and every gradient;
 * this program reaches them directly so that a failure names the function. */
double sqrt(double x);
double cbrt(double x);
double exp(double x);
double log(double x);
double log2(double x);
double pow(double x, double y);
double sin(double x);
double cos(double x);
double atan2(double y, double x);
double fmod(double x, double y);
double hypot(double x, double y);
double floor(double x);
double fabs(double x);
void sincos(double x, double *sine, double *cosine);
float sqrtf(float x);

/* Parsing, the mirror of printf. */
int sscanf(const char *input, const char *format, ...);
double strtod(const char *s, char **end);
double atof(const char *s);

/* Text, the parts that were missing until the contract named them. */
char *strncat(char *dst, const char *src, size_t n);
char *strtok_r(char *s, const char *delim, char **save);
void *memmem(const void *haystack, size_t hn, const void *needle, size_t nn);
char *strerror(int code);

/* One locale, named "C". */
char *setlocale(int category, const char *locale);
char *nl_langinfo(int item);
#define LC_ALL 6
#define CODESET 14

/* Pages rather than bytes. */
void *mmap(void *addr, size_t length, int prot, int flags, int fd, long offset);
int munmap(void *addr, size_t length);
int mprotect(void *addr, size_t length, int prot);
size_t staros_mmap_retained(void);
#define PROT_READ 1
#define PROT_WRITE 2
#define PROT_EXEC 4
#define MAP_PRIVATE 2
#define MAP_ANONYMOUS 0x20
#define MAP_FAILED ((void *)-1)

/* The calendar. */
struct tm {
    int tm_sec, tm_min, tm_hour, tm_mday, tm_mon, tm_year, tm_wday, tm_yday, tm_isdst;
    long tm_gmtoff;
    const char *tm_zone;
};
struct tm *gmtime_r(const long *when, struct tm *out);
long mktime(struct tm *broken_down);
size_t strftime(char *s, size_t n, const char *format, const struct tm *broken_down);
void tzset(void);
extern char *tzname[2];

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

/* Layer 1, second half: the mathematics.
 *
 * The checks are the ones a wrong implementation fails and a right one cannot: the
 * exact results (integer powers, remainders, powers of two), the identities that
 * hold at every argument, and the three cases that are hard on purpose — an
 * argument too large for naive reduction, an exponent that multiplies the
 * logarithm's error, and a magnitude that overflows the obvious spelling of hypot. */
static void near(double got, double want, double tol, const char *what)
{
    double err = fabs(got - want);
    if (fabs(want) > 1e-12)
        err = err / fabs(want);
    check(err <= tol, what);
}

static void check_math(void)
{
    /* Exact, not close. A library that routes these through a logarithm returns
     * 99.99999999999999 and every layout built on it drifts. */
    check(pow(10.0, 2.0) == 100.0, "pow of an integer exponent is exact");
    check(pow(2.0, 10.0) == 1024.0, "pow of a power of two is exact");
    check(sqrt(64.0) == 8.0, "sqrt of a perfect square is exact");
    check(log2(4096.0) == 12.0, "log2 of a power of two is exact");
    check(fmod(7.0, 3.0) == 1.0, "fmod is exact");
    check(floor(-2.5) == -3.0 && floor(2.5) == 2.0, "floor rounds toward minus infinity");

    /* Identities: true for every argument, so they catch a coefficient that is
     * wrong in the last digits as well as one that is wrong entirely. */
    for (int i = -60; i <= 60; i++) {
        double x = (double)i * 0.37;
        double s, c;
        sincos(x, &s, &c);
        near(s * s + c * c, 1.0, 1e-14, "sin^2 + cos^2 == 1");
        near(exp(log(fabs(x) + 1.0)), fabs(x) + 1.0, 1e-14, "exp(log(x)) == x");
        near(cbrt(x * x * x), x, 1e-14, "cbrt(x^3) == x");
        /* atan2 must undo sincos, up to the turn the angle wrapped through. The
         * guard skips angles that land on the ±pi seam, where the two spellings
         * legitimately disagree by a full turn. */
        double turns = floor((x + 3.141592653589793) / 6.283185307179586);
        double wrapped = x - 6.283185307179586 * turns;
        if (fabs(fabs(wrapped) - 3.141592653589793) > 1e-6)
            near(atan2(s, c), wrapped, 1e-13, "atan2(sin, cos) recovers the angle");
    }

    /* The hard three. */
    /* The two constants are what the host's glibc returns for these arguments,
     * checked rather than remembered. A naive Cody-Waite reduction gets the first
     * one right to nine digits; a single-double logarithm gets the second one
     * right to nine. Both tolerances below are tighter than that. */
    near(sin(1e15), 0.85827279317023586, 1e-13, "sin reduces an argument of 1e15");
    near(pow(1.0000001, 1e7), 2.71828169413208176, 1e-12, "pow keeps its digits when y is large");
    /* The other half of the same problem: here the logarithm is large rather than
     * the exponent, so the error being multiplied is the one in `k*ln2` rather than
     * the one in the series. A pow that carries only one of the two comes back with
     * eleven digits and passes the check above. */
    /* The other half of the same problem: here the logarithm is large rather than
     * the exponent, so what gets multiplied is the rounding in `k*ln2 + ln(m)`
     * rather than the one in the series. Dropping that word alone still passes the
     * check above and fails this one, by 1.4e-14. */
    near(pow(3.0, 100.0), 5.15377520732011324e47, 1e-14, "pow keeps its digits when the base is far from one");
    near(hypot(3e300, 4e300), 5e300, 1e-14, "hypot does not overflow");

    /* The float entry points exist and are not the double ones under another name. */
    check(sqrtf(2.25f) == 1.5f, "sqrtf");

    printf("[hello-c] math: sin(1e15)=%.6f, pow(1.0000001,1e7)=%.6f, hypot(3,4)=%.1f\n",
           sin(1e15), pow(1.0000001, 1e7), hypot(3.0, 4.0));
}

/* Layer 1, third part: reading text back. printf has been checked since the first
 * C program ran here; nothing checked the inverse until the contract named
 * __isoc23_sscanf, which is what a C23 compiler turns a call to sscanf into. */
static void check_scan(void)
{
    int day = 0, month = 0, year = 0;
    check(sscanf("2025-08-13", "%d-%d-%d", &year, &month, &day) == 3, "sscanf assigned three");
    check(year == 2025 && month == 8 && day == 13, "sscanf parsed the date");

    /* A width splits digits no separator splits, and %n reports the position. */
    int a = 0, b = 0, consumed = 0;
    check(sscanf("20260813", "%4d%2d%n", &a, &b, &consumed) == 2, "widths split a digit run");
    check(a == 2026 && b == 8, "the widths took the right digits");
    check(consumed == 6, "%n reported six bytes consumed");

    /* Failure has two shapes and C distinguishes them: nothing to read is EOF,
     * something unreadable is zero. A loop that treats them alike either spins or
     * stops early. */
    check(sscanf("", "%d", &a) == -1, "sscanf on empty input is EOF");
    check(sscanf("abc", "%d", &a) == 0, "sscanf on unmatchable input is zero");

    char word[16];
    check(sscanf("  hello world", "%s", word) == 1, "%s");
    check(strcmp(word, "hello") == 0, "%s stopped at the space");

    double d = 0;
    char *end = 0;
    d = strtod("  -12.5e2rest", &end);
    check(d == -1250.0, "strtod value");
    check(end != 0 && strcmp(end, "rest") == 0, "strtod reported where it stopped");
    /* The literal on the right is the compiler's own: this is the claim that the
     * parser and the compiler agree bit for bit. */
    check(strtod("0.1", 0) == 0.1, "strtod of 0.1 is the same double the compiler makes");
    check(atof("3.5") == 3.5, "atof");
}

/* The text functions the contract asked for after the first pass. */
static void check_text(void)
{
    char buf[32];
    strcpy(buf, "qml");
    strncat(buf, "-runtime-and-more", 8);
    check(strcmp(buf, "qml-runtime") == 0, "strncat took exactly n bytes and terminated");

    char line[] = "one,two,,three";
    char *save = 0;
    char *tok = strtok_r(line, ",", &save);
    check(tok && strcmp(tok, "one") == 0, "strtok_r first token");
    tok = strtok_r(0, ",", &save);
    check(tok && strcmp(tok, "two") == 0, "strtok_r second token");
    /* A run of delimiters is one separator, so the empty field disappears — which
     * is what C says and what surprises everyone once. */
    tok = strtok_r(0, ",", &save);
    check(tok && strcmp(tok, "three") == 0, "strtok_r skipped the empty field");
    check(strtok_r(0, ",", &save) == 0, "strtok_r ended");

    const char *hay = "the needle is here";
    check(memmem(hay, 18, "needle", 6) == hay + 4, "memmem found the needle");
    check(memmem(hay, 18, "thread", 6) == 0, "memmem reports a miss as null");

    check(strcmp(strerror(2), "No such file or directory") == 0, "strerror names a real code");

    /* One locale, and the refusal is the point: a program told "C" when it asked
     * for German formats numbers wrongly with no way to find out. */
    check(strcmp(setlocale(LC_ALL, "C"), "C") == 0, "setlocale accepts C");
    check(setlocale(LC_ALL, "de_DE.UTF-8") == 0, "setlocale refuses a locale we do not have");
    check(strcmp(nl_langinfo(CODESET), "UTF-8") == 0, "nl_langinfo names the encoding");
}

/* Layer 2's other half: pages rather than bytes. */
static void check_mmap(void)
{
    size_t len = 3 * 4096 + 17; /* deliberately not a whole number of pages */
    char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(p != MAP_FAILED, "mmap of anonymous memory");
    /* Touch both ends: a mapping one page short would fault on the second write,
     * and a rounding error in the page count is otherwise invisible. */
    p[0] = 'a';
    p[len - 1] = 'z';
    check(p[0] == 'a' && p[len - 1] == 'z', "the whole mapping is writable");

    /* The two refusals, which are the honest part of this layer. */
    check(mmap(0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED,
          "mmap refuses to hand out an executable page");
    check(mprotect(p, 4096, PROT_READ | PROT_EXEC) == -1, "mprotect refuses PROT_EXEC");
    check(mmap(0, 4096, PROT_READ, MAP_PRIVATE, 3, 0) == MAP_FAILED, "mmap refuses a file mapping");

    size_t before = staros_mmap_retained();
    check(munmap(p, len) == 0, "munmap");
    /* And the price of a kernel with no unmap, counted rather than hidden. */
    check(staros_mmap_retained() == before + 4 * 4096, "munmap accounted for the pages it kept");

    printf("[hello-c] mmap: %lu bytes mapped and returned, %lu retained by the kernel\n",
           (unsigned long)len, (unsigned long)staros_mmap_retained());
}

/* Layer 3's other half: the calendar. */
static void check_calendar(void)
{
    tzset();
    check(tzname[0] && strcmp(tzname[0], "UTC") == 0, "the only time zone says what it is");

    /* A date with a known answer, and the two rules a hand-rolled calendar gets
     * wrong: 2000 was a leap year and 2100 is not. */
    struct tm t;
    long when = 1755043200L; /* 2025-08-13 00:00:00 UTC, a Wednesday */
    check(gmtime_r(&when, &t) != 0, "gmtime_r");
    check(t.tm_year == 125 && t.tm_mon == 7 && t.tm_mday == 13, "gmtime_r split the date");
    check(t.tm_wday == 3, "gmtime_r knew the weekday");
    check(t.tm_yday == 224, "gmtime_r counted the day of the year");
    check(mktime(&t) == when, "mktime is the inverse of gmtime_r");

    long leap = 951782400L; /* 2000-02-29 */
    check(gmtime_r(&leap, &t) != 0, "gmtime_r again");
    check(t.tm_mon == 1 && t.tm_mday == 29, "2000 had a 29 February");
    long century = 4107542400L; /* 2100-03-01 */
    check(gmtime_r(&century, &t) != 0, "gmtime_r once more");
    check(t.tm_mon == 2 && t.tm_mday == 1, "2100 did not");

    /* mktime normalises, which is how C programs do date arithmetic. */
    struct tm plus = {0};
    plus.tm_year = 125;
    plus.tm_mon = 0;
    plus.tm_mday = 45;
    mktime(&plus);
    check(plus.tm_mon == 1 && plus.tm_mday == 14, "day 45 of January became 14 February");

    char stamp[64];
    gmtime_r(&when, &t);
    size_t n = strftime(stamp, sizeof stamp, "%Y-%m-%d %H:%M:%S %Z (%a)", &t);
    check(n == strlen(stamp), "strftime returned the length it wrote");
    check(strcmp(stamp, "2025-08-13 00:00:00 UTC (Wed)") == 0, "strftime formatted the date");
    /* Too small a buffer must refuse rather than truncate: a truncated timestamp
     * looks like a real one. */
    char tiny[8];
    check(strftime(tiny, sizeof tiny, "%Y-%m-%d", &t) == 0, "strftime refused a short buffer");
    check(tiny[0] == 0, "and left nothing behind to be printed");

    printf("[hello-c] calendar: %s\n", stamp);
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

/* Layer 5: threads, locks and thread-local storage.
 *
 * Two thread-locals with different homes: one initialised (it lives in .tdata and
 * must be *copied* into every thread's block) and one not (.tbss, which must be
 * zeroed). A runtime that mapped one block for everybody passes nothing here — the
 * seeds would collide — and one that forgot the .tdata copy would give every thread
 * a zero where 0xAB belongs. */
static _Thread_local int tls_seed = 0xAB;
static _Thread_local int tls_scratch;

#define WORKERS 4
/* Enough contention to be a test, few enough to be a test that *finishes*: every
 * iteration yields inside the critical section and every contended acquisition
 * costs a park and a wake, so this is thousands of syscalls per worker on a machine
 * emulating four cores. Raising it does not make the checks stronger; it only makes
 * the smoke matrix slower. */
#define BUMPS 250

static pthread_mutex_t counter_lock;
static pthread_cond_t go_signal;
static pthread_mutex_t go_lock;
static int go;
static long shared_counter;
/* How many threads are inside the critical section, and whether that was ever
 * more than one. */
static int inside;
static int exclusion_violated;
static unsigned long worker_tp[WORKERS];
static int worker_seed_ok[WORKERS];
static int worker_alloc_ok[WORKERS] = { 1, 1, 1, 1 };
static int specific_key;
static unsigned once_state;
static int once_count;

static void run_once(void)
{
    once_count++;
}

static void *worker(void *arg)
{
    long id = (long)arg;

    /* Our own copies: the seed arrived from the template, the scratch was zeroed. */
    int seed_ok = (tls_seed == 0xAB) && (tls_scratch == 0);
    tls_seed = (int)(0x100 + id);
    tls_scratch = (int)(id * 7);
    worker_tp[id] = staros_thread_pointer();

    pthread_once(&once_state, run_once);
    pthread_setspecific(specific_key, (void *)(id + 1));

    /* Wait for main to release everybody, so the increments below actually
     * contend rather than running one thread at a time. */
    pthread_mutex_lock(&go_lock);
    while (!go)
        pthread_cond_wait(&go_signal, &go_lock);
    pthread_mutex_unlock(&go_lock);

    /* The critical section is deliberately awkward: read, *yield*, write. A plain
     * `shared_counter++` is three instructions and a preempting scheduler almost
     * never lands inside it — a lock that does nothing at all passes that test,
     * which was checked before this comment was written. Yielding in the middle
     * makes interleaving certain, so the count and the occupancy flag below can
     * only come out right if the lock actually excludes. */
    for (int i = 0; i < BUMPS; i++) {
        pthread_mutex_lock(&counter_lock);
        inside++;
        long seen = shared_counter;
        sched_yield();
        if (inside != 1)
            exclusion_violated = 1;
        shared_counter = seen + 1;
        inside--;
        pthread_mutex_unlock(&counter_lock);
    }

    /* Hammer the library's own statics from every thread at once: the heap and the
     * console are process-wide, and before layer 5 they had no locks at all. Each
     * block is filled with a byte only this thread writes, so an allocator that
     * hands the same block to two threads is caught by the check rather than by a
     * crash somewhere later. */
    for (int i = 0; i < 64; i++) {
        unsigned char *p = malloc(96);
        if (!p) {
            worker_alloc_ok[id] = 0;
            break;
        }
        memset(p, (int)(0x40 + id), 96);
        sched_yield();
        for (int j = 0; j < 96; j++)
            if (p[j] != (unsigned char)(0x40 + id))
                worker_alloc_ok[id] = 0;
        free(p);
    }

    /* After all that contention, our thread-locals must still be ours. */
    seed_ok = seed_ok && tls_seed == (int)(0x100 + id) && tls_scratch == (int)(id * 7);
    seed_ok = seed_ok && (long)pthread_getspecific(specific_key) == id + 1;
    worker_seed_ok[id] = seed_ok;
    return (void *)(id * 11);
}

static void check_threads(void)
{
    pthread_t threads[WORKERS];

    check(tls_seed == 0xAB, "main's thread-local came from the .tdata template");
    check(tls_scratch == 0, "main's .tbss thread-local starts zeroed");
    tls_seed = 0x1234; /* if the workers share our block, this leaks into them */

    check(pthread_key_create(&specific_key, 0) == 0, "pthread_key_create");

    for (long i = 0; i < WORKERS; i++)
        check(pthread_create(&threads[i], 0, worker, (void *)i) == 0, "pthread_create");

    /* Release them together. */
    pthread_mutex_lock(&go_lock);
    go = 1;
    pthread_cond_broadcast(&go_signal);
    pthread_mutex_unlock(&go_lock);

    for (int i = 0; i < WORKERS; i++) {
        void *retval = 0;
        check(pthread_join(threads[i], &retval) == 0, "pthread_join");
        check((long)retval == (long)i * 11, "the thread's return value survived the join");
        check(worker_seed_ok[i], "each thread kept its own thread-locals");
        check(worker_alloc_ok[i], "malloc from four threads at once handed out distinct memory");
    }

    check(shared_counter == (long)WORKERS * BUMPS, "the mutex serialised every increment");
    check(!exclusion_violated, "no two threads were inside the critical section at once");
    check(tls_seed == 0x1234, "main's thread-local was not touched by any worker");
    check(once_count == 1, "pthread_once ran the routine exactly once");

    /* Every thread pointer must be distinct — this is the check that fails if the
     * kernel stops switching TPIDR_EL0, and the one the roadmap names. */
    int distinct = 1;
    for (int i = 0; i < WORKERS; i++) {
        if (worker_tp[i] == 0 || worker_tp[i] == staros_thread_pointer())
            distinct = 0;
        for (int j = i + 1; j < WORKERS; j++)
            if (worker_tp[i] == worker_tp[j])
                distinct = 0;
    }
    check(distinct, "every thread ran on its own thread pointer");

    printf("[hello-c] threads: %d workers x %d increments = %ld, %lu thread(s) live at the end\n",
           WORKERS, BUMPS, shared_counter, staros_threads_live());
}

/* Layer 6: descriptors you can wait on.
 *
 * This is the shape of an event loop — block in poll until something happens, with
 * a deadline — so the checks are about *waiting* rather than about data: that a
 * timeout is really honoured, that a poll blocked until another thread acted, and
 * that a descriptor stops being ready once its event is consumed. */
static int event_fd;
static int pipe_fds[2];
static int pipe_message_ok;

static void *io_worker(void *arg)
{
    (void)arg;
    char buf[16];
    struct pollfd wait_read = { pipe_fds[0], POLLIN, 0 };

    /* Block until main writes. If poll returned early or spuriously, the read
     * below would come back short and the check would fail. */
    if (poll(&wait_read, 1, 5000) == 1 && (wait_read.revents & POLLIN)) {
        ssize_t got = read(pipe_fds[0], buf, sizeof buf);
        pipe_message_ok = (got == 4) && (memcmp(buf, "ping", 4) == 0);
    }
    /* Tell main we are done, the way a worker thread signals an event loop. */
    eventfd_write(event_fd, 1);
    return 0;
}

static void check_poll(void)
{
    struct timespec before, after;
    pthread_t worker_id;

    event_fd = eventfd(0, 0);
    check(event_fd >= 0, "eventfd");
    check(pipe(pipe_fds) == 0, "pipe");

    struct pollfd watch_event = { event_fd, POLLIN, 0 };

    /* Nothing has happened yet: a zero timeout must report nothing ready and must
     * not block. */
    check(poll(&watch_event, 1, 0) == 0, "poll with a zero timeout reports nothing");

    /* A timeout must be *waited out*. Measured, because a poll that returns
     * immediately reports the same zero. */
    clock_gettime(0, &before);
    check(poll(&watch_event, 1, 20) == 0, "poll timed out with nothing ready");
    clock_gettime(0, &after);
    long long waited = (long long)(after.tv_sec - before.tv_sec) * 1000000000LL
                       + (after.tv_nsec - before.tv_nsec);
    check(waited >= 20 * 1000 * 1000, "poll actually waited for its timeout");

    check(pthread_create(&worker_id, 0, io_worker, 0) == 0, "pthread_create for the io worker");
    check(write(pipe_fds[1], "ping", 4) == 4, "write to the pipe");

    /* Block until the worker signals. Nothing here spins: the wake comes from the
     * other thread's eventfd_write. */
    check(poll(&watch_event, 1, 5000) == 1, "poll woke on another thread's eventfd");
    check(watch_event.revents & POLLIN, "poll reported the eventfd readable");

    unsigned long value = 0;
    check(eventfd_read(event_fd, &value) == 0, "eventfd_read");
    check(value == 1, "the eventfd carried the value written");
    /* Consumed: the descriptor must stop being ready. A readiness bit that is
     * remembered rather than recomputed is how an event loop comes to spin. */
    check(poll(&watch_event, 1, 0) == 0, "the eventfd is not ready once it is read");

    /* The counter accumulates until read, which is the whole difference between an
     * eventfd and a flag. */
    eventfd_write(event_fd, 2);
    eventfd_write(event_fd, 3);
    check(eventfd_read(event_fd, &value) == 0, "eventfd_read again");
    check(value == 5, "the eventfd summed the writes it had not delivered");

    check(pthread_join(worker_id, 0) == 0, "join the io worker");
    check(pipe_message_ok, "the worker read exactly what the pipe was given");

    close(event_fd);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
    printf("[hello-c] poll: a thread slept on an eventfd and a pipe, and a %d ms timeout took %lld ns\n",
           20, waited);
}

int main(void)
{
    puts("[hello-c] a C program in EL0: printf, malloc, clock and files, no syscall in sight");

    check_format();
    check_math();
    check_scan();
    check_text();
    check_mmap();
    check_calendar();
    check_heap();
    check_time();
    check_files();
    check_threads();
    check_poll();

    if (failures == 0)
        puts("[hello-c] C RUNTIME OK - every check passed");
    else
        printf("[hello-c] C RUNTIME BROKEN - %d checks failed\n", failures);
    return failures;
}
