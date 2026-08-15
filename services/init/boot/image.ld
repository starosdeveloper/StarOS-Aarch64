/* Link layout for the `init` boot image (services/init/boot/image.rs).
 *
 * The kernel's ELF loader maps this program from USER_BASE = 0x8000_0000 (see
 * `staros_arch_aarch64::addrspace`), honouring each PT_LOAD segment's own R/W/X
 * rights. We link it there and let it use absolute virtual addresses. `_start`
 * is forced first so `e_entry` (= USER_BASE) lands on its first instruction.
 *
 * We deliberately produce two loadable segments so the loader is exercised on
 * real, differing permissions:
 *   - a read + execute segment (.text, with the inline greeting, and .rodata);
 *   - a read/write segment (.data + .bss), page-aligned onto its own frame.
 * `-z max-page-size=4096` (set by build.rs) keeps that alignment at 4 KiB, so the
 * writable segment lands at USER_BASE + 0x1000 rather than a 64 KiB page further
 * on, which would waste frames on an image this small.
 *
 * The image may grow to USER_IMAGE_END (a gigabyte); the kernel's own per-process
 * pages are placed gigabytes above it, not squeezed in just overhead. The
 * per-process id byte lives in one of them — the kernel-seeded data page at
 * USER_DATA_VA — and is not part of this image.
 */

ENTRY(_start)

USER_BASE = 0x80000000;

SECTIONS
{
    . = USER_BASE;

    .text : {
        KEEP(*(.text.start))   /* _start must be at offset 0 (== e_entry) */
        *(.text .text.*)
        *(.rodata .rodata.*)

        /* Read-only sections a library brings with a name of its own.
         *
         * A section this script does not mention is an *orphan*, and lld places
         * orphans by attribute rather than leaving them out: a read-only,
         * non-executable orphan goes ahead of `.text` and becomes a `PT_LOAD` of
         * its own, after which `.text` no longer starts on a page boundary and the
         * kernel's `map_segment` refuses the image. Naming them here puts them
         * inside this segment, where they cost nothing.
         *
         * `.qtmimedatabase` is Qt's compiled-in MIME table, read by
         * `QMimeDatabase`. Unlike `.gcc_except_table` below it cannot be discarded
         * — it is data the program reads.
         *
         * When a new library adds another, the failure is loud now: `qt-link.sh`
         * checks every `PT_LOAD`'s alignment and names the section it found. */
        *(.qtmimedatabase)
        *(.gnu.linkonce.r.*)
    }

    /* Page-align onto a fresh frame so the loader can map the writable segment
     * with different (R-W) rights than the R-X text. `-z max-page-size=4096`
     * (build.rs) makes lld start the RW LOAD segment exactly here. */
    . = ALIGN(0x1000);

    /* Thread-local storage.
     *
     * These two sections are the *template* every thread's TLS block is built
     * from: `.tdata` is copied into the block, `.tbss` is the zeroed tail. The
     * symbols around them are how `crates/staros-libc` finds the template at run
     * time — there is no dynamic loader here to hand it a `PT_TLS` header, and a
     * program that guessed the size would give every thread a block that is right
     * until somebody adds a variable.
     *
     * `.tbss` is NOBITS and occupies no file space, so `__tbss_end` measures the
     * block's *memory* size while `__tdata_end - __tdata_start` measures how much
     * of it is initialised.
     *
     * The 16-byte alignment matters: on AArch64 (TLS variant I) the thread pointer
     * points at a 16-byte TCB and the block starts right after it, so the linker's
     * `tprel` offsets assume that alignment. A TLS variable wanting more than 16
     * would shift them, and `services/hello-c` checks by value that it has not. */
    . = ALIGN(16);
    .tdata : {
        __tdata_start = .;
        *(.tdata .tdata.*)
        __tdata_end = .;
    }
    .tbss : {
        *(.tbss .tbss.*)
        *(.tcommon)
        __tbss_end = .;
    }

    /* Constructors, and the symbols the C runtime walks them with. A C program's
     * `.init_array` is empty; a C++ program's holds one entry per namespace-scope
     * object with a constructor, and a runtime that does not run them starts with
     * those objects unbuilt — a failure that shows up far from its cause. */
    .init_array : {
        __init_array_start = .;
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*)))
        KEEP(*(.init_array))
        __init_array_end = .;
    }

    /* The global offset table, and relocated read-only data.
     *
     * These used to be discarded, on the reasoning that a `-no-pie` program linked
     * at a fixed address needs no indirection. That is true of the *code* this tree
     * writes and false of C++: the compiler reaches some symbols through the GOT
     * regardless, and with the section discarded those `adrp`/`ldr` pairs resolved
     * to address zero. The symptom was a C++ thread faulting at 0x0 three frames
     * into `std::thread`, with nothing in the source anywhere near a null pointer.
     *
     * Keeping the GOT costs a few words and no run-time work: with static
     * relocations lld fills each entry with the final address at link time. */
    .got : {
        *(.got)
        *(.got.plt)
    }

    .data.rel.ro : {
        *(.data.rel.ro .data.rel.ro.*)
    }

    .data : {
        *(.data .data.*)
    }

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /* Unwinding metadata, all of it, and `.gcc_except_table` is the one that was
     * missed.
     *
     * `.eh_frame` describes how to walk the stack; `.gcc_except_table` is the
     * language-specific data beside it, saying which handler covers which range of
     * an unwind. Neither means anything here — this system builds `-fno-exceptions`
     * and `crates/staros-libc/cxx/runtime.cpp` stops the program at `__cxa_throw`
     * rather than unwinding — but the compiler still emits the tables for code it
     * inlines out of libstdc++'s headers.
     *
     * Leaving `.gcc_except_table` unnamed did not merely waste space. An unassigned
     * section is an *orphan*, and lld places orphans by attribute: read-only,
     * non-executable, so it went ahead of `.text` and became a `PT_LOAD` of its
     * own — 148 KiB of it in the Qt program. `.text` then began at `0x80024930`,
     * partway through that segment's last page, and the kernel's `map_segment`
     * requires a page-aligned `vaddr` and refuses anything else. The whole image
     * failed to load, and the only symptom was the Qt program never appearing.
     *
     * Every other program here has exactly two segments and never produced an
     * orphan large enough to need one, which is why the gap survived this long. */
    /DISCARD/ : {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
        *(.gcc_except_table*)
        *(.ARM.exidx*)
    }
}
