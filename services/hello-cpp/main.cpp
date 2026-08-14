// hello-cpp — the checkpoint the roadmap calls the most underrated one in the
// whole plan: a C++ program in EL0 with `std::vector`, `std::string`,
// `std::thread`, a function-local static with a destructor, and `printf`.
//
// No Qt, no graphics. If this does not run, Qt will not run either, and nobody
// will know why — which is the entire reason it exists as its own program.
//
// Everything here is the *real* standard library: these are libstdc++'s headers,
// compiled against this tree's own C headers, linked against this tree's own libc
// and the C++ runtime in `crates/staros-libc/cxx/runtime.cpp`. Nothing is a
// look-alike written for the demo.

#include <algorithm>
#include <cstdio>
#include <cstring>
#include <list>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>

// The header a platform plugin is handed. It has been compiled as C since it was
// written, and a QPA plugin is C++ — under `-fno-exceptions -fno-rtti -std=c++17`,
// which is exactly how this file is built. A header that only ever met one of the
// two compilers is a header that half works, and the half that fails is the one
// nobody has tried.
#include <staros.h>

extern "C" {
unsigned long staros_heap_live_bytes(void);
}

namespace {

int failures;

void check(bool ok, const char *what) {
    if (!ok) {
        failures++;
        std::printf("[hello-cpp] FAIL: %s\n", what);
    }
}

/// An object with a destructor, built lazily inside a function and destroyed at
/// exit. Its two halves prove different things: construction on first use means
/// `__cxa_guard_acquire` worked, and the line printed at exit means `__cxa_atexit`
/// ran the destructor — which is the part a runtime silently skips.
class Ledger {
public:
    Ledger() : built_(true) { std::printf("[hello-cpp] a static local was constructed on first use\n"); }

    ~Ledger() {
        // Printed during exit, after `main` returned. If this line is missing the
        // destructor never ran, and every RAII type in the program is a lie.
        std::printf("[hello-cpp] the static local's destructor ran at exit, holding %d entries\n",
                    count_);
    }

    void add() { count_++; }
    bool built() const { return built_; }

private:
    bool built_;
    int count_ = 0;
};

Ledger &ledger() {
    static Ledger instance;
    return instance;
}

/// A namespace-scope object, which is the other kind: constructed from
/// `.init_array` before `main` runs at all.
///
/// The constructor prints, and that is not decoration. A constructor that only
/// assigned a constant would be folded into a `.data` initialiser by the compiler,
/// `.init_array` would be empty, and the check below would pass on a runtime that
/// never ran a single constructor — which is exactly what happened the first time
/// this was written.
struct Beacon {
    Beacon() {
        std::printf("[hello-cpp] a namespace-scope constructor ran before main\n");
        constructed = true;
    }
    static bool constructed;
};
bool Beacon::constructed = false;
Beacon beacon;

/// A backing store, the way a plugin would own one: pixels a display server can
/// read, held by a C++ object with a destructor.
///
/// This program holds **no capabilities**, which is the claim it exists to make, so
/// it never talks to the display server — `staros_shared_create` needs no
/// authority, exactly as `malloc` does not. What is being proved is narrower and
/// still worth proving: that the plugin's calls compile and link from C++ with
/// exceptions and RTTI off, and that a buffer sized for a real window is a thing
/// this system can hand out.
class BackingStore {
public:
    BackingStore(int width, int height)
        : width_(width), height_(height),
          cap_(staros_shared_create(static_cast<size_t>(width) * height * 4)) {
        if (cap_ != 0) {
            pixels_ = static_cast<unsigned int *>(staros_shared_map(cap_));
        }
    }

    // Copying would give two objects one buffer and one of them would eventually
    // hand it back twice. There is no unmap here to get wrong yet, and saying so in
    // the type is cheaper than remembering it.
    BackingStore(const BackingStore &) = delete;
    BackingStore &operator=(const BackingStore &) = delete;

    ~BackingStore() {
        // Nothing to release: this system has no unmap, and the pages stay. Saying
        // so where the release would go beats an empty destructor that reads like
        // an oversight — and beats a `free` that would be a lie.
    }

    bool valid() const { return cap_ != 0 && pixels_ != nullptr; }
    size_t bytes() const { return staros_shared_bytes(cap_); }
    unsigned int *pixels() const { return pixels_; }
    int width() const { return width_; }
    int height() const { return height_; }

    /// Fill a rectangle, the way `QPainter` eventually will: row by row, inside a
    /// buffer whose stride is its width because the client's pixels are packed.
    void fill(int x, int y, int w, int h, unsigned int colour) {
        // A store that never got its pages is not a store to draw into. Without
        // this a failed allocation becomes a null write and the program dies three
        // lines later, where the report says "EL0 fault" and not "no memory".
        if (!valid()) {
            return;
        }
        for (int row = y; row < y + h && row < height_; row++) {
            for (int col = x; col < x + w && col < width_; col++) {
                pixels_[row * width_ + col] = colour;
            }
        }
    }

private:
    int width_;
    int height_;
    unsigned int cap_;
    unsigned int *pixels_ = nullptr;
};

}  // namespace

int main() {
    std::printf("[hello-cpp] a C++ program in EL0: vector, string, thread, and a static with a destructor\n");

    check(Beacon::constructed, "a namespace-scope constructor ran before main");

    // ---- containers -------------------------------------------------------
    std::vector<std::string> words;
    words.push_back("initramfs");
    words.push_back("microkernel");
    words.push_back("capability");
    words.emplace_back("aarch64");
    // Force a reallocation: the vector must move its strings, and a string longer
    // than the small-string buffer must move its heap allocation with it.
    for (int i = 0; i < 64; i++) {
        words.push_back(std::string("filler-") + std::to_string(i) +
                        "-with-enough-text-to-leave-the-small-string-buffer");
    }
    check(words.size() == 68, "the vector grew to the size it was given");
    check(words[0] == "initramfs", "the first string survived the reallocations");
    check(words.back().size() > 32, "a long string kept its contents through a move");

    std::sort(words.begin(), words.end());
    check(std::is_sorted(words.begin(), words.end()), "std::sort ordered the strings");
    // Sorting compares strings byte by byte, which is where a signed `char` would
    // put every byte above 0x7f below the ASCII range — the bug the C library's
    // own tests cover, checked here through the layer that actually uses it.
    std::vector<std::string> mixed{"Ä", "A", "z"};
    std::sort(mixed.begin(), mixed.end());
    check(mixed[0] == "A" && mixed[2] == "Ä", "byte comparison is unsigned all the way up");

    // ---- threads ----------------------------------------------------------
    std::mutex guard;
    long total = 0;
    std::vector<std::thread> workers;
    for (int worker = 0; worker < 3; worker++) {
        workers.emplace_back([&guard, &total, worker] {
            for (int i = 0; i < 100; i++) {
                std::lock_guard<std::mutex> held(guard);
                total += worker + 1;
            }
        });
    }
    for (auto &worker : workers) {
        worker.join();
    }
    check(total == 100 * (1 + 2 + 3), "three std::threads added under one std::mutex");

    // A thread that returns a value through a captured variable, which is what
    // std::async would do with more machinery than this system needs yet.
    std::string built;
    std::thread scribe([&built, &words] { built = words.front() + "/" + words.back(); });
    scribe.join();
    check(built.find('/') != std::string::npos, "a thread built a string the main thread reads");

    // ---- the containers whose other half this tree wrote ------------------
    //
    // `std::map`, `std::set`, `std::list` and `std::unordered_map` are templates,
    // but the parts that do not depend on the element type are compiled once and
    // normally come from libstdc++.a: the red-black tree's rebalancing, the list's
    // splice, the hash table's bucket growth. Qt uses all four, so
    // `scripts/cxx-progress.sh` named them and `cxx/runtime.cpp` has them.
    //
    // The insertions below are *ascending*, which is the input a naive tree
    // degrades on and the one a real program supplies — a map of timestamps, of
    // file names, of ids. A rebalance written the wrong way round still produces a
    // container that answers correctly; it produces one shaped like a list, so the
    // check is on the shape as well as the answers.
    //
    // Three insertion orders, and the third is not decoration. The rebalance has
    // two mirror-image halves, and *ascending* keys reach only one of them: every
    // parent is a right child, so the branch for a parent that is a left child
    // never runs. Reversing a rotation in that half was falsified and nothing
    // failed — the test could not see it. Descending keys reach it; the
    // interleaved order reaches both within one tree, which is where the two
    // halves have to agree about the same nodes.
    {
        std::map<int, std::string> descending;
        for (int i = 400; i > 0; i--) {
            descending[i] = std::to_string(i);
        }
        check(descending.size() == 400, "four hundred descending keys");
        int previous_key = 0;
        for (const auto &entry : descending) {
            check(entry.first > previous_key, "and they come back ascending");
            previous_key = entry.first;
        }
        check(previous_key == 400, "all the way to the last");
    }
    {
        // Alternating outward from the middle: each insert lands on the opposite
        // side of the root from the one before, so both halves of the rebalance run
        // against the same tree.
        std::map<int, int> zigzag;
        for (int i = 0; i < 200; i++) {
            zigzag[200 + i] = i;
            zigzag[199 - i] = i;
        }
        check(zigzag.size() == 400, "four hundred keys inserted outward from the middle");
        int previous_key = -1;
        int seen_keys = 0;
        for (const auto &entry : zigzag) {
            check(entry.first > previous_key, "and the whole tree is still in order");
            previous_key = entry.first;
            seen_keys++;
        }
        check(seen_keys == 400 && previous_key == 399, "every one of them, to the last");
    }
    {
        std::map<int, std::string> ordered;
        for (int i = 0; i < 400; i++) {
            ordered[i] = std::to_string(i);
        }
        check(ordered.size() == 400, "a std::map took four hundred ascending keys");
        check(ordered[137] == "137", "and gives them back");
        check(ordered.begin()->first == 0 && ordered.rbegin()->first == 399,
              "in order, from both ends");

        // Iteration visits every key once and in order. This is what exercises
        // _Rb_tree_increment, including the step off the maximum onto the header —
        // the one that returns the wrong node if the header test is missing.
        int seen = 0;
        int previous = -1;
        for (const auto &entry : ordered) {
            check(entry.first > previous, "keys arrive strictly increasing");
            previous = entry.first;
            seen++;
        }
        check(seen == 400, "and every one of them arrives");

        // Erase half, alternating, which is the case that makes the delete-side
        // rebalance run — and erasing the smallest and largest is what moves the
        // header's own leftmost and rightmost pointers.
        for (int i = 0; i < 400; i += 2) {
            ordered.erase(i);
        }
        check(ordered.size() == 200, "half of them erased");
        check(ordered.begin()->first == 1 && ordered.rbegin()->first == 399,
              "and the ends followed");
        previous = -1;
        for (const auto &entry : ordered) {
            check(entry.first > previous, "what is left is still in order");
            previous = entry.first;
        }
    }
    {
        // `std::list`'s splice is the only user of _M_transfer, and a self-splice
        // is the case that corrupts a list rather than doing nothing.
        std::list<int> a{1, 2, 3};
        std::list<int> b{4, 5, 6};
        a.splice(a.end(), b);
        check(a.size() == 6 && b.empty(), "splice moved the whole list");
        check(a.front() == 1 && a.back() == 6, "and kept the order");
        a.reverse();
        check(a.front() == 6 && a.back() == 1, "reverse walks it both ways");
    }
    {
        // The hash table, past enough insertions to force several rehashes — which
        // is what _M_next_bkt and _M_need_rehash decide. Pointer-shaped keys on
        // purpose: a power-of-two bucket count keeps only the low bits, which for
        // aligned values are the alignment, and every entry lands in one bucket.
        std::unordered_map<unsigned long, int> table;
        for (unsigned long i = 0; i < 500; i++) {
            table[i * 64] = static_cast<int>(i);
        }
        check(table.size() == 500, "five hundred entries survived the rehashes");
        check(table.bucket_count() > 500, "and the table really grew");
        int found = 0;
        for (unsigned long i = 0; i < 500; i++) {
            auto it = table.find(i * 64);
            if (it != table.end() && it->second == static_cast<int>(i)) {
                found++;
            }
        }
        check(found == 500, "every one of them is findable");
    }

    // ---- the plugin's own calls, from C++ ---------------------------------
    // A 320x240 window's worth of pixels: 300 KiB, seventy-five pages, larger than
    // anything else this program allocates and the size a first backing store
    // actually is. Held in an RAII type, because that is how a plugin will hold it
    // and because `-fno-exceptions` does not take destructors away.
    {
        BackingStore store(640, 480);
        check(store.valid(), "a backing store's pixels, allocated and mapped from C++");
        if (store.valid()) {
            check(store.bytes() == 640 * 480 * 4, "and the kernel agrees about its size");
            check(store.pixels()[0] == 0, "shared pages arrive zeroed");

            store.fill(0, 0, store.width(), store.height(), 0x00203040u);
            store.fill(16, 16, 32, 32, 0x00FF8000u);
            check(store.pixels()[0] == 0x00203040u, "the background reached the first pixel");
            check(store.pixels()[17 * 640 + 17] == 0x00FF8000u,
                  "and the rectangle reached its own");
            check(store.pixels()[15 * 640 + 15] == 0x00203040u,
                  "the rectangle stopped where it was told");
            // The last pixel, because a buffer that is a page short passes every
            // check that only reads the beginning.
            check(store.pixels()[640 * 480 - 1] == 0x00203040u,
                  "and the last pixel of the last page is ours too");
            std::printf("[hello-cpp] backing store: %zu KiB for a whole 640x480 screen, "
                        "filled and read back from C++\n",
                        store.bytes() / 1024);
        }

        // And the size a Raspberry Pi 5 at 1080p actually needs: 8 MB, 2048 pages,
        // the ceiling the kernel allows. Measured rather than assumed, because
        // these frames are *physically contiguous* — the object holds one address —
        // so this can fail on a pool with far more than 8 MB free in pieces. Which
        // way it goes is the answer to whether shared memory has to stop being
        // contiguous, and guessing at that before the number exists is how a day
        // gets spent rebuilding something that worked.
        BackingStore full(1920, 1080);
        if (full.valid()) {
            full.fill(1919, 1079, 1, 1, 0x00112233u);
            check(full.pixels()[1920 * 1080 - 1] == 0x00112233u,
                  "the last pixel of an 8 MB buffer is mapped");
            std::printf("[hello-cpp] a 1920x1080 backing store: %zu KiB, contiguous, mapped whole\n",
                        full.bytes() / 1024);
        } else {
            std::printf("[hello-cpp] a 1920x1080 backing store was refused: "
                        "8 MB of contiguous frames were not there\n");
        }
    }

    // ---- the static local -------------------------------------------------
    check(ledger().built(), "the static local reports itself constructed");
    ledger().add();
    ledger().add();

    if (failures == 0) {
        std::printf("[hello-cpp] C++ RUNTIME OK - %zu strings, %ld from three threads\n",
                    words.size(), total);
    } else {
        std::printf("[hello-cpp] C++ RUNTIME BROKEN - %d checks failed\n", failures);
    }
    return failures;
}
