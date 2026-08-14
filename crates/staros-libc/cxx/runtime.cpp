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
#include <list>
#include <map>
#include <new>
#include <string>
#include <thread>
#include <unordered_map>

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

}  // namespace std

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
