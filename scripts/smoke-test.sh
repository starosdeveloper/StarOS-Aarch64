#!/usr/bin/env bash
# Boot the kernel across a matrix of QEMU machines and assert on its output.
#
# The host tests (`cargo ktest-host`) cover the portable logic; this covers the
# thing they can't — that the whole image actually *boots* and runs end to end on
# each shape of machine we claim to support. It builds one image, boots it under
# every config, and checks that the expected lines appear (and that failure
# signals do not), then exits non-zero if any assertion failed. That is what
# turns "it still works on GICv3, right?" from a manual grep into a gate.
#
# Each run is fed a newline on stdin so every EL0 task (including the UART-RX
# driver, which otherwise blocks forever waiting for input) finishes — only then
# does the pool return whole and the `every frame returned` check mean something.
#
# Deliberately no global `pkill`: a stray QEMU from a timed-out run once
# destabilised a host here, and `pkill -9` made it worse. Instead each run is
# bounded by `timeout -k` (SIGTERM, then SIGKILL after a grace), so a hang is
# contained to its own config without reaching for a blunt instrument.
#
# Usage: smoke-test.sh [--quick]   (--quick skips the slow 8-core run)

set -uo pipefail # NOT -e: run the whole matrix, then report, even if one fails.

QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

cd "$(dirname "$0")/.."
ROOT="$PWD"
LOGDIR="$(mktemp -d)"

# --- colours (only if stdout is a terminal) ----------------------------------
if [ -t 1 ]; then
    R='\e[31m'; G='\e[32m'; Y='\e[33m'; B='\e[1m'; Z='\e[0m'
else
    R=''; G=''; Y=''; B=''; Z=''
fi
say() { printf '%b\n' "$*"; }

# --- build the image once ----------------------------------------------------
say "${B}building kernel image…${Z}"
cargo kbuild >/dev/null 2>&1 || { say "${R}build failed${Z}"; cargo kbuild; exit 1; }

ELF="target/aarch64-unknown-none/debug/kernel"
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
[ -z "$objcopy" ] && objcopy="$(command -v llvm-objcopy || command -v aarch64-linux-gnu-objcopy || true)"
[ -z "$objcopy" ] && { say "${R}no llvm-objcopy (rustup component add llvm-tools)${Z}"; exit 1; }
IMAGE="$LOGDIR/kernel.img"
"$objcopy" -O binary "$ELF" "$IMAGE"
say "  image: $(wc -c <"$IMAGE") bytes"

# The `init` EL0 image the kernel's build script just produced. It goes into the
# initramfs as a file so the `SpawnImage` path has a real program to load.
INIT_ELF="$(ls -t target/aarch64-unknown-none/debug/build/kernel-*/out/init.elf 2>/dev/null | head -1)"

# --- build an initramfs to exercise the CPIO/initramfs path (2.4) ------------
# A real `cpio -o -H newc` archive with a known file; the smoke test asserts the
# kernel's bootstrap process unpacks it and reads that file's contents back. The
# greeting string is fixed so the assertion can match it verbatim.
INITRAMFS=""
GREETING="hello from the initramfs"
if command -v cpio >/dev/null 2>&1; then
    IRDIR="$LOGDIR/initramfs"
    mkdir -p "$IRDIR"
    printf '%s\n' "$GREETING" >"$IRDIR/greeting.txt"
    printf 'STAR OS 0.2.0\n'  >"$IRDIR/version"
    # A real ELF as a *file in the archive*, for the `SpawnImage` path: the device
    # manager finds it, hands the bytes to the kernel, and the kernel builds a
    # process out of them. It is the same program the kernel also has built in,
    # which is what makes the test cheap — but it arrives the other way round, and
    # the id it is seeded with is one only this path uses.
    # A subdirectory, and one below it. CPIO stores paths and not directories, so
    # these two members are the only thing that makes `docs` a directory — which is
    # the whole premise `opendir`/`readdir` are built on, and the reason the archive
    # needs a nested path in it to be tested at all.
    mkdir -p "$IRDIR/docs/deep"
    printf 'read me\n'   >"$IRDIR/docs/readme.txt"
    printf 'down here\n' >"$IRDIR/docs/deep/note.txt"
    MEMBERS="greeting.txt version docs/readme.txt docs/deep/note.txt"
    # The system font, where Qt's font database will look for it. One weight here
    # rather than all fourteen: the matrix boots seven machines and the archive is
    # copied into each, while what the assertions need is that a 130 KB binary file
    # comes back byte for byte through a 4 KB bounce buffer. `cargo krun` ships the
    # whole family — see qemu-run.sh, and the reason there.
    FONT="$ROOT/../IBM_Plex_Mono/IBMPlexMono-Regular.ttf"
    HAVE_FONT=""
    if [ -f "$FONT" ]; then
        mkdir -p "$IRDIR/fonts"
        cp "$FONT" "$IRDIR/fonts/IBMPlexMono-Regular.ttf"
        MEMBERS="$MEMBERS fonts/IBMPlexMono-Regular.ttf"
        HAVE_FONT=1
    fi
    HAVE_PROGRAM=""
    if [ -n "$INIT_ELF" ] && [ -f "$INIT_ELF" ]; then
        cp "$INIT_ELF" "$IRDIR/init.elf"
        MEMBERS="$MEMBERS init.elf"
        HAVE_PROGRAM=1
    fi
    INITRAMFS="$LOGDIR/initramfs.cpio"
    # shellcheck disable=SC2086
    ( cd "$IRDIR" && printf '%s\n' $MEMBERS | cpio -o -H newc --reproducible 2>/dev/null ) >"$INITRAMFS"
    say "  initramfs: $(wc -c <"$INITRAMFS") bytes ($(printf '%s\n' $MEMBERS | wc -l) files)"
else
    say "  ${Y}cpio not found — skipping the initramfs assertions${Z}"
fi
say ""

# --- assertion helpers (operate on the current run's $LOG) -------------------
PASS=0; FAIL=0
declare -a FAILED_CONFIGS
CURRENT=""; LOG=""

have()   { grep -qa -F -- "$2" "$1"; }
req()    { if have "$LOG" "$1"; then PASS=$((PASS+1)); say "    ${G}✓${Z} $1";
           else FAIL=$((FAIL+1)); say "    ${R}✗ MISSING: $1${Z}"; FAILED_CONFIGS+=("$CURRENT"); fi; }
forbid() { if have "$LOG" "$1"; then FAIL=$((FAIL+1)); say "    ${R}✗ PRESENT (must be absent): $1${Z}"; FAILED_CONFIGS+=("$CURRENT");
           else PASS=$((PASS+1)); say "    ${G}✓${Z} absent: $1"; fi; }

# The sleep *resolution* is deliberately not asserted anywhere, and the empty
# space is worth a note so it is not filled back in.
#
# Two attempts were made. Asserting it in EL0 measured the whole park/wake/schedule
# round trip and failed on a loaded four-core run while the wake-up itself was
# 2.3 ms late. Asserting the kernel's own measurement — the deadline-to-`Ready` gap,
# with no scheduling in it — failed too, on a *headless* config: the worst case
# depends on whether the sleep landed on the longest uninterruptible stretch in the
# kernel (zeroing 2.5 MiB of `.bss` per `Spawn`, or a framebuffer scroll), and under
# TCG that is a lottery. The same config produced 2.9 ms and 437 ms on different
# runs. The number is printed by the kernel and worth reading; a threshold on it
# would be a test of the emulator's luck. What *is* asserted is the part that is
# deterministic: EL0 never wakes before its deadline.

# run <name> <timeout-seconds> -- <qemu args...>
# Runs the image, then applies the assertions common to every machine. Config-
# specific assertions follow the call.
run() {
    CURRENT="$1"; local to="$2"; shift 3 # drop name, timeout, and the "--"
    LOG="$LOGDIR/$CURRENT.log"
    say "${B}▶ $CURRENT${Z}  (timeout ${to}s)"
    local t0 t1 rc
    t0=$(date +%s)
    printf '\n' | timeout -k 5 "$to" qemu-system-aarch64 -nographic -kernel "$IMAGE" "$@" >"$LOG" 2>&1
    rc=$?
    t1=$(date +%s)
    say "  exit=$rc in $((t1-t0))s  (log: $LOG)"
    if [ $rc -eq 124 ] || [ $rc -eq 137 ]; then
        FAIL=$((FAIL+1)); FAILED_CONFIGS+=("$CURRENT")
        say "    ${R}✗ TIMED OUT — no clean shutdown within ${to}s${Z}"
        return
    fi
    # Assertions every healthy boot must satisfy, whatever the machine:
    req "shutting down (PSCI SYSTEM_OFF)"          # reached the end, no hang/crash
    # No frame leak after teardown. The kernel prints one of three verdicts, and
    # only `LEAKED` is a failure: with no console input the UART driver is still
    # blocked in `Wait`, and the frames it holds are not lost. On a machine with
    # room to spare those tasks sit outside the longest free run and the verdict is
    # the strict one; on the 128 MiB configuration the pool is small enough that
    # they can land inside it, and demanding the strict verdict there tests where
    # the allocator happened to place a live task. `RECLAIM_STRICT=0` says so for
    # that machine; `forbid LEAKED` below carries the claim on every machine.
    if [ "${RECLAIM_STRICT:-1}" = 1 ]; then
        req "every frame returned"
    else
        req "frame reclaim: longest free run"
    fi
    RECLAIM_STRICT=1
    req "isolated, kernel continues"               # the canary EL0 fault was contained
    forbid "LEAKED"                                # the reclaim check did not fail
    forbid "did not initialise"                    # no half-configured device
    # IPC under contention: three senders and one receiver on a two-slot endpoint.
    # The receiver checks the sum of every sequence number it drained, so a message
    # lost or duplicated by the ring/wait-queue path prints MISMATCH instead — on
    # every machine in the matrix, single-core included.
    # The input driver comes up on every machine; whether it is *given* a device
    # depends on the machine, and claiming one that is not there is the failure
    # worth forbidding everywhere. The key-press half needs a synthesised event
    # and lives in `scripts/input-check.sh`.
    forbid "[inputsrv] that is not a virtio-input device"
    req "[ipc-storm] receiver drained every message"
    forbid "SEQUENCE SUM MISMATCH"
    # Demand-paged stacks: a task starts with ONE mapped stack page, walks 40 pages
    # down (each step a translation fault the kernel resolves and retries), reads
    # every marker back, then overruns the limit on purpose. Both halves are
    # asserted: the growth must work and the guard must still stop it.
    req "[stack] walked 40 pages down a stack that started with one mapped"
    forbid "MARKER MISMATCH"
    req "stack guard: growth limit reached"
    req "page(s) mapped on demand"
    # The monotonic clock, at both ends of the syscall boundary. The kernel holds
    # `ClockNow`'s scale against the interval its own tick source is armed with
    # (a mis-built scale breaks that equality and nothing else), and an EL0 task
    # holding no capabilities at all reads the clock twice and requires the second
    # reading to be strictly later.
    # Multi-page allocation, at both syscalls that do it. The heap run is checked
    # at its far end (a kernel honouring only the first page faults there instead),
    # runs must be handed out back to back, and a zero-page request must be an
    # error rather than a quiet page. The shared buffer's marker is written to and
    # read from its SECOND page, so a one-page mapping kills the server instead.
    # Sleeping against an absolute deadline, checked from both sides in EL0: waking
    # early means the deadline was ignored, and waking only after a full 100 ms tick
    # period means the kernel never re-aimed its timer. The final memtest line is
    # the other half — that task sleeps last, so a scheduler that counted a sleeping
    # task as "no work left" would end the run before it printed.
    # Sleeping against an absolute deadline, asserted from the side that can see
    # each half. EL0 checks only the lower bound — it must not wake early — because
    # an upper bound measured there times the whole park/wake/schedule round trip.
    # The kernel checks the resolution, from the gap between deadline and wake-up
    # with no scheduling in it: at or above half a tick period means the timer was
    # never re-aimed and the sleep just rode the next periodic tick.
    req "[client] SleepUntil: woke no earlier than its 20 ms absolute deadline"
    forbid "SLEEP WRONG"
    # The resolution itself is measured by the kernel and printed, not asserted —
    # see the note above the matrix for why a threshold on it cannot hold here.
    # Waiting on a *set* of sources with a deadline — the primitive an event loop
    # is built on. One line covers all three failures worth naming: the wrong
    # index (a wait that reports the first handle rather than the one that fired),
    # a wait that returns despite nothing signalling it, and — the race this was
    # written for — a signal arriving after a timed-out wait, which must be counted
    # rather than handed to a task that is no longer listening.
    req "[client] WaitAny: index 1 of 2 from the server's notification"
    forbid "WAITANY WRONG"
    # A thread in the creator's own address space: it writes through a page the
    # creator allocated (same address means the same frame — the whole difference
    # from `Spawn`), runs with its own `TPIDR_EL0`, and its exit must NOT tear the
    # shared space down. The last is checked by claiming 64 fresh pages afterwards
    # and re-reading the marker: if the thread's exit had freed the space, its page
    # tables would be back in the pool and handed straight out again.
    req "[client] SpawnThread: a thread in this very address space"
    forbid "THREAD WRONG"
    # That one line covers four failures: the wrong index, a wait that returns with
    # nothing to wake it, a signal after a timed-out wait going uncounted, and a
    # stale registration poisoning the *next* block (which surfaces as a sleep that
    # does not sleep — nowhere near notifications, which is why it is checked here).
    req "grew the heap by 16 MiB in 8 calls of 1024 pages"
    req "[memtest] MapAnon(0) refused"
    req "[client] read the marker from the SECOND page of a 2-page shared buffer"
    req "agrees with the tick interval"
    forbid "CLOCK SCALE WRONG"
    req "[client] monotonic clock: two ClockNow reads from EL0"
    forbid "CLOCK DID NOT ADVANCE"
}

# ---------------------------------------------------------------------------
# The matrix: axes are EL1/EL2 entry (virt vs virtualization=on), GICv2/v3,
# 1/4/8 cores, IOMMU present/absent, and a small-RAM run to exercise the memory
# map. `-cpu max` gives GICv3 + an SMMU; `cortex-a72` is our GICv2 baseline.
# ---------------------------------------------------------------------------

# Single core, so the bootstrap process's byte-at-a-time output stays contiguous
# and the initramfs greeting can be matched verbatim. Pass the archive if we built
# one; otherwise this is the plain no-initramfs run.
if [ -n "$INITRAMFS" ]; then
    run gicv2-el1-smp1 90 -- -M virt,gic-version=2 -cpu cortex-a72 -smp 1 -m 512M -initrd "$INITRAMFS"
    req "unpacked the initramfs in user space: $(printf '%s\n' $MEMBERS | wc -l) files, no storage driver"
    req "read 'greeting.txt' from the initramfs: $GREETING"
    # A *program* out of the same archive: user space parsed the CPIO, found an
    # ELF and handed the kernel its bytes. The loaded process is seeded with an id
    # nothing else uses, so its line can only have come from this path. The kernel
    # must also refuse what a loader gets handed by mistake — a file that is not a
    # program, and a pointer that is not memory (which it can only know by walking
    # the caller's tables; falsifying that check faults the kernel itself).
    if [ -n "$HAVE_PROGRAM" ]; then
        req "started 'init.elf' from the initramfs as a new process"
        req "[loaded] hello - my ELF was a file in the initramfs"
        req "refused a non-ELF file and an unmapped pointer"
        forbid "SPAWNIMAGE WRONG"
    fi
    # Files stop being one process's memory. The server holds the archive; the
    # client holds two endpoint capabilities and prints the file's bytes anyway.
    # The offset is asserted through the *content*: a server that ignored it would
    # return the head of the file twice and this line would read "hello hello from
    # the initramfs", which passes every other check here.
    req "[fssrv] the files are mine: $(printf '%s\n' $MEMBERS | wc -l) of them"
    req "[fsclient] read 'greeting.txt' through fssrv in 2 chunks: $GREETING"
    # The greeting plus its newline — computed, so changing the file's text does
    # not quietly turn this assertion into a comparison of two stale numbers.
    req "[fsclient] $((${#GREETING} + 1)) of $((${#GREETING} + 1)) bytes in 2 reads, the second one from offset 6"
    req "[fsclient] fssrv refused an unopened handle, a missing file, a closed handle and a lied-about length"
    # The lengths in a request are the client's to lie about, so the server takes
    # the size from the capability (`SharedPages`) instead: asked for a whole file
    # into one page, it must answer with one page and say what is left. Trusting the
    # number instead faults *the server* at 0x500001000.
    if [ -n "$HAVE_PROGRAM" ]; then
        req "into a 4096-byte buffer and got 4096, with"
    fi
    # And the proof that none of it was a shortcut: the client touches the address
    # the archive lives at *in the server* and the kernel kills it for that.
    req "EL0 fault at 0x900000000"
    forbid "I READ THE ARCHIVE DIRECTLY"
    # A program written in C, compiled by clang against crates/staros-libc: printf,
    # malloc, the clock and the file server, with every result checked inside the
    # program itself. The one line asserted here is the one it prints only if every
    # check passed — the individual FAIL lines are forbidden separately so a partial
    # failure cannot hide behind a missing summary.
    req "[hello-c] a C program in EL0"
    req "[hello-c] C RUNTIME OK - every check passed"
    forbid "[hello-c] FAIL"
    forbid "C RUNTIME BROKEN"
    # The mathematics, with the printed value being the one that separates a real
    # argument reduction from a naive one: glibc says sin(1e15) is 0.858273, and
    # Cody-Waite with two 33-bit halves of pi/2 says 0.833149.
    req "[hello-c] math: sin(1e15)=0.858273"
    # The calendar, which is UTC and says so. The date is fixed in the program, so
    # the whole line is asserted rather than a prefix.
    req "[hello-c] calendar: 2025-08-13 00:00:00 UTC (Wed)"
    # Pages rather than bytes, including the count of what munmap could not give
    # back — the kernel has no unmap syscall and this is where that shows.
    req "[hello-c] mmap: 12305 bytes mapped and returned, 16384 retained by the kernel"
    # The C program reaches the same file through the same server as fsclient, but
    # through open/read/lseek rather than raw IPC.
    req "[hello-c] read 'greeting.txt' through fssrv with libc's open/read/lseek: $GREETING"
    # The buffered layer on top of those: a stream reads a page ahead, so its
    # position and its descriptor's are different numbers, and every claim behind
    # this line is one where a stream that forgot to subtract its read-ahead gets a
    # different answer.
    req "[hello-c] FILE*: fgetc/ungetc/fgets/fread agree with ftell"
    # Fourteen ctype functions the sysroot had declared and nobody had written,
    # plus the assertion macro. Found by starting G6: the 282-symbol contract is
    # measured from what a Qt *link* leaves undefined, and a header is a different
    # demand — libstdc++'s <cctype> says `using ::isalpha;` and fails at compile
    # time in a file that has nothing to do with the mistake.
    req "[hello-c] ctype: fourteen classifications the header had promised and nobody had written"
    # And the rest of what starting G6 found declared and unwritten: qsort and
    # bsearch, rand, strdup, and the special functions. erf and tgamma had *looked*
    # implemented — they came from compiler_builtins' weak libm, which is a floor
    # under everything this tree writes and was invisible until the check learned to
    # count a weak definition apart from a real one.
    req "[hello-c] stdlib: qsort, bsearch, rand, strdup and the special functions this sysroot had only promised"
    # The system font, read the way FreeType will read it: 133796 bytes through a
    # 4 KiB bounce buffer, twice — once in 512-byte bites through FILE*, once as a
    # single 33-page read of the descriptor — and the two agree byte for byte. The
    # checksum is in the line, so a page delivered twice or a refill that came back
    # zeroed changes it. Everything above reads files that fit in one buffer, which
    # is why the descriptor's chunk loop had no coverage until this line existed.
    if [ -n "$HAVE_FONT" ]; then
        req "[hello-c] font: read 133796 bytes of IBM Plex Mono through fssrv, checksum 6016661948058288260"
    fi
    # Directories over a flat archive. `docs` has no entry in the CPIO — it exists
    # because `docs/readme.txt` does — and `docs/deep` must be listed once rather
    # than once per file inside it. The counts are the whole claim, so the whole
    # line is asserted.
    req "[hello-c] listed 'docs': 1 file, 1 directory, over a flat archive"
    # sendfile with an explicit offset: the bytes are the file's, copied to standard
    # output by the library rather than by the program, and the text that lands here
    # is the tail of the greeting.
    req "[hello-c] sendfile: from the initramfs"
    # Threads: four of them, each with its own thread pointer, sharing a counter
    # through a mutex whose critical section yields in the middle — a lock that does
    # nothing passes a plain `counter++` loop and fails this one.
    # Layer 7: the process and the system around it. The pid is measured and the
    # frame count depends on inlining, so only the fixed middle of the line is
    # asserted — `uname` naming this system and the stack limit being the one the
    # kernel actually enforces (the same 256 KiB the guard-page line above reports).
    req "[hello-c] process "
    req "uname StarOS 0.2.0, stack limit 256 KiB"
    req "[hello-c] threads: 4 workers x 250 increments = 1000"
    # The event-loop layer: a thread blocked in poll until another thread wrote to
    # an eventfd, a pipe carried bytes between them, and a timeout was waited out
    # rather than returned from. The number in the line is measured, so only the
    # prefix is asserted.
    req "[hello-c] poll: a thread slept on an eventfd and a pipe"
    # `select` too, which is `poll` wearing a worse interface and was written for
    # Qt's sake: `qcore_unix_p.h` includes <sys/select.h> unconditionally. Its two
    # awkwardnesses are the ones a compiling-but-untested version gets wrong — the
    # sets are rewritten in place, and `nfds` is the highest descriptor plus one.
    forbid "[hello-c] FAIL: an nfds that does not reach the descriptor excludes it"
    # The same loop, woken by the system's own primitive: an endpoint wrapped in a
    # descriptor, sharing a poll set with an eventfd, and a message from *another
    # process* ending the wait. This is the shape a Qt event dispatcher needs, and
    # the line only appears when the wake came from the message rather than the
    # 5-second timeout that guards the check.
    req "[hello-c] endpoint in poll: a message from another process woke the loop"
    # The other half of what a platform plugin needs: memory a display server can
    # read, allocated from C. Two buffers at two addresses is the property a window
    # system stands on — one fixed address is one surface.
    req "[hello-c] shared buffers: 16 KiB of surface, mapped at"
    # The phase's checkpoint: a C++ program with the real standard library — three
    # std::threads under a std::mutex, 68 std::strings through a reallocating
    # std::vector — plus the two halves of static initialisation. The constructor
    # line proves `.init_array` ran; the destructor line proves `__cxa_atexit` did,
    # and it is printed *after* main returned, so a runtime that forgets it loses
    # only that line and nothing else.
    req "[hello-cpp] a namespace-scope constructor ran before main"
    req "[hello-cpp] a static local was constructed on first use"
    req "[hello-cpp] C++ RUNTIME OK - 68 strings, 600 from three threads"
    req "[hello-cpp] the static local's destructor ran at exit"
    # The plugin's own calls, from the compiler the plugin is written in: a whole
    # screen's worth of shareable pixels in an RAII type, under -fno-exceptions
    # -fno-rtti. The size is the point — 1200 KiB is what a full-screen backing
    # store costs, and the kernel's shared-memory ceiling used to be 256 KiB.
    req "[hello-cpp] backing store: 1200 KiB for a whole 640x480 screen"
    # The containers whose out-of-line half this tree had to write: the red-black
    # tree's rebalancing, std::list's splice, the hash table's bucket growth. Qt
    # uses all four, and the C++ RUNTIME OK line above only appears if the four
    # hundred keys came back in order from three different insertion orders — a
    # rotation reversed in the wrong half corrupts the tree into a null dereference.
    forbid "[hello-cpp] FAIL"
    forbid "C++ RUNTIME BROKEN"
    # No screen on this machine, and the kernel says so instead of starting a Qt
    # program that would find no display server. The Qt assertions live with the
    # `ramfb` config, which is the only one that has a framebuffer to composite onto.
    req "no Qt program started: this machine has no framebuffer"
    # An EL0 fault now reports where it happened, not just that it did.
    req "[fault]   backtrace ("
else
    run gicv2-el1-smp1 90 -- -M virt,gic-version=2 -cpu cortex-a72 -smp 1 -m 512M
    req "no initramfs on this machine"
    # No archive is not a reason to leave clients blocked forever: the server still
    # runs and still answers, and the answer is "no such file".
    req "[fssrv] the kernel gave me no initramfs; every open will be refused"
    req "[fsclient] the server has no 'greeting.txt'"
fi
req "entered at EL1, running at EL1"
req "interrupt controller: GICv2 online"
req "no IOMMU on this machine"
req "privileged access never (PAN): not implemented on this CPU"  # cortex-a72 has no PAN

run gicv2-el2-smp4 90 -- -M virt,gic-version=2,virtualization=on -cpu cortex-a72 -smp 4 -m 256M
req "entered at EL2, running at EL1"
req "interrupt controller: GICv2 online"
req "no increments lost"
req "every core signalled"                   # wake IPI (SGI) reached every secondary

run gicv3-el2-smp4 90 -- -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 2G
req "entered at EL2, running at EL1"
req "interrupt controller: GICv3 online"
req "no increments lost"
req "every core signalled"                   # wake IPI (SGI) reached every secondary
# The IPC storm's other half: the messages were not merely correct, they were sent
# from more than one core. race_test makes this claim for the kernel's lock; this
# makes it for the endpoint's ring and block/wake path.
req "endpoint contended across cores"
req "privileged access never (PAN): enabled" # -cpu max implements FEAT_PAN; the whole
                                             # user-copy demo runs under it via LDTR/STTR

RECLAIM_STRICT=0  # see the note in `run`: 128 MiB is too small to place tasks by luck
run gicv3-el2-smp4-128m 90 -- -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 128M
req "interrupt controller: GICv3 online"     # smallest RAM: the memory-map path
req "no increments lost"

# Framebuffer console over ramfb. Asserts the fw_cfg round-trip succeeded (the
# kernel found etc/ramfb, wrote the config, and QEMU accepted it) and that the
# never-freed pixel buffer was accounted for (every frame still returns). The
# pixels themselves are proven separately, live, by the QMP screendump path; here
# we only need the cheap serial gate so a broken fw_cfg driver fails CI.
# With the initramfs, unlike every other framebuffer boot before it. A Qt program
# needs both halves at once — a screen to composite onto *and* a filesystem to read
# its typeface from — and until this config carried an archive there was no machine
# in the matrix where the whole stack could run. The font assertion below is the one
# that could not exist without it.
run ramfb-el2-smp4 90 -- -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 512M -device ramfb ${INITRAMFS:+-initrd "$INITRAMFS"}
req "framebuffer: ramfb 640x480 online"
# The screen leaves the kernel: a process is handed the pixels, and a second
# process with nothing but two endpoint capabilities gets its surface onto a
# display it cannot touch. These are the *text* half of the claim — that the
# rectangle really lands where it was asked for is pixels, and only
# `scripts/fb-check.sh` can say so (it catches a server ignoring the client's
# coordinates, which every line here passes).
req "framebuffer: handed to displaysrv"
req "[displaysrv] the screen is mine"
req "[displaysrv] composited client surfaces onto a screen no client can touch"
# The window-system half: five surfaces from four clients, some overlapping, one
# restacked, one closed again and one taken down with the process that crashed, and
# commits that repainted only the rectangles they named. No keys are pressed here —
# that is input-check.sh — so the routing counters are zero and say so, which is a
# claim of its own: a server dropping keystrokes with nobody focused must admit it. The tally is exact — two full 64x64 commits and a raise at
# 4096 pixels each, 64 for the 8x8 damage, then the C program's 1024, its 16, its
# throwaway window's 1024 and the 1024 that repainted what closing it uncovered —
# so a server quietly repainting whole surfaces on every commit fails this line
# rather than merely being slow. All five refusals are provoked on purpose: a
# made-up surface id, a destroyed one, a surface claiming more pixels than its
# buffer holds, one with no buffer at all, and damage past the bottom edge. The
# third of those is what keeps a lying client from making the *server* fault.
# Ten commits and 255056 pixels, up from nine and 24656: the tenth is `qt-hello`'s,
# and the 230400 new pixels are three passes over its 320x240 window — 76800 each for
# the raise on show, the commit that carried the painted frame, and the repaint of
# what the window uncovered when it closed. The difference between "Qt started" and
# "Qt drew" is in that number, and it stays exact for the same reason the rest of the
# tally does: a compositor repainting whole surfaces per commit would overshoot it.
req "[displaysrv] 4 surface(s) live, 10 commit(s), 255056 pixel(s) composited, 8 refused, 1 client(s) reaped, 0 key(s) routed, 0 dropped for want of focus"
# A fourth client opened a window, asked the server to watch it, and crashed. The
# kernel signals the notification it delegated, the server takes its windows off
# the screen, and the count says it happened. Whether the *pixels* went back is
# fb-check.sh: this line would pass on a server that reaped in principle.
req "[dyingclient] a 32x32 window on screen, the server watching me, and now I crash"
req "[displaysrv] a client died; its windows are off the screen"
forbid "[dyingclient] WINDOW WRONG"
# And that a second *process* was served: the C program's window, drawn through the
# same header a platform plugin is given. Two clients on one server is where a
# shared reply endpoint shows itself — as one of them hanging on an answer the
# other took — so this line is also the routing assertion.
req "[hello-c] window: a 48x48 surface on a 640x480 screen, double buffered, from C through staros.h"
req "[fbclient] asked the screen its size (640x480 xRGB8888)"
forbid "[fbclient] SURFACE WRONG"
forbid "SURFACE WRONG"
# The single-core font self-test runs before SMP/tasks, so it must appear on a
# machine with a framebuffer — and proves the line-atomic DebugWrite path is wired.
req "[selftest] SINGLE THREAD TEST PASSED"

# Qt, from the top of the stack down: the platform plugin was found without a loader
# (it is linked in and registered by `Q_IMPORT_PLUGIN`, because `dlopen` refuses
# here), a window reached the compositor, and the raster engine painted into a buffer
# the display server reads. `platform=staros` rather than merely "a platform": Qt
# falling back to `offscreen` would run the same event loop and paint into memory
# nobody ever sees, printing much the same lines on the way.
req "[qt-hello] QGuiApplication constructed, platform=staros"
# The font is named because `drawText` against an empty database draws nothing and
# says nothing. `IBM Plex Mono` is the family in the initramfs, resolved by the
# plugin's own database and read whole over IPC from `fssrv` — so this line fails if
# the database goes back to searching the *build host's* directories, which is what
# the stock one did.
req "[qt-hello] painted 320x240, text in 'IBM Plex Mono'"
# The loop ran, and it ended by itself. `exec returned 0` is the assertion that
# matters most: this program painted nothing and never returned for a whole day,
# because `fcntl(F_SETFL, O_NONBLOCK)` returned success and changed nothing, and Qt's
# dispatcher then blocked for ever draining a wake-up pipe that could not report
# `EAGAIN`. Both lines are needed — a loop that starts and a loop that finishes are
# different claims, and only the first one was ever true before.
req "[qt-hello] event loop tick 1"
req "[qt-hello] exec returned 0"

run smmu-el2-smp4 120 -- -M virt,gic-version=3,virtualization=on,iommu=smmuv3 -cpu max -smp 4 -m 2G -device edu
req "iommu: SMMUv3 at"
req "ENABLED"
req "bound the DMA buffer to StreamID"       # 2.3 enforcement path
req "translation enforced"                   # end-to-end: a real bus master (edu) is
                                             # translated to its mapped page, aborted elsewhere
# The SMMU's own account of the abort. Without STE.S2R the hardware still blocks the
# transaction but records nothing, and this line disappears while every other
# assertion above still passes — which is exactly why it is asserted separately.
req "iommu: fault record - F_TRANSLATION"
req "fault records drained"
forbid "UNEXPECTED"                          # a fault from another stream/address
forbid "event queue silent"
req "no increments lost"

if [ "$QUICK" -eq 0 ]; then
    run smmu-el2-smp8 200 -- -M virt,gic-version=3,virtualization=on,iommu=smmuv3 -cpu max -smp 8 -m 2G
    req "bound the DMA buffer to StreamID"
    req "8 cores x 20000 locked increments = 160000 (expected 160000) — no increments lost"
else
    say "\n${Y}(--quick: skipping the 8-core run)${Z}"
fi

# --- summary -----------------------------------------------------------------
say ""
say "${B}────────────────────────────────────────${Z}"
if [ $FAIL -eq 0 ]; then
    say "${G}${B}SMOKE TEST PASSED${Z} — $PASS assertions across the matrix"
    exit 0
else
    # De-duplicate the failing config names.
    uniq_failed="$(printf '%s\n' "${FAILED_CONFIGS[@]}" | sort -u | tr '\n' ' ')"
    say "${R}${B}SMOKE TEST FAILED${Z} — $FAIL failed, $PASS passed"
    say "${R}  failing configs: ${uniq_failed}${Z}"
    say "  logs are under: $LOGDIR"
    exit 1
fi
