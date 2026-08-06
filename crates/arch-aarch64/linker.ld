/* Kernel image layout: linked high, loaded low.
 *
 * The kernel runs in the *upper* half of the virtual address space, out of
 * `TTBR1_EL1`, while `TTBR0_EL1` belongs entirely to whichever user task is
 * current. So every kernel symbol has two addresses that must not be confused:
 *
 *  - a **virtual** address (VMA) — where the code is linked and where it runs
 *    once translation is on: `KERNEL_VA_OFFSET + physical`;
 *  - a **load** address (LMA) — where the bootloader actually puts the bytes,
 *    which is all it knows about, since it runs with the MMU off.
 *
 * `AT(ADDR(section) - KERNEL_VA_OFFSET)` is what states the second: it makes
 * the ELF's program headers carry physical `p_paddr`s, so `llvm-objcopy -O
 * binary` lays the image out by physical address and a bootloader loading it at
 * `DRAM base + text_offset` puts every byte where the high VAs will point.
 *
 * `virt` places RAM at 0x4000_0000, so a physical base of 0x4008_0000 gives a
 * text_offset of 0x80000 — the value hard-coded in the image header in
 * `boot.rs`. If this address changes, that header must change with it.
 *
 * The kernel is *not* relocatable: `KERNEL_PHYS_BASE` is baked into every high
 * VA at link time. A board whose DRAM does not start at 0x4000_0000 needs this
 * number changed (a real Linux kernel relocates itself instead).
 */

ENTRY(_start)

/* Where the bootloader loads us. */
KERNEL_PHYS_BASE = 0x40080000;

/* Distance between a physical address and its address in the kernel's linear
 * map. Must equal `mmu::KERNEL_VA_OFFSET` — the boot stub builds the map from
 * that constant and then jumps to code linked against this one. */
KERNEL_VA_OFFSET = 0xFFFF000000000000;

/* Where the kernel is linked, and runs. */
KERNEL_VMA_BASE = KERNEL_VA_OFFSET + KERNEL_PHYS_BASE;

/* Size of the initial boot stack (64 KiB). */
STACK_SIZE = 0x10000;

SECTIONS
{
    . = KERNEL_VMA_BASE;
    __image_start = .;

    .text : AT(ADDR(.text) - KERNEL_VA_OFFSET) ALIGN(4K) {
        KEEP(*(.text.boot))   /* _start must come first */
        *(.text .text.*)
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VA_OFFSET) ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    .data : AT(ADDR(.data) - KERNEL_VA_OFFSET) ALIGN(4K) {
        *(.data .data.*)
    }

    .bss (NOLOAD) : AT(ADDR(.bss) - KERNEL_VA_OFFSET) ALIGN(4K) {
        __bss_start = .;
        /* The boot translation tables come first so they are easy to find in a
         * memory dump, but they are ordinary .bss: `_start` zeroes the section
         * before filling them in. The bounds are exported because `_start` has
         * to invalidate the D-cache over exactly this range before it turns
         * translation on — see boot.rs. */
        __boot_tables_start = .;
        KEEP(*(.bss.boot_tables))
        __boot_tables_end = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* Boot stack: reserve space and mark its top for `sp`. */
    .stack (NOLOAD) : AT(ADDR(.stack) - KERNEL_VA_OFFSET) ALIGN(16) {
        . += STACK_SIZE;
        __stack_top = .;
    }

    /* There is deliberately no frame pool here, and no user region either. The
     * frame pool went when the kernel started asking the device tree how much
     * memory the machine has (see `kernel::mem`); the user region went when
     * user programs became separately-linked ELFs loaded into address spaces of
     * their own (see `arch::addrspace`). Neither is something a linker script
     * can know. */
    . = ALIGN(4K);

    /* End of everything the kernel occupies at runtime. `image_size` in the
     * boot header is measured across this whole span, which is how the loader
     * is told to keep the device tree and initrd off our .bss and stack —
     * neither of which occupies space in the file. */
    __image_end = .;
    __image_size = __image_end - __image_start;

    /* Drop metadata we don't need in the raw image. */
    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
