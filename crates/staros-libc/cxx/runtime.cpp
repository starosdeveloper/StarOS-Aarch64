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
// Built with `-fno-exceptions -fno-rtti`, which is what the roadmap chose for the
// whole C++ side (Qt supports it as `QT_NO_EXCEPTIONS`). That choice is what keeps
// an unwinder, `.eh_frame` and `__cxa_throw` out of this file: a program that
// would have thrown calls one of the `__throw_*` helpers below, which report and
// stop instead of unwinding into a caller that has no idea how to unwind.

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <new>
#include <string>
#include <thread>

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
int __cxa_guard_acquire(unsigned char *guard) {
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

void __cxa_guard_release(unsigned char *guard) {
    __atomic_store_n(&guard[0], 1, __ATOMIC_RELEASE);
    __atomic_store_n(&guard[1], 0, __ATOMIC_RELEASE);
}

void __cxa_guard_abort(unsigned char *guard) {
    __atomic_store_n(&guard[1], 0, __ATOMIC_RELEASE);
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

}  // namespace std
