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
    }

    /* Page-align onto a fresh frame so the loader can map the writable segment
     * with different (R-W) rights than the R-X text. `-z max-page-size=4096`
     * (build.rs) makes lld start the RW LOAD segment exactly here. */
    . = ALIGN(0x1000);

    .data : {
        *(.data .data.*)
    }

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.got*)
        *(.comment)
        *(.note*)
        *(.eh_frame*)
        *(.ARM.exidx*)
    }
}
