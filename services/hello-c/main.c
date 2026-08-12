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

int main(void)
{
    puts("[hello-c] a C program in EL0: printf, malloc, clock and files, no syscall in sight");

    check_format();
    check_heap();
    check_time();
    check_files();
    check_threads();

    if (failures == 0)
        puts("[hello-c] C RUNTIME OK - every check passed");
    else
        printf("[hello-c] C RUNTIME BROKEN - %d checks failed\n", failures);
    return failures;
}
