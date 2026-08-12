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
#include <mutex>
#include <string>
#include <thread>
#include <vector>

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
