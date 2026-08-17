// The C++ runtime: the part of libstdc++ that is *not* in the headers.
//
// A C++ standard library is two halves. The templates — `std::vector`,
// `std::string`, most of `<algorithm>` — are header-only and compile straight out
// of the host's libstdc++ headers against this system's own C headers. The other
// half is compiled code that normally comes from `libstdc++.a`, built for the host
// and useless here, so it is written out below: memory operators, the ABI's
// `__cxa_*` hooks, the throw helpers that a `-fno-exceptions` build still calls,
// and the three out-of-line functions behind `std::thread`.
//
// That list is not a guess. It is what the linker asked for, one symbol at a time,
// and nothing here exists for any other reason. When Qt arrives it will ask for
// more; the same loop applies, and every addition is a decision recorded in
// docs/LIBC-CONTRACT.md rather than a stub returning zero.
//
// Built with `-fno-exceptions`, which is what the roadmap chose for the whole C++
// side (Qt supports it as `QT_NO_EXCEPTIONS`). That choice is what keeps an
// unwinder, `.eh_frame` and `__cxa_throw` out of this file: a program that would
// have thrown calls one of the `__throw_*` helpers below, which report and stop
// instead of unwinding into a caller that has no idea how to unwind.
//
// It was `-fno-exceptions -fno-rtti` until Qt Quick, and the second half did not
// survive: the software scene graph identifies every node it draws with a chain of
// `dynamic_cast`s and has no other way to. So `__dynamic_cast` is implemented here,
// near the bottom of this file, and every C++ translation unit for this target is
// compiled with RTTI. Exceptions are still genuinely absent.

#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <filesystem>
#include <list>
#include <map>
#include <memory_resource>
#include <new>
#include <string>
#include <thread>
#include <unordered_map>

#include <bits/atomic_futex.h>
// For the `__cxxabiv1::__*_type_info` classes, whose vtables this file has to emit.
#include <cxxabi.h>

#include <pthread.h>
#include <sched.h>
#include <time.h>

// `std::string`'s out-of-line members. libstdc++'s header declares them
// `extern template`, meaning "somebody else compiled these" — normally
// libstdc++.a, which does not exist for this target. One explicit instantiation
// here is that somebody: it emits `reserve`, `_M_create`, `_M_append` and the rest
// into this object file, and it is why a `std::string` that outgrows its small
// buffer links at all.
template class std::__cxx11::basic_string<char>;

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------
//
// `operator new` here returns null on failure rather than throwing, because there
// is nothing to throw with. The standard says it must not return null unless it is
// the `nothrow` form — this is a documented divergence, and the alternative
// (stopping the program on a failed allocation) would make a recoverable condition
// fatal.

void *operator new(std::size_t size) {
    void *p = std::malloc(size ? size : 1);
    return p;
}

void *operator new[](std::size_t size) { return ::operator new(size); }

void *operator new(std::size_t size, const std::nothrow_t &) noexcept {
    return ::operator new(size);
}

void *operator new[](std::size_t size, const std::nothrow_t &) noexcept {
    return ::operator new(size);
}

void *operator new(std::size_t size, std::align_val_t align) {
    return aligned_alloc(static_cast<std::size_t>(align), size ? size : 1);
}

void *operator new[](std::size_t size, std::align_val_t align) {
    return ::operator new(size, align);
}

// The aligned nothrow forms. Identical here to the throwing ones, because with
// `-fno-exceptions` neither throws — `aligned_alloc` returns null and so does this.
// They exist as separate symbols because the *caller* chose the nothrow spelling,
// and a program that wrote `new (std::nothrow)` and got a link error would be told
// its own syntax is unsupported.
void *operator new(std::size_t size, std::align_val_t align,
                   const std::nothrow_t &) noexcept {
    return ::operator new(size, align);
}

void *operator new[](std::size_t size, std::align_val_t align,
                     const std::nothrow_t &) noexcept {
    return ::operator new(size, align);
}

namespace std {
// The tag object itself. One byte of data, and libstdc++ leaves it to the library
// rather than the header — so a program writing `new (std::nothrow)` needs this to
// exist somewhere, and this is somewhere.
const nothrow_t nothrow = nothrow_t{};
}  // namespace std

void operator delete(void *p) noexcept { std::free(p); }
void operator delete[](void *p) noexcept { std::free(p); }
// The *sized* deletes are what a modern compiler emits; both forms end in the same
// place because this allocator keeps the size in its own header.
void operator delete(void *p, std::size_t) noexcept { std::free(p); }
void operator delete[](void *p, std::size_t) noexcept { std::free(p); }
void operator delete(void *p, std::align_val_t) noexcept { std::free(p); }
void operator delete[](void *p, std::align_val_t) noexcept { std::free(p); }
void operator delete(void *p, std::size_t, std::align_val_t) noexcept { std::free(p); }
void operator delete[](void *p, std::size_t, std::align_val_t) noexcept { std::free(p); }
void operator delete(void *p, const std::nothrow_t &) noexcept { std::free(p); }
void operator delete[](void *p, const std::nothrow_t &) noexcept { std::free(p); }

// ---------------------------------------------------------------------------
// The Itanium C++ ABI hooks
// ---------------------------------------------------------------------------

// `__cxa_atexit` is deliberately **not** here: the table of destructors belongs to
// the C library, which owns `exit` and therefore owns when they run. Putting it in
// this file would mean the C runtime had to call into the C++ one — a link-time
// dependency in the wrong direction, solvable only with weak symbols, for a table
// that is not a C++ idea in the first place. See `atexit` in `crates/staros-libc`.

extern "C" {

/// A pure virtual call: the object is under construction or destruction and the
/// override does not exist yet. There is no recovery.
void __cxa_pure_virtual(void) {
    std::fputs("[c++] pure virtual function called\n", stderr);
    std::abort();
}

void __cxa_deleted_virtual(void) {
    std::fputs("[c++] deleted virtual function called\n", stderr);
    std::abort();
}

// Guards for function-local statics. The guard variable's first byte is "already
// constructed"; the second is "construction in progress", which is what makes the
// racing thread wait rather than run the constructor twice.
//
// The waiting is a yield loop rather than a futex: construction of a static local
// is short, and a second lock in the C++ runtime would need the thread layer,
// which needs the allocator, which needs this.
//
// The parameter is `__cxxabiv1::__guard*` rather than the `unsigned char*` this was
// first written with. The two behave identically — the code below only ever touches
// the first two bytes, which is what the generic Itanium ABI specifies — but the
// declared type has to match `<cxxabi.h>`, which this file now includes for the
// `type_info` classes further down. It is a 64-bit integer here; `bytes_of` is where
// the reinterpretation happens, once, instead of at every use.
static unsigned char *bytes_of(__cxxabiv1::__guard *guard) {
    return reinterpret_cast<unsigned char *>(guard);
}

int __cxa_guard_acquire(__cxxabiv1::__guard *g) {
    unsigned char *guard = bytes_of(g);
    if (__atomic_load_n(&guard[0], __ATOMIC_ACQUIRE) != 0) {
        return 0;  // already constructed
    }
    unsigned char expected = 0;
    while (!__atomic_compare_exchange_n(&guard[1], &expected, 1, false, __ATOMIC_ACQ_REL,
                                        __ATOMIC_ACQUIRE)) {
        expected = 0;
        if (__atomic_load_n(&guard[0], __ATOMIC_ACQUIRE) != 0) {
            return 0;  // somebody else finished it while we waited
        }
        sched_yield();
    }
    if (__atomic_load_n(&guard[0], __ATOMIC_ACQUIRE) != 0) {
        __atomic_store_n(&guard[1], 0, __ATOMIC_RELEASE);
        return 0;
    }
    return 1;  // we own the construction
}

void __cxa_guard_release(__cxxabiv1::__guard *g) noexcept {
    unsigned char *guard = bytes_of(g);
    __atomic_store_n(&guard[0], 1, __ATOMIC_RELEASE);
    __atomic_store_n(&guard[1], 0, __ATOMIC_RELEASE);
}

void __cxa_guard_abort(__cxxabiv1::__guard *g) noexcept {
    __atomic_store_n(&bytes_of(g)[1], 0, __ATOMIC_RELEASE);
}

}  // extern "C"

// ---------------------------------------------------------------------------
// What a `-fno-exceptions` build calls instead of throwing
// ---------------------------------------------------------------------------
//
// libstdc++'s headers call these when they would have thrown. Each one reports
// *which* invariant broke and stops. Reporting matters more than usual here: with
// exceptions off there is no type, no message and no stack — the name of the
// function is all the evidence a crash leaves behind.

namespace {
[[noreturn]] void fail(const char *what) {
    std::fprintf(stderr, "[c++] %s\n", what);
    std::abort();
}
}  // namespace

namespace std {

void __throw_bad_alloc() { fail("bad_alloc: out of memory"); }
void __throw_bad_array_new_length() { fail("bad_array_new_length"); }
void __throw_bad_cast() { fail("bad_cast"); }
void __throw_bad_function_call() { fail("bad_function_call: empty std::function"); }
void __throw_bad_typeid() { fail("bad_typeid"); }
void __throw_length_error(const char *what) { fail(what); }
void __throw_logic_error(const char *what) { fail(what); }
void __throw_domain_error(const char *what) { fail(what); }
void __throw_invalid_argument(const char *what) { fail(what); }
void __throw_out_of_range(const char *what) { fail(what); }
void __throw_out_of_range_fmt(const char *what, ...) { fail(what); }
void __throw_runtime_error(const char *what) { fail(what); }
void __throw_range_error(const char *what) { fail(what); }
void __throw_overflow_error(const char *what) { fail(what); }
void __throw_underflow_error(const char *what) { fail(what); }
void __throw_ios_failure(const char *what) { fail(what); }
void __throw_ios_failure(const char *what, int) { fail(what); }
void __throw_system_error(int) { fail("system_error"); }
void __throw_future_error(int) { fail("future_error"); }
void __throw_bad_exception() { fail("bad_exception"); }

void terminate() noexcept { fail("terminate called"); }

/// libstdc++'s hardened assertions land here.
void __glibcxx_assert_fail(const char *file, int line, const char *function,
                           const char *condition) noexcept {
    std::fprintf(stderr, "[c++] assertion failed: %s in %s (%s:%d)\n", condition, function, file,
                 line);
    std::abort();
}

// -------------------------------------------------------------------------
// std::thread
// -------------------------------------------------------------------------
//
// Three functions and a destructor, which is all of `std::thread` that is not a
// template. They map onto this system's pthreads, which map onto `SpawnThread`.

thread::_State::~_State() = default;

void thread::_M_start_thread(_State_ptr state, void (*)()) {
    // The thread takes ownership of the state; releasing it here and deleting it in
    // the trampoline is what libstdc++ does, and it is the only arrangement that
    // survives `pthread_create` failing.
    _State *raw = state.release();
    pthread_t native = 0;
    int rc = pthread_create(
        &native, nullptr,
        [](void *argument) -> void * {
            _State_ptr owned(static_cast<_State *>(argument));
            owned->_M_run();
            return nullptr;
        },
        raw);
    if (rc != 0) {
        delete raw;
        fail("thread::_M_start_thread: the kernel refused another thread");
    }
    // `thread::id` has a constructor from a native handle, and it is private —
    // which is fine here, because this *is* a member of `std::thread`.
    _M_id = id(native);
}

void thread::join() {
    if (pthread_join(_M_id._M_thread, nullptr) != 0) {
        fail("thread::join: not joinable");
    }
    _M_id = id();
}

void thread::detach() {
    if (pthread_detach(_M_id._M_thread) != 0) {
        fail("thread::detach: not joinable");
    }
    _M_id = id();
}

unsigned int thread::hardware_concurrency() noexcept {
    // One, until the C library has something to say about the other cores. A
    // guessed number here sizes somebody's thread pool.
    return 1;
}

// ---------------------------------------------------------------------------
// The containers' out-of-line halves
// ---------------------------------------------------------------------------
//
// `std::map`, `std::set`, `std::list` and `std::unordered_map` are templates, but
// the parts of them that do not depend on the element type are compiled once and
// live in libstdc++.a: the red-black tree's rebalancing, the linked list's splice,
// the hash table's bucket growth. Qt uses all four, so `scripts/cxx-progress.sh`
// named them and they are written here.
//
// These are algorithms rather than policy — there is one correct red-black
// rebalance — so what follows is the standard formulation, and the tests that
// matter are the ones in `services/hello-cpp` that put a few hundred elements
// through a `std::map` and read them back in order. A rotation written the wrong
// way round still produces a tree; it produces one that is no longer balanced, and
// the symptom is a container that gets slower and never wrong.

// ---- std::list ------------------------------------------------------------
//
// A circular doubly-linked list with a sentinel node, which is why these three
// functions need no special case for an empty list or for the ends.

void __detail::_List_node_base::_M_hook(_List_node_base *const position) noexcept {
    _M_next = position;
    _M_prev = position->_M_prev;
    position->_M_prev->_M_next = this;
    position->_M_prev = this;
}

void __detail::_List_node_base::_M_unhook() noexcept {
    _List_node_base *const next_node = _M_next;
    _List_node_base *const prev_node = _M_prev;
    prev_node->_M_next = next_node;
    next_node->_M_prev = prev_node;
}

void __detail::_List_node_base::_M_reverse() noexcept {
    // Swap every node's two pointers, including the sentinel's. That is the whole
    // reverse: a circular list read the other way round *is* the reversed list, and
    // no node moves.
    //
    // Not in `scripts/cxx-progress.sh`'s list, because the distribution's Qt never
    // calls `std::list::reverse`. The linker asked for it the moment a test did —
    // which is the same loop the top of this file describes, and the reason the
    // measurement is a floor rather than a specification.
    _List_node_base *node = this;
    do {
        _List_node_base *const next_node = node->_M_next;
        node->_M_next = node->_M_prev;
        node->_M_prev = next_node;
        node = node->_M_prev;
    } while (node != this);
}

void __detail::_List_node_base::_M_transfer(_List_node_base *const first,
                                            _List_node_base *const last) noexcept {
    // Splice `[first, last)` in front of `this`. The self-splice is excluded
    // because it would corrupt the list rather than do nothing: the pointers are
    // rewritten in an order that assumes the two ranges are distinct.
    if (this == last) {
        return;
    }
    last->_M_prev->_M_next = this;
    first->_M_prev->_M_next = last;
    _M_prev->_M_next = first;

    _List_node_base *const old_prev = _M_prev;
    _M_prev = last->_M_prev;
    last->_M_prev = first->_M_prev;
    first->_M_prev = old_prev;
}

// ---- std::unordered_map ---------------------------------------------------
//
// The bucket count is always a prime, because the hash is reduced by `%` and a
// power-of-two modulus keeps only the low bits — which for a pointer hash are the
// alignment, and every entry then lands in one bucket. libstdc++ carries a table of
// 256 primes; this is a smaller table with the same growth ratio, and the honest
// difference is that a table above four million buckets stops growing by primes and
// doubles instead.

namespace {

const unsigned long kPrimes[] = {
    2UL,        3UL,         5UL,         7UL,         11UL,       13UL,
    17UL,       23UL,        29UL,        37UL,        47UL,       59UL,
    73UL,       97UL,        127UL,       151UL,       197UL,      251UL,
    313UL,      397UL,       499UL,       631UL,       797UL,      1009UL,
    1259UL,     1597UL,      2011UL,      2539UL,      3203UL,     4027UL,
    5087UL,     6421UL,      8089UL,      10193UL,     12853UL,    16193UL,
    20399UL,    25717UL,     32401UL,     40823UL,     51437UL,    64811UL,
    81649UL,    102877UL,    129607UL,    163307UL,    205759UL,   259229UL,
    326617UL,   411527UL,    518509UL,    653267UL,    823117UL,   1037059UL,
    1306601UL,  1646237UL,   2074129UL,   2613229UL,   3292489UL,  4148279UL,
};
const unsigned kPrimeCount = sizeof(kPrimes) / sizeof(kPrimes[0]);

}  // namespace

unsigned long __detail::_Prime_rehash_policy::_M_next_bkt(unsigned long n) const {
    for (unsigned i = 0; i < kPrimeCount; i++) {
        if (kPrimes[i] >= n) {
            _M_next_resize = static_cast<unsigned long>(
                static_cast<double>(kPrimes[i]) * static_cast<double>(_M_max_load_factor));
            return kPrimes[i];
        }
    }
    // Past the table: double, which keeps the growth ratio even though the modulus
    // stops being prime. A table this size is already pathological for a hash map
    // in this system, and refusing to grow would be worse than a poor modulus.
    unsigned long bkt = kPrimes[kPrimeCount - 1];
    while (bkt < n) {
        bkt *= 2;
    }
    _M_next_resize = static_cast<unsigned long>(static_cast<double>(bkt) *
                                                static_cast<double>(_M_max_load_factor));
    return bkt;
}

std::pair<bool, unsigned long> __detail::_Prime_rehash_policy::_M_need_rehash(
    unsigned long n_bkt, unsigned long n_elt, unsigned long n_ins) const {
    if (n_elt + n_ins >= _M_next_resize) {
        // Grow to at least twice the element count, so that the load factor after
        // the insert is roughly half — the same target libstdc++ aims at, and what
        // keeps the amortised insert constant rather than quadratic.
        double min_bkts = static_cast<double>(n_elt + n_ins) /
                          static_cast<double>(_M_max_load_factor);
        unsigned long want = static_cast<unsigned long>(min_bkts) + 1;
        if (want < n_bkt * 2) {
            want = n_bkt * 2;
        }
        return {true, _M_next_bkt(want)};
    }
    return {false, 0};
}

// ---- std::chrono ----------------------------------------------------------
//
// Both clocks read the same counter, because there is one: monotonic nanoseconds
// since the kernel started counting. `system_clock` claiming to be a wall clock
// would be the only lie in this file — there is no battery-backed clock and no
// network, so the epoch is boot, and `docs/LIBC-CONTRACT.md` records that decision
// where `clock_gettime` makes it.
//
// A caller measuring a *duration* — which is what almost every user of these does,
// including Qt's animation driver — gets an exact answer either way.

chrono::steady_clock::time_point chrono::steady_clock::now() noexcept {
    struct timespec ts = {0, 0};
    ::clock_gettime(1 /* CLOCK_MONOTONIC */, &ts);
    return time_point(duration(static_cast<long long>(ts.tv_sec) * 1000000000LL + ts.tv_nsec));
}

chrono::system_clock::time_point chrono::system_clock::now() noexcept {
    struct timespec ts = {0, 0};
    ::clock_gettime(0 /* CLOCK_REALTIME */, &ts);
    return time_point(duration(static_cast<long long>(ts.tv_sec) * 1000000LL +
                               ts.tv_nsec / 1000));
}

// ---- std::condition_variable ----------------------------------------------
//
// A thin layer over `pthread_cond_t`, which is what it is in libstdc++ too — the
// class holds one, and these five functions are the only part not in the header.
//
// The `unique_lock` overload takes the mutex out of the lock object and hands the
// raw `pthread_mutex_t` to `pthread_cond_wait`, which is the whole trick: the
// condition variable must release *that* mutex atomically with sleeping, and a
// wrapper that unlocked and then waited would lose every signal in between.

// `_M_cond` is libstdc++'s `__condvar`, not a raw `pthread_cond_t` — it is a thin
// wrapper that already calls the pthread functions inline. So these four forward to
// it rather than to pthread directly: reaching past the wrapper would mean this
// file and the header disagreeing about which of them owns the initialisation.
condition_variable::condition_variable() noexcept = default;

condition_variable::~condition_variable() = default;

void condition_variable::notify_one() noexcept {
    _M_cond.notify_one();
}

void condition_variable::notify_all() noexcept {
    _M_cond.notify_all();
}

// Not `noexcept`, because the header does not say so: `wait` is allowed to throw
// `system_error`, and a definition that promised more than the declaration is an
// error rather than a stricter promise.
void condition_variable::wait(unique_lock<mutex> &lock) {
    _M_cond.wait(*lock.mutex());
}

// `std::future` and the timed `wait_for` go through this rather than through the
// condition variable: libstdc++ builds them on a futex directly.
//
// There is no futex here — the kernel's primitive is a notification, and the C
// library's parkers are built on it — so this polls with a yield until the value
// changes or the deadline passes. That is honest and it is not free: a thread
// waiting on a future burns its timeslice. It is written down rather than hidden
// because the fix is a real one (a futex-shaped syscall) and nothing has needed it
// enough to justify the ABI yet.
bool __atomic_futex_unsigned_base::_M_futex_wait_until(
    unsigned *addr, unsigned val, bool has_timeout,
    chrono::seconds seconds, chrono::nanoseconds nanoseconds) {
    const unsigned long long deadline =
        static_cast<unsigned long long>(seconds.count()) * 1000000000ULL +
        static_cast<unsigned long long>(nanoseconds.count());
    for (;;) {
        if (__atomic_load_n(addr, __ATOMIC_ACQUIRE) != val) {
            return true;
        }
        if (has_timeout) {
            struct timespec ts = {0, 0};
            ::clock_gettime(1 /* CLOCK_MONOTONIC */, &ts);
            const unsigned long long now =
                static_cast<unsigned long long>(ts.tv_sec) * 1000000000ULL +
                static_cast<unsigned long long>(ts.tv_nsec);
            if (now >= deadline) {
                return false;
            }
        }
        ::sched_yield();
    }
}

// ---- std::exception -------------------------------------------------------
//
// One string for every exception type. With `-fno-exceptions` nothing constructs a
// derived exception and nothing catches one, so the only way this is reached is a
// program calling `what()` on a base it made itself — and the honest answer to that
// is the name of the type it has.
const char *exception::what() const noexcept {
    return "std::exception";
}

// `current_exception` outside a handler is a null `exception_ptr`, which is what it
// returns here always: with `-fno-exceptions` there is never a handler to be inside.
__exception_ptr::exception_ptr current_exception() noexcept {
    return __exception_ptr::exception_ptr();
}

// ---- std::pmr -------------------------------------------------------------
//
// The polymorphic allocators. Qt uses `monotonic_buffer_resource` for parsing —
// allocate quickly, free everything at once — and libstdc++ leaves its destructor,
// its buffer release and its vtable out of line.
//
// `get_default_resource` returns the new/delete resource, which is what a program
// that never sets one gets. The settable global is absent on purpose: it would be a
// process-wide mutable pointer nothing in this system changes, and an unused knob
// is a thing to keep in step for no reason.

namespace pmr {

namespace {

// The default resource: `operator new` and `operator delete`, which is exactly
// what `std::pmr::new_delete_resource()` is defined to be.
class NewDeleteResource final : public memory_resource {
public:
    ~NewDeleteResource() override = default;

private:
    void *do_allocate(size_t bytes, size_t alignment) override {
        return ::operator new(bytes, align_val_t(alignment));
    }

    void do_deallocate(void *p, size_t bytes, size_t alignment) override {
        ::operator delete(p, bytes, align_val_t(alignment));
    }

    bool do_is_equal(const memory_resource &other) const noexcept override {
        // Two resources are equal when memory from one can be returned to the
        // other. There is one of these, so identity is the whole answer.
        return this == &other;
    }
};

NewDeleteResource &default_resource() {
    // A function-local static, so it is constructed on first use and never before
    // — a namespace-scope object here would need `.init_array` to have run, and
    // `get_default_resource` can be called from another static's constructor.
    static NewDeleteResource resource;
    return resource;
}

}  // namespace

memory_resource *get_default_resource() noexcept {
    return &default_resource();
}

// The base's destructor, which libstdc++ declares and leaves out of line. Named by
// the linker the moment a derived resource above got one of its own.
memory_resource::~memory_resource() = default;

monotonic_buffer_resource::~monotonic_buffer_resource() {
    release();
}

// `_Chunk` is declared in the header and *defined* in libstdc++'s own source, so
// its layout is not visible here. That is a real limit and it is stated rather than
// worked around by guessing at the fields: a struct written from memory that
// happened to be the wrong size would free the wrong addresses.
//
// So this releases nothing and says so. What it costs is bounded and known: a
// `monotonic_buffer_resource` never returns its blocks to the upstream resource,
// which for the default upstream means they stay allocated until the process ends.
// That is the same shape as `munmap` here — this system does not return pages
// either — and a resource whose whole premise is "allocate quickly, free once" is
// the least surprising place for it.
//
// The day something needs it back, the fix is to build against a libstdc++ whose
// sources are present rather than to reconstruct a private layout.
void monotonic_buffer_resource::_M_release_buffers() noexcept {
    _M_head = nullptr;
}

// Take another buffer from upstream, at least `bytes` with `alignment`.
//
// libstdc++'s version threads the block onto `_M_head` so `release()` can hand it
// back. This one does not, for the reason above: `_Chunk`'s layout is not visible
// here, and a link in a struct written from memory would be a free at the wrong
// address. So each buffer is taken and kept — which is exactly what
// `_M_release_buffers` already documents, and which makes the two halves agree
// rather than one of them pretending.
//
// The growth is geometric, as the standard requires: each buffer at least twice the
// last, so an allocator used the way this one is meant to be — many small
// allocations, one release — asks upstream a logarithmic number of times.
void monotonic_buffer_resource::_M_new_buffer(size_t bytes, size_t alignment) {
    size_t want = _M_next_bufsiz;
    if (want < bytes + alignment) {
        want = bytes + alignment;
    }
    void *const buffer = _M_upstream->allocate(want, alignof(max_align_t));
    _M_current_buf = buffer;
    _M_avail = want;
    // Doubling, saturating rather than wrapping: a resource asked for something
    // near the address space would otherwise ask upstream for a small buffer next
    // time and loop.
    _M_next_bufsiz = (want > (size_t(-1) / 2)) ? want : want * 2;

    // Align the caller's allocation inside the fresh buffer, which is what the
    // header's `do_allocate` expects to find on return.
    void *p = buffer;
    size_t space = want;
    p = std::align(alignment, bytes, p, space);
    _M_current_buf = p;
    _M_avail = space;
}

}  // namespace pmr

// ---- iostreams ------------------------------------------------------------
//
// The static initialiser that constructs `std::cout` and friends. It is empty
// because nothing here constructs them: this system's output is `printf` over
// `DebugWrite`, and a `std::cout` that existed would need a `streambuf` over the
// same call plus the locale machinery behind `<iostream>`.
//
// Defined rather than absent because libstdc++'s `<iostream>` emits a reference to
// it in every translation unit that includes the header — Qt includes it in a few —
// so leaving it out fails the link in files that never print anything.
void ios_base_library_init() {}

}  // namespace std

// ---- std::filesystem::path ------------------------------------------------
//
// Qt uses `std::filesystem::path` in a handful of places, and libstdc++ leaves
// three pieces of it out of line. `_M_split_cmpts` is the parser: it takes the
// string a path was built from and breaks it into root, directories and filename.
//
// This system's paths are archive member names — flat, no root, always relative —
// so the parser here is the small correct one for that shape rather than the
// general one that has to answer what `C:` means. A path with a leading slash is
// accepted and its slash ignored, because that is what every caller writing
// `/fonts/x.ttf` means here.

namespace std {
namespace filesystem {
inline namespace __cxx11 {

void path::_List::_Impl_deleter::operator()(_Impl *p) const noexcept {
    // The component list is a single allocation holding its own header, so it goes
    // back the way it came and not element by element.
    if (p != nullptr) {
        ::operator delete(p);
    }
}

path::_List::_List() = default;

void path::_M_split_cmpts() {
    // Every component of this system's paths is a filename: there is one directory
    // level in the archive's own view, no root name, and no `..` to resolve. So the
    // list stays empty and the path is its own single component, which is what
    // `begin() == end()` and `filename() == *this` together mean.
    //
    // Leaving it empty is not a stub: an empty `_List` is precisely how libstdc++
    // represents a path with no separable parts, and every accessor already answers
    // correctly from the string when it finds one.
    //
    // Assigning a fresh `_List` rather than calling `clear()`, because `clear()` is
    // itself out of line in libstdc++'s sources and would be the next symbol the
    // linker asked for — the same loop, one step further along, for no gain.
    _M_cmpts = _List();
}

}  // namespace __cxx11
}  // namespace filesystem
}  // namespace std

// ---- the C++ ABI's remaining hooks ----------------------------------------

extern "C" {

// `__cxa_demangle` turns a mangled name into a readable one. Qt's logging asks for
// it when printing a type.
//
// Null with `status = -2`, which the ABI defines as "not a valid mangled name" and
// every caller already handles — Qt prints the mangled name instead. A demangler is
// a parser for a grammar with templates and substitutions in it, and writing one to
// make a log line prettier is the wrong trade.
char *__cxa_demangle(const char *, char *, size_t *, int *status) {
    if (status != nullptr) {
        *status = -2;
    }
    return nullptr;
}

// `__cxa_thread_atexit` used to be here, forwarding every `thread_local`
// destructor to the process-wide `__cxa_atexit` list — which ran them at process
// exit rather than at thread exit. It is now in `crates/staros-libc/src/thread.rs`,
// with a per-thread list, because the list belongs to the thread layer and because
// the shortcut was not merely late: `QThreadPrivate::cleanup` runs from such a
// destructor and is what wakes `QThread::wait`, so a QML program stopped waiting
// for a thread that had already returned. The comment there records it.

}  // extern "C"

// ---- std::map and std::set ------------------------------------------------
//
// The red-black tree, in the global namespace where libstdc++ declares it.
//
// Written out rather than approximated: a tree that is merely *sorted* works and
// degrades to a list on ordered input, which is the input a map of file names or
// timestamps actually receives. The invariants are the standard four, and the
// rebalance below is the textbook one.

namespace {

// The colours, as libstdc++ names them in `<bits/stl_tree.h>`.
const std::_Rb_tree_color kRed = std::_S_red;
const std::_Rb_tree_color kBlack = std::_S_black;

void rotate_left(std::_Rb_tree_node_base *const x, std::_Rb_tree_node_base *&root) {
    std::_Rb_tree_node_base *const y = x->_M_right;
    x->_M_right = y->_M_left;
    if (y->_M_left != nullptr) {
        y->_M_left->_M_parent = x;
    }
    y->_M_parent = x->_M_parent;

    if (x == root) {
        root = y;
    } else if (x == x->_M_parent->_M_left) {
        x->_M_parent->_M_left = y;
    } else {
        x->_M_parent->_M_right = y;
    }
    y->_M_left = x;
    x->_M_parent = y;
}

void rotate_right(std::_Rb_tree_node_base *const x, std::_Rb_tree_node_base *&root) {
    std::_Rb_tree_node_base *const y = x->_M_left;
    x->_M_left = y->_M_right;
    if (y->_M_right != nullptr) {
        y->_M_right->_M_parent = x;
    }
    y->_M_parent = x->_M_parent;

    if (x == root) {
        root = y;
    } else if (x == x->_M_parent->_M_right) {
        x->_M_parent->_M_right = y;
    } else {
        x->_M_parent->_M_left = y;
    }
    y->_M_right = x;
    x->_M_parent = y;
}

}  // namespace

std::_Rb_tree_node_base *std::_Rb_tree_increment(std::_Rb_tree_node_base *x) noexcept {
    if (x->_M_right != nullptr) {
        x = x->_M_right;
        while (x->_M_left != nullptr) {
            x = x->_M_left;
        }
        return x;
    }
    std::_Rb_tree_node_base *y = x->_M_parent;
    while (x == y->_M_right) {
        x = y;
        y = y->_M_parent;
    }
    // The header's right child is the tree's maximum, so incrementing past it lands
    // on the header — which is `end()`. Without this test the walk would return the
    // header's *parent* and iteration would run backwards from the top.
    // Incrementing the maximum lands on the header, which is `end()`. The extra
    // test guards the case where `x` *is* the header — that is `++end()`, which the
    // standard leaves undefined and which nothing can therefore test. Removing it
    // was tried and no check failed; it stays because libstdc++ has it, and
    // matching an ABI's behaviour includes the parts nobody is allowed to observe.
    if (x->_M_right != y) {
        x = y;
    }
    return x;
}

namespace std {
// The `const` overloads have to be *inside* the namespace rather than defined with
// a qualified name. An out-of-line `const std::T *std::f(const std::T *)` is read as
// redefining the non-const overload with a mismatched parameter, which the compiler
// then reports as "does not match any declaration" — a message that points at the
// return type and means the qualification.
const _Rb_tree_node_base *_Rb_tree_increment(const _Rb_tree_node_base *x) noexcept {
    return _Rb_tree_increment(const_cast<_Rb_tree_node_base *>(x));
}
}  // namespace std

std::_Rb_tree_node_base *std::_Rb_tree_decrement(std::_Rb_tree_node_base *x) noexcept {
    // The header is recognised by being red with a parent that points back at it —
    // an arrangement no real node can have, which is exactly why libstdc++ chose it.
    if (x->_M_color == kRed && x->_M_parent->_M_parent == x) {
        return x->_M_right;
    }
    if (x->_M_left != nullptr) {
        std::_Rb_tree_node_base *y = x->_M_left;
        while (y->_M_right != nullptr) {
            y = y->_M_right;
        }
        return y;
    }
    std::_Rb_tree_node_base *y = x->_M_parent;
    while (x == y->_M_left) {
        x = y;
        y = y->_M_parent;
    }
    return y;
}

namespace std {
const _Rb_tree_node_base *_Rb_tree_decrement(const _Rb_tree_node_base *x) noexcept {
    return _Rb_tree_decrement(const_cast<_Rb_tree_node_base *>(x));
}
}  // namespace std

void std::_Rb_tree_insert_and_rebalance(const bool insert_left,
                                        std::_Rb_tree_node_base *x,
                                        std::_Rb_tree_node_base *p,
                                        std::_Rb_tree_node_base &header) noexcept {
    std::_Rb_tree_node_base *&root = header._M_parent;

    x->_M_parent = p;
    x->_M_left = nullptr;
    x->_M_right = nullptr;
    x->_M_color = kRed;

    if (insert_left) {
        p->_M_left = x;
        if (p == &header) {
            // The first node: the header's three pointers all name it.
            header._M_parent = x;
            header._M_right = x;
        } else if (p == header._M_left) {
            header._M_left = x;  // a new minimum
        }
    } else {
        p->_M_right = x;
        if (p == header._M_right) {
            header._M_right = x;  // a new maximum
        }
    }

    // Rebalance: a red node under a red parent is the only violation an insert can
    // create, and it moves up the tree until an uncle is black or the root is
    // reached.
    while (x != root && x->_M_parent->_M_color == kRed) {
        std::_Rb_tree_node_base *const grandparent = x->_M_parent->_M_parent;
        if (x->_M_parent == grandparent->_M_left) {
            std::_Rb_tree_node_base *const uncle = grandparent->_M_right;
            if (uncle != nullptr && uncle->_M_color == kRed) {
                x->_M_parent->_M_color = kBlack;
                uncle->_M_color = kBlack;
                grandparent->_M_color = kRed;
                x = grandparent;
            } else {
                if (x == x->_M_parent->_M_right) {
                    x = x->_M_parent;
                    rotate_left(x, root);
                }
                x->_M_parent->_M_color = kBlack;
                grandparent->_M_color = kRed;
                rotate_right(grandparent, root);
            }
        } else {
            std::_Rb_tree_node_base *const uncle = grandparent->_M_left;
            if (uncle != nullptr && uncle->_M_color == kRed) {
                x->_M_parent->_M_color = kBlack;
                uncle->_M_color = kBlack;
                grandparent->_M_color = kRed;
                x = grandparent;
            } else {
                if (x == x->_M_parent->_M_left) {
                    x = x->_M_parent;
                    rotate_right(x, root);
                }
                x->_M_parent->_M_color = kBlack;
                grandparent->_M_color = kRed;
                rotate_left(grandparent, root);
            }
        }
    }
    root->_M_color = kBlack;
}

std::_Rb_tree_node_base *std::_Rb_tree_rebalance_for_erase(
    std::_Rb_tree_node_base *const z, std::_Rb_tree_node_base &header) noexcept {
    std::_Rb_tree_node_base *&root = header._M_parent;
    std::_Rb_tree_node_base *&leftmost = header._M_left;
    std::_Rb_tree_node_base *&rightmost = header._M_right;
    std::_Rb_tree_node_base *y = z;
    std::_Rb_tree_node_base *x = nullptr;
    std::_Rb_tree_node_base *x_parent = nullptr;

    if (y->_M_left == nullptr) {
        x = y->_M_right;
    } else if (y->_M_right == nullptr) {
        x = y->_M_left;
    } else {
        // Two children: `y` becomes the successor, which is spliced into `z`'s
        // place. The node is *moved* rather than its value copied, because a map's
        // value type need not be assignable and iterators to it must stay valid.
        y = y->_M_right;
        while (y->_M_left != nullptr) {
            y = y->_M_left;
        }
        x = y->_M_right;
    }

    if (y != z) {
        z->_M_left->_M_parent = y;
        y->_M_left = z->_M_left;
        if (y != z->_M_right) {
            x_parent = y->_M_parent;
            if (x != nullptr) {
                x->_M_parent = y->_M_parent;
            }
            y->_M_parent->_M_left = x;
            y->_M_right = z->_M_right;
            z->_M_right->_M_parent = y;
        } else {
            x_parent = y;
        }
        if (root == z) {
            root = y;
        } else if (z->_M_parent->_M_left == z) {
            z->_M_parent->_M_left = y;
        } else {
            z->_M_parent->_M_right = y;
        }
        y->_M_parent = z->_M_parent;
        std::_Rb_tree_color tmp = y->_M_color;
        y->_M_color = z->_M_color;
        z->_M_color = tmp;
        y = z;  // `y` now points at the node actually removed
    } else {
        x_parent = y->_M_parent;
        if (x != nullptr) {
            x->_M_parent = y->_M_parent;
        }
        if (root == z) {
            root = x;
        } else if (z->_M_parent->_M_left == z) {
            z->_M_parent->_M_left = x;
        } else {
            z->_M_parent->_M_right = x;
        }
        // The extremes may have been the node just removed.
        if (leftmost == z) {
            leftmost = (z->_M_right == nullptr) ? z->_M_parent : std::_Rb_tree_increment(x);
        }
        if (rightmost == z) {
            rightmost = (z->_M_left == nullptr) ? z->_M_parent : std::_Rb_tree_decrement(x);
        }
    }

    if (y->_M_color != kRed) {
        // Removing a black node shortens one path, so a black must be found to
        // replace it — up the tree if necessary.
        while (x != root && (x == nullptr || x->_M_color == kBlack)) {
            if (x == x_parent->_M_left) {
                std::_Rb_tree_node_base *w = x_parent->_M_right;
                if (w->_M_color == kRed) {
                    w->_M_color = kBlack;
                    x_parent->_M_color = kRed;
                    rotate_left(x_parent, root);
                    w = x_parent->_M_right;
                }
                if ((w->_M_left == nullptr || w->_M_left->_M_color == kBlack) &&
                    (w->_M_right == nullptr || w->_M_right->_M_color == kBlack)) {
                    w->_M_color = kRed;
                    x = x_parent;
                    x_parent = x_parent->_M_parent;
                } else {
                    if (w->_M_right == nullptr || w->_M_right->_M_color == kBlack) {
                        if (w->_M_left != nullptr) {
                            w->_M_left->_M_color = kBlack;
                        }
                        w->_M_color = kRed;
                        rotate_right(w, root);
                        w = x_parent->_M_right;
                    }
                    w->_M_color = x_parent->_M_color;
                    x_parent->_M_color = kBlack;
                    if (w->_M_right != nullptr) {
                        w->_M_right->_M_color = kBlack;
                    }
                    rotate_left(x_parent, root);
                    break;
                }
            } else {
                // The mirror image. Written out rather than folded into the branch
                // above with a pair of function pointers: the two differ only in
                // left and right, and every attempt to share them makes the one
                // place a rotation is chosen harder to check than two.
                std::_Rb_tree_node_base *w = x_parent->_M_left;
                if (w->_M_color == kRed) {
                    w->_M_color = kBlack;
                    x_parent->_M_color = kRed;
                    rotate_right(x_parent, root);
                    w = x_parent->_M_left;
                }
                if ((w->_M_right == nullptr || w->_M_right->_M_color == kBlack) &&
                    (w->_M_left == nullptr || w->_M_left->_M_color == kBlack)) {
                    w->_M_color = kRed;
                    x = x_parent;
                    x_parent = x_parent->_M_parent;
                } else {
                    if (w->_M_left == nullptr || w->_M_left->_M_color == kBlack) {
                        if (w->_M_right != nullptr) {
                            w->_M_right->_M_color = kBlack;
                        }
                        w->_M_color = kRed;
                        rotate_left(w, root);
                        w = x_parent->_M_left;
                    }
                    w->_M_color = x_parent->_M_color;
                    x_parent->_M_color = kBlack;
                    if (w->_M_left != nullptr) {
                        w->_M_left->_M_color = kBlack;
                    }
                    rotate_right(x_parent, root);
                    break;
                }
            }
        }
        if (x != nullptr) {
            x->_M_color = kBlack;
        }
    }
    return y;
}

// ---------------------------------------------------------------------------
// The exception ABI, which exists so that it can refuse
// ---------------------------------------------------------------------------
//
// Everything here is built `-fno-exceptions`, and the header at the top of this
// file said that keeps `__cxa_throw` out. It does — out of code *this tree*
// compiles. It does not keep it out of libstdc++'s headers, which are not compiled
// with that flag when they are written and which contain `throw` in inline and
// template code that a `-fno-exceptions` translation unit still instantiates. Qt
// found it: linking `services/qt-hello` asked for eight names in one go.
//
// So they are here, and every one of them stops the program. That is not a
// placeholder standing in for a real unwinder — it is the only behaviour that can
// be correct without one. `__cxa_throw` cannot return: its caller has already given
// up on the current control flow and there is nothing after the call. It cannot
// unwind: unwinding needs `.eh_frame`, a personality routine and a stack walker,
// none of which this build produces. What is left is to say which exception was
// thrown, on the console, and stop — which is more than a program that jumps into
// a garbage return address would have said.
//
// The message matters. A C++ program that dies here died at a `throw`, and the
// type name is usually enough to find it.

namespace {

// The type name out of a `std::type_info`, without RTTI.
//
// `type_info::name()` is a virtual call, and calling it needs the vtable this build
// may not have emitted for the concrete type. The mangled name is the first member
// of the object after the vtable pointer, which is layout the Itanium ABI fixes, so
// reading it directly works where the call would not. It is the mangled form —
// `St9bad_alloc` rather than `std::bad_alloc` — and `__cxa_demangle` is in this file
// for anyone who wants the other.
const char *type_name(const void *type) {
    if (type == nullptr) {
        return "<no type>";
    }
    const char *name = *reinterpret_cast<const char *const *>(
        reinterpret_cast<const char *>(type) + sizeof(void *));
    return name != nullptr ? name : "<unnamed type>";
}

}  // namespace

extern "C" {

// The buffer a thrown object is constructed into.
//
// A real implementation allocates, so that a throw during stack unwinding has
// somewhere to put a second exception. Here the first throw ends the program, so
// there is never a second one and one static buffer is enough. 256 bytes covers
// every standard exception type; anything larger stops here rather than being
// silently truncated into it, because a `throw` of an object too large to hold is
// still a `throw` and still ends the program — just with a clearer reason.
alignas(16) static unsigned char exception_storage[256];

void *__cxa_allocate_exception(std::size_t size) noexcept {
    if (size > sizeof(exception_storage)) {
        fail("an exception object larger than the throw buffer was allocated");
    }
    return exception_storage;
}

void __cxa_free_exception(void *) noexcept {}

void __cxa_throw(void *object, std::type_info *type, void (*destructor)(void *)) {
    (void)object;
    (void)destructor;
    std::fprintf(stderr, "[c++] throw of %s, and this build has no unwinder\n",
                 type_name(type));
    std::fflush(stderr);
    std::abort();
}

void __cxa_rethrow() {
    fail("rethrow, and there was nothing in flight to rethrow");
}

// `catch` cannot be entered, because nothing ever unwinds into one. Reaching either
// of these means the unwinder ran, which it cannot.
void *__cxa_begin_catch(void *) noexcept {
    fail("entered a catch block, which requires an unwinder this build does not have");
}

void __cxa_end_catch() {
    fail("left a catch block that was never entered");
}

// What `std::current_exception` and `std::exception_ptr` ask. There is never one in
// flight — a throw ends the program — so the honest answer is null, and unlike the
// functions above this one is *reachable*: `std::current_exception()` outside a
// catch block is legal and returns an empty pointer.
std::type_info *__cxa_current_exception_type() noexcept { return nullptr; }

// The personality routine, named in every `.eh_frame` CIE the compiler emits even
// under `-fno-exceptions` when it inlines code that was not. It is called by the
// unwinder, so reaching it means an unwind began.
int __gxx_personality_v0(int, int, unsigned long long, void *, void *) {
    fail("the unwinder ran, and there is no unwinder");
}

[[noreturn]] void _Unwind_Resume(void *) {
    fail("_Unwind_Resume: unwinding cannot be resumed because it never started");
}

}  // extern "C"

// ---------------------------------------------------------------------------
// The exception classes themselves
// ---------------------------------------------------------------------------
//
// Not the throwing machinery above but the objects: `std::exception` and
// `std::bad_alloc` are ordinary polymorphic classes whose *vtables* are emitted
// wherever their key function is defined, and their key functions live in
// libstdc++.a. Defining them here is what puts the vtables in this object.
//
// They are reachable code, not refusals. Nothing throws, but `std::bad_alloc` is a
// complete type that code constructs, copies and destroys — libstdc++'s
// `__throw_bad_alloc` above builds one on the way to reporting — and `what()` is an
// ordinary virtual call that has to return something.

namespace std {

exception::~exception() noexcept {}
bad_alloc::~bad_alloc() noexcept {}

const char *bad_alloc::what() const noexcept { return "std::bad_alloc"; }

// `std::make_shared`'s tag. `_S_eq` compares a `type_info` against the tag's own to
// decide whether a `shared_ptr`'s control block came from `make_shared` — which
// matters for `_M_get_deleter` and for nothing else here. With no RTTI there is no
// type to compare, and false is the answer that says "this control block was not
// made that way": it makes `get_deleter` return null, which is a documented result,
// rather than handing back a pointer into a block of the wrong shape.
bool _Sp_make_shared_tag::_S_eq(const type_info &) noexcept { return false; }

}  // namespace std

// ---------------------------------------------------------------------------
// The RTTI class hierarchy, which exists for its vtables
// ---------------------------------------------------------------------------
//
// Every polymorphic class has a `type_info` object, and every `type_info` object
// for a class is an instance of one of the `__cxxabiv1::__*_type_info` types — so
// the moment libstdc++'s headers emit the type information for `std::bad_alloc`,
// the linker wants the vtables of those types. `-fno-rtti` does not prevent this:
// it stops `typeid` and `dynamic_cast` in *our* code, and the exception classes
// still carry their type information because `__cxa_throw` takes it as an argument.
//
// A vtable is emitted where the class's key function is defined, and it references
// every virtual — so defining one destructor pulls the whole set in. All of them are
// here, and the split between them is the interesting part:
//
//   * The destructors are real. They do nothing, which is correct — these objects
//     are static and own nothing — and defining them is the entire reason this
//     section exists.
//   * The comparison functions stop the program. `__do_catch` is only called during
//     a handler match, which cannot happen: a throw ends the program before any
//     handler is looked for. `__do_dyncast`, `__do_upcast` and `__do_find_public_src`
//     are libstdc++'s *internal* walkers, called by libstdc++'s own `__dynamic_cast`
//     — and the `__dynamic_cast` on this system is the one in the section below,
//     which walks the same structures directly and calls none of them. Reaching any
//     of these means an assumption here is wrong, which is worth a message rather
//     than a wrong answer.
//
// Answering `false` instead — "no, this handler does not match" — was the
// alternative, and it is worse in the one case it would arise: a `catch` that
// silently never matches turns an exception into a call to `std::terminate` from a
// place unrelated to the throw.

namespace std {

// The base of the hierarchy, and the first thing the linker asked for once the two
// derived classes below had vtables. `~type_info` is its key function.
//
// `__is_pointer_p` and `__is_function_p` are the two that answer rather than stop,
// and they can: a `std::type_info` that is neither a `__pointer_type_info` nor a
// `__function_type_info` describes something that is not a pointer and not a
// function, and those derived classes override these to say otherwise. False is the
// base class's correct answer, not a shrug.
type_info::~type_info() {}

bool type_info::__is_pointer_p() const { return false; }
bool type_info::__is_function_p() const { return false; }

bool type_info::__do_catch(const type_info *, void **, unsigned) const {
    fail("type_info::__do_catch: matching a handler without an unwinder");
}

bool type_info::__do_upcast(const __cxxabiv1::__class_type_info *, void **) const {
    fail("type_info::__do_upcast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

}  // namespace std

namespace __cxxabiv1 {

__class_type_info::~__class_type_info() {}
__si_class_type_info::~__si_class_type_info() {}

bool __class_type_info::__do_upcast(const __class_type_info *, void **) const {
    fail("__do_upcast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

bool __class_type_info::__do_catch(const std::type_info *, void **, unsigned) const {
    fail("__do_catch: matching a handler in a build without an unwinder");
}

bool __class_type_info::__do_upcast(const __class_type_info *, const void *,
                                    __upcast_result &) const {
    fail("__do_upcast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

bool __class_type_info::__do_dyncast(std::ptrdiff_t, __sub_kind,
                                     const __class_type_info *, const void *,
                                     const __class_type_info *, const void *,
                                     __dyncast_result &) const {
    fail("__do_dyncast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

__class_type_info::__sub_kind __class_type_info::__do_find_public_src(
    std::ptrdiff_t, const void *, const __class_type_info *, const void *) const {
    fail("__do_find_public_src: libstdc++'s own dynamic-cast walker, which this build does not use");
}

bool __si_class_type_info::__do_upcast(const __class_type_info *, const void *,
                                       __upcast_result &) const {
    fail("__do_upcast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

bool __si_class_type_info::__do_dyncast(std::ptrdiff_t, __sub_kind,
                                        const __class_type_info *, const void *,
                                        const __class_type_info *, const void *,
                                        __dyncast_result &) const {
    fail("__do_dyncast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

__class_type_info::__sub_kind __si_class_type_info::__do_find_public_src(
    std::ptrdiff_t, const void *, const __class_type_info *, const void *) const {
    fail("__do_find_public_src: libstdc++'s own dynamic-cast walker, which this build does not use");
}

// The third kind, for a class with several bases or a virtual one. Its vtable is
// needed for the same reason as the other two, and additionally because the walker
// below asks `typeid(*type) == typeid(__vmi_class_type_info)` — a comparison that
// needs this class's own type information to exist.
__vmi_class_type_info::~__vmi_class_type_info() {}

bool __vmi_class_type_info::__do_upcast(const __class_type_info *, const void *,
                                        __upcast_result &) const {
    fail("__do_upcast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

bool __vmi_class_type_info::__do_dyncast(std::ptrdiff_t, __sub_kind,
                                         const __class_type_info *, const void *,
                                         const __class_type_info *, const void *,
                                         __dyncast_result &) const {
    fail("__do_dyncast: libstdc++'s own dynamic-cast walker, which this build does not use");
}

__class_type_info::__sub_kind __vmi_class_type_info::__do_find_public_src(
    std::ptrdiff_t, const void *, const __class_type_info *, const void *) const {
    fail("__do_find_public_src: libstdc++'s own dynamic-cast walker, which this build does not use");
}

// Type information for pointers and for functions.
//
// These arrived with RTTI and not before, and the reason is worth a line: with
// `-fno-rtti` a `typeid` for a pointer type is never emitted, so nothing referenced
// their vtables. With RTTI on, Qt Quick's property system emits type information for
// pointer-to-QObject types as a matter of course, and the linker then wants the
// vtable of the class those objects are instances of.
//
// `__is_pointer_p` and `__is_function_p` are the honest overrides — these classes
// exist precisely to answer those two questions with `true`, and the base class's
// `false` would be wrong. The catch helpers stop the program for the same reason as
// every other one above: they are reached only while matching a handler, and a throw
// here ends the program before any handler is looked for.
__pbase_type_info::~__pbase_type_info() {}
__pointer_type_info::~__pointer_type_info() {}
__function_type_info::~__function_type_info() {}

bool __pointer_type_info::__is_pointer_p() const { return true; }
bool __function_type_info::__is_function_p() const { return true; }

bool __pbase_type_info::__do_catch(const std::type_info *, void **, unsigned) const {
    fail("__pbase_type_info::__do_catch: matching a handler without an unwinder");
}

bool __pointer_type_info::__pointer_catch(const __pbase_type_info *, void **,
                                          unsigned) const {
    fail("__pointer_type_info::__pointer_catch: matching a handler without an unwinder");
}

}  // namespace __cxxabiv1

// ---------------------------------------------------------------------------
// `dynamic_cast`
// ---------------------------------------------------------------------------
//
// This is the one piece of the C++ runtime here that implements an algorithm rather
// than filling in a hook, and it exists because Qt Quick cannot draw without it.
//
// `QSGSoftwareRenderableNodeUpdater::visit(QSGGeometryNode *)` — the software scene
// graph's dispatch, which decides what a node is before drawing it — is five chained
// `dynamic_cast`s over the public node classes, ending in `// We dont know, so skip`.
// There is no type enum to switch on instead. A build without `dynamic_cast` renders
// every QML scene as an empty window and reports nothing wrong, which is the worst
// shape a failure can have.
//
// What the compiler emits at a `dynamic_cast<D *>(p)` is a call to
// `__cxxabiv1::__dynamic_cast`, and everything it needs is reachable from `p`:
//
//   * the object's vtable pointer is the first word of any polymorphic object;
//   * `vtable[-1]` is that object's `std::type_info`;
//   * `vtable[-2]` is the offset from this subobject back to the complete object.
//
// From the complete object's type information the class graph is walkable, because
// the ABI gives each class one of exactly three type-information shapes:
// `__class_type_info` (no bases), `__si_class_type_info` (one public non-virtual
// base at offset zero) and `__vmi_class_type_info` (an array of bases, each with an
// offset and access flags).
//
// libstdc++ walks that graph through six virtual functions on those classes, with a
// result structure threaded through them. This does not implement those — they are
// the `fail()` stubs above — and walks the same structures directly instead. The
// reason is honesty about what is here: the libstdc++ algorithm is one long
// mutually-recursive function set tuned to answer several questions in one pass, and
// reproducing it from memory would produce something that looks right and is subtly
// wrong on the cases that matter. What is below is the rule from the standard,
// [expr.dynamic.cast]/8, written out as it reads.
//
// The `src2dst` hint the compiler passes — "the source is a unique public
// non-virtual base of the destination at this offset", or one of three sentinels
// meaning it could not say — is deliberately ignored. It is an optimisation: it lets
// an implementation answer without walking. Ignoring it costs a walk over a class
// graph with a handful of nodes, and it removes a whole class of bug where the fast
// path and the slow path disagree.

namespace {

// Where a subobject is, and whether some path to it was public all the way.
struct Subobject {
    const void *ptr;
    bool public_path;
};

// Everything here is bounded. Thirty-two distinct subobjects of one type inside one
// object is far past anything a real hierarchy does — Qt's deepest is single figures
// — and the alternative to a limit is an allocation on a path that must work when
// the heap does not.
constexpr unsigned MAX_SUBOBJECTS = 32;

class SubobjectSet {
   public:
    void add(const void *ptr, bool public_path) {
        for (unsigned i = 0; i < count_; ++i) {
            if (items_[i].ptr == ptr) {
                // The same subobject reached a second time: a virtual base, which is
                // shared, or a repeated base that happens to land here. It is one
                // subobject either way, and it is a *public* base of the whole if any
                // path to it is public — which is what the standard means by "public
                // base class subobject".
                items_[i].public_path = items_[i].public_path || public_path;
                return;
            }
        }
        if (count_ == MAX_SUBOBJECTS) {
            overflowed_ = true;
            return;
        }
        items_[count_++] = Subobject{ptr, public_path};
    }

    unsigned count() const { return count_; }
    const Subobject &operator[](unsigned i) const { return items_[i]; }
    bool overflowed() const { return overflowed_; }

   private:
    Subobject items_[MAX_SUBOBJECTS] = {};
    unsigned count_ = 0;
    bool overflowed_ = false;
};

// Where a base subobject sits, given the address of the subobject that derives from
// it.
//
// A non-virtual base is at a constant offset, fixed at compile time. A virtual base
// is not — the whole point of virtual inheritance is that the distance depends on
// what the complete object turned out to be — so its offset is stored in the vtable,
// and `__offset()` is where in the vtable to look: a byte displacement from the vptr,
// negative, into the vcall/vbase region above the address point.
const void *base_address(const void *ptr, const __cxxabiv1::__base_class_type_info &base) {
    if (!base.__is_virtual_p()) {
        return static_cast<const char *>(ptr) + base.__offset();
    }
    const char *vtable = *reinterpret_cast<const char *const *>(ptr);
    const std::ptrdiff_t offset =
        *reinterpret_cast<const std::ptrdiff_t *>(vtable + base.__offset());
    return static_cast<const char *>(ptr) + offset;
}

// Every subobject of type `wanted` inside the object of type `type` at `ptr`.
//
// The dispatch on which of the three shapes `type` has is by `typeid` and not by
// `dynamic_cast`, which would be circular. `typeid` on a polymorphic lvalue is a load
// of `vtable[-1]` and a comparison — it calls nothing, and in particular it does not
// call this.
void collect(const __cxxabiv1::__class_type_info *type, const void *ptr, bool public_path,
             const std::type_info &wanted, SubobjectSet &out) {
    using __cxxabiv1::__base_class_type_info;
    using __cxxabiv1::__si_class_type_info;
    using __cxxabiv1::__vmi_class_type_info;

    if (*static_cast<const std::type_info *>(type) == wanted) {
        out.add(ptr, public_path);
    }

    const std::type_info &shape = typeid(*type);
    if (shape == typeid(__si_class_type_info)) {
        // One base, public, non-virtual, at offset zero. That is not an assumption
        // about this hierarchy — it is the definition of this type-information shape,
        // and the compiler emits `__vmi_class_type_info` for anything else.
        const auto *si = static_cast<const __si_class_type_info *>(type);
        collect(si->__base_type, ptr, public_path, wanted, out);
        return;
    }
    if (shape == typeid(__vmi_class_type_info)) {
        const auto *vmi = static_cast<const __vmi_class_type_info *>(type);
        for (unsigned i = 0; i < vmi->__base_count; ++i) {
            const __base_class_type_info &base = vmi->__base_info[i];
            collect(base.__base_type, base_address(ptr, base),
                    public_path && base.__is_public_p(), wanted, out);
        }
        return;
    }
    // `__class_type_info` proper: no bases, nothing further to walk.
}

// Is `target` one of the subobjects in `found`, reached publicly?
bool found_public(const SubobjectSet &found, const void *target) {
    for (unsigned i = 0; i < found.count(); ++i) {
        if (found[i].ptr == target) {
            return found[i].public_path;
        }
    }
    return false;
}

}  // namespace

namespace __cxxabiv1 {

extern "C" void *__dynamic_cast(const void *src_ptr, const __class_type_info *src_type,
                                const __class_type_info *dst_type, std::ptrdiff_t src2dst) {
    // The hint. See the section comment: deliberately unused.
    (void)src2dst;

    // The compiler emits a null check before the call, so this is the second one.
    // It stays because the first one is the *compiler's* invariant, not this
    // function's, and a runtime entry point that reads through a null pointer
    // because its caller promised not to pass one is a bad trade.
    if (src_ptr == nullptr || src_type == nullptr || dst_type == nullptr) {
        return nullptr;
    }

    // The complete object, from the vtable of the subobject we were handed.
    const void *const *vtable = *reinterpret_cast<const void *const *const *>(src_ptr);
    const std::ptrdiff_t offset_to_top = reinterpret_cast<std::ptrdiff_t>(vtable[-2]);
    const auto *whole_type =
        static_cast<const __class_type_info *>(static_cast<const std::type_info *>(vtable[-1]));
    const void *whole_ptr = static_cast<const char *>(src_ptr) + offset_to_top;

    if (whole_type == nullptr) {
        // A vtable with no type information in it, which means some translation unit
        // in this program was compiled without RTTI. That is not a cast that failed,
        // it is a build that cannot answer, and the two must not look alike.
        fail("__dynamic_cast: this object's vtable carries no type information — "
             "some translation unit was compiled with -fno-rtti");
    }

    // Every subobject of the destination type inside the complete object.
    SubobjectSet destinations;
    collect(whole_type, whole_ptr, true, *dst_type, destinations);
    if (destinations.overflowed()) {
        fail("__dynamic_cast: more than 32 subobjects of one type in one object");
    }
    if (destinations.count() == 0) {
        return nullptr;
    }

    // [expr.dynamic.cast]/8, first rule: if the source points at a public base
    // subobject of exactly one destination-typed object, that object is the answer.
    // This is the rule that covers the ordinary downcast and every cross-cast.
    const void *answer = nullptr;
    unsigned matches = 0;
    for (unsigned i = 0; i < destinations.count(); ++i) {
        SubobjectSet sources;
        collect(dst_type, destinations[i].ptr, true, *src_type, sources);
        if (sources.overflowed()) {
            fail("__dynamic_cast: more than 32 subobjects of one type in one object");
        }
        if (found_public(sources, src_ptr)) {
            ++matches;
            answer = destinations[i].ptr;
        }
    }
    if (matches == 1) {
        return const_cast<void *>(answer);
    }
    if (matches > 1) {
        // Ambiguous by the first rule. The second rule below cannot rescue it — it
        // asks about the same object from the other end — so this is a null result,
        // which is what an ambiguous `dynamic_cast` is defined to give.
        return nullptr;
    }

    // Second rule: if the source points at a public base subobject of the *complete*
    // object, and the complete object has exactly one public destination-typed base,
    // that is the answer. This is the upcast-then-downcast case, where the source and
    // the destination are siblings under the most-derived type.
    SubobjectSet sources_in_whole;
    collect(whole_type, whole_ptr, true, *src_type, sources_in_whole);
    if (sources_in_whole.overflowed()) {
        fail("__dynamic_cast: more than 32 subobjects of one type in one object");
    }
    if (!found_public(sources_in_whole, src_ptr)) {
        return nullptr;
    }

    answer = nullptr;
    matches = 0;
    for (unsigned i = 0; i < destinations.count(); ++i) {
        if (destinations[i].public_path) {
            ++matches;
            answer = destinations[i].ptr;
        }
    }
    return matches == 1 ? const_cast<void *>(answer) : nullptr;
}

}  // namespace __cxxabiv1

// ---------------------------------------------------------------------------
// `std::locale`, exactly as far as it is asked for
// ---------------------------------------------------------------------------
//
// This is the smallest section in this file with the longest justification, because
// what it does *not* do is the decision.
//
// A real `std::locale` is a reference-counted `_Impl` holding an array of facets
// indexed by `locale::id`, with `use_facet` doing the lookup and `_M_install_facet`
// building it. That is a subsystem, and on this machine it would have exactly one
// instance — `crate::locale` in the C library accepts the name "C" and nothing else,
// because there is no second locale's data anywhere in the system to switch to.
//
// So what is built here is the one thing that is asked for and can be answered
// truthfully: `use_facet<ctype<char>>` returns the classic `ctype<char>`, because
// that is the only `ctype<char>` there is, whatever locale it is asked about.
//
// Qt reaches it through one line in a bundled third-party file:
// `double-conversion/string-to-double.cc` lowercases a character to match "infinity"
// and "nan" case-insensitively. That is the whole demand.
//
// The limit is deliberate and it is sharp: any *other* facet — `numpunct`,
// `num_get`, `collate`, the stream facets — fails at the link, naming itself. That
// is the intended behaviour, not an oversight. Each one is a decision about what
// this system's locale means, and a decision should be made when something asks
// rather than pre-empted by a lookup table full of guesses.

namespace std {

// `locale::id` needs storage; the value is never read here, because nothing does a
// lookup by id.
locale::id ctype<char>::id;

// The base every facet derives from. Its destructor is the key function, so this one
// line is what puts `locale::facet`'s vtable in this object — and `ctype<char>`'s
// vtable references it, which is how the linker came to ask.
//
// It does nothing, which is right: a facet owns the table it was given only when it
// was constructed with `del` set, and the one facet in this system was not.
locale::facet::~facet() {}

// The classic table is the C library's own, which is already correct and already
// used by `isalpha` and friends: one table, one answer, no chance of the C and C++
// halves disagreeing about whether a byte is a letter.
//
// glibc's table is indexed from -128 so that `isalpha(EOF)` works; the pointer
// `__ctype_b_loc` yields is at index 0, and libstdc++ indexes it with an
// `unsigned char`, so the ranges line up without adjustment.
const ctype_base::mask *ctype<char>::classic_table() throw() {
    return *__ctype_b_loc();
}

ctype<char>::ctype(const mask *table, bool del, size_t refs)
    : facet(refs),
      // Never used: the `__c_locale` handle is glibc's locale object, which is what
      // `ctype_byname` needs and what this build has no equivalent of. Every path
      // reachable here goes through `_M_table` instead.
      _M_c_locale_ctype(nullptr),
      _M_del(table != nullptr && del),
      _M_toupper(*__ctype_toupper_loc()),
      _M_tolower(*__ctype_tolower_loc()),
      _M_table(table != nullptr ? table : classic_table()),
      _M_widen_ok(0),
      _M_narrow_ok(0) {
    __builtin_memset(_M_widen, 0, sizeof(_M_widen));
    __builtin_memset(_M_narrow, 0, sizeof(_M_narrow));
}

ctype<char>::~ctype() {}

// The four case-conversion virtuals, over the C library's tables. `_M_toupper` and
// `_M_tolower` are `const int*` indexed the same way as the mask table.
char ctype<char>::do_toupper(char c) const {
    return static_cast<char>(_M_toupper[static_cast<unsigned char>(c)]);
}

const char *ctype<char>::do_toupper(char *lo, const char *hi) const {
    while (lo < hi) {
        *lo = do_toupper(*lo);
        ++lo;
    }
    return hi;
}

char ctype<char>::do_tolower(char c) const {
    return static_cast<char>(_M_tolower[static_cast<unsigned char>(c)]);
}

const char *ctype<char>::do_tolower(char *lo, const char *hi) const {
    while (lo < hi) {
        *lo = do_tolower(*lo);
        ++lo;
    }
    return hi;
}

// The two caches libstdc++'s inline `widen` and `narrow` fill on first use. For
// `char` both conversions are the identity, so the tables are their own indices —
// which is what the C locale means by widening a `char` to a `char`.
void ctype<char>::_M_widen_init() const {
    for (size_t i = 0; i < sizeof(_M_widen); ++i) {
        _M_widen[i] = static_cast<char>(i);
    }
    _M_widen_ok = 1;
}

void ctype<char>::_M_narrow_init() const {
    for (size_t i = 0; i < sizeof(_M_narrow); ++i) {
        _M_narrow[i] = static_cast<char>(i);
    }
    _M_narrow_ok = 1;
}

namespace {

// The one `ctype<char>` in the system, built once into static storage.
//
// Placement new rather than a plain `static ctype<char>`, because `~ctype()` is
// protected — a facet is meant to be destroyed by the locale that owns it and by
// nothing else, so the language will not let a static object of this type be
// declared. Constructing in place says the same thing the access specifier does:
// this object is never destroyed.
//
// `refs` is 1 rather than 0, which is the other half of it: with 0 the reference
// count starts at zero and the first locale to install the facet would take
// ownership and free it.
alignas(ctype<char>) unsigned char classic_ctype_storage[sizeof(ctype<char>)];

ctype<char> &classic_ctype() {
    static ctype<char> *facet = new (classic_ctype_storage) ctype<char>(nullptr, false, 1);
    return *facet;
}

// Storage shaped like a `std::locale`, never constructed.
//
// `locale`'s constructor is out-of-line in libstdc++ and would drag `_Impl` in with
// it. `classic()` has to return a reference to *something*, and this is a reference
// to storage whose only property is its address — the `use_facet` below ignores it,
// which is the whole reason this is enough.
//
// Anything that reads through the returned reference — `name()`, a copy, an
// equality test — fails at the link naming the member it needed. That is the
// boundary of this section, and it fails loudly rather than reading a null `_M_impl`.
alignas(locale) unsigned char classic_locale_storage[sizeof(locale)] = {};

}  // namespace

const locale &locale::classic() {
    return *reinterpret_cast<const locale *>(classic_locale_storage);
}

}  // namespace std

// `use_facet<ctype<char>>`, defined under its mangled name.
//
// The obvious spelling is an explicit specialisation, and the compiler rejects it:
// `<bits/locale_facets.tcc>` already contains an explicit *instantiation
// declaration* for this exact specialisation, and specialising after that point is
// ill-formed — `explicit specialization of 'use_facet<std::ctype<char>>' after
// instantiation`. An explicit instantiation *definition* is accepted and is worse:
// it instantiates the generic body, which reads `loc._M_impl->_M_facets` and would
// dereference the null `_M_impl` of the storage above at the first call.
//
// So the definition is given the mangled name directly. The `asm` label is the whole
// trick and there is nothing hidden in it: `_ZSt9use_facetISt5ctypeIcEERKT_RKSt6locale`
// is what `const std::ctype<char>& std::use_facet<std::ctype<char>>(const
// std::locale&)` mangles to, which `c++filt` will confirm and which the failing link
// printed in full. Every caller reaches this function through that name, so binding
// it here binds all of them.
//
// The name is checked rather than trusted: `scripts/qt-link.sh` fails if it is
// wrong, because the symbol it is meant to satisfy is still undefined.
//
// It is at file scope and not in an unnamed namespace, which was the first attempt
// and produced an object file with no such symbol in it: internal linkage plus `-O1`
// is enough for the compiler to notice that nothing in *this* translation unit calls
// it and drop it. An `asm` label renames a symbol; it does not make one survive.
const std::ctype<char> &staros_use_facet_ctype_char(const std::locale &)
    __asm__("_ZSt9use_facetISt5ctypeIcEERKT_RKSt6locale");

// The locale argument is ignored, and that is the honest reading of a system with
// one locale rather than a shortcut: there is no second `ctype<char>` for a
// different `locale` to select.
const std::ctype<char> &staros_use_facet_ctype_char(const std::locale &) {
    return std::classic_ctype();
}
