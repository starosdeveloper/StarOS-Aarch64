//! ARM SMMUv3 in-memory bit-layout — the part that is pure integer arithmetic.
//!
//! The SMMU enforces device DMA by walking structures the kernel builds in
//! ordinary memory: a **StreamTableEntry** (STE) per StreamID that says how to
//! translate that device's transactions, **stage-2 translation tables** that map
//! the addresses a device is allowed to emit, and **commands** posted to a queue
//! to invalidate cached configuration. All three are fixed bit-layouts defined
//! by the architecture; this crate is *only* those layouts — functions from an
//! address and a handful of attributes to the `u64` words the hardware reads.
//!
//! It lives apart from the MMIO driver (in `arch-aarch64`) on purpose: with no
//! bus master in QEMU to issue a transaction, the SMMU never actually *walks* an
//! STE we install, so a wrong bit there would pass a live run silently. Pinning
//! the layout with host tests against the spec is the real check. Bit positions
//! below cite the ARM SMMUv3 architecture specification (IHI 0070).
//!
//! Scope: **stage-2-only** translation (`STE.Config = 0b110`, stage-1 bypassed).
//! A device has no page tables of its own, so a single stage of translation —
//! kernel-owned, mapping exactly the buffer it was granted — is all that is
//! needed to confine it. Stage-1 nesting (a device with its own MMU context) is
//! not modelled.

#![no_std]

/// One StreamTableEntry is 64 bytes = eight 64-bit doublewords.
pub const STE_DWORDS: usize = 8;
/// One command is 16 bytes = two doublewords.
pub const CMD_DWORDS: usize = 2;

// --- STE.Config (bits [3:1] of dword0) ---------------------------------------
/// `Config = 0b110`: stage-1 bypass, **stage-2 translate**. With `V = 1` this is
/// a live STE that sends the device's addresses through our stage-2 tables.
const STE_CONFIG_S2_ONLY: u64 = 0b110;

// --- STE.S2TG (STE stage-2 granule, dword2 [47:46]) --------------------------
// NOTE: the STE granule encoding is NOT the TCR one — here 4 KiB = 0b00.
const S2TG_4K: u64 = 0b00;
// --- STE.S2SL0 (starting level, dword2 [39:38]) for a 4 KiB granule ----------
/// Start the stage-2 walk at level 0 (valid for a 40-bit IPA, `T0SZ = 24` — the
/// walk still begins at level 0 because the IPA exceeds 39 bits).
const S2SL0_LEVEL0: u64 = 0b10;
// Cacheability/shareability of the stage-2 table walk itself.
const S2_XR0_WB: u64 = 0b01; // Normal WB cacheable
const S2SH0_INNER: u64 = 0b11; // inner shareable

/// Parameters of the stage-2 translation regime an STE points at. Fixed for our
/// use (4 KiB granule, 40-bit IPA, level-0 start) but named so the STE builder
/// reads as the spec does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage2Regime {
    /// `T0SZ`: size of the input (IPA) region is `2^(64 - T0SZ)`. `24` ⇒ 40-bit.
    pub t0sz: u64,
    /// `S2PS`: output physical-address size (`0b100` = 44-bit, `0b101` = 48-bit).
    pub ps: u64,
    /// The VMID that tags this stream's stage-2 TLB entries.
    pub vmid: u16,
}

impl Stage2Regime {
    /// The regime we install for every bound stream: **40-bit IPA** (`T0SZ = 24`, so
    /// the walk still starts at level 0 and `stage2_page`'s 4-level table is right),
    /// **44-bit output** (`S2PS = 0b100`). The 40-bit input is roomy for a device's
    /// IOVA window while staying within the output address size QEMU's SMMU (and
    /// typical hardware) advertises — a 48-bit IPA (`T0SZ = 16`) is rejected as a
    /// bad STE because it exceeds the 44-bit OAS. Proven against a real bus master,
    /// not just the bit-layout tests below.
    #[must_use]
    pub const fn stage2(vmid: u16) -> Self {
        Self { t0sz: 24, ps: 0b100, vmid }
    }
}

/// Build a **stage-2-only** StreamTableEntry pointing at the stage-2 table whose
/// base physical address is `s2ttb`. The device is then confined to whatever that
/// table maps (see [`stage2_page`]); everything it maps nothing for aborts.
///
/// `s2ttb` must be aligned to the stage-2 table (at least 4 KiB); its low bits
/// are masked off into the reserved region of the field.
#[must_use]
pub fn stage2_ste(s2ttb: u64, regime: Stage2Regime) -> [u64; STE_DWORDS] {
    let mut ste = [0u64; STE_DWORDS];

    // dword0: V (bit 0) + Config (bits [3:1]). S1Fmt/S1ContextPtr stay 0 — there
    // is no stage 1.
    ste[0] = 1 | (STE_CONFIG_S2_ONLY << 1);

    // dword2: the stage-2 regime descriptor (the STE's built-in "VTCR").
    ste[2] = u64::from(regime.vmid)          // S2VMID   [15:0]
        | (regime.t0sz << 32)                // S2T0SZ   [37:32]
        | (S2SL0_LEVEL0 << 38)               // S2SL0    [39:38]
        | (S2_XR0_WB << 40)                   // S2IR0    [41:40]
        | (S2_XR0_WB << 42)                   // S2OR0    [43:42]
        | (S2SH0_INNER << 44)                // S2SH0    [45:44]
        | (S2TG_4K << 46)                    // S2TG     [47:46]
        | (regime.ps << 48)                  // S2PS     [50:48]
        | (1 << 51); // S2AA64 [51] = AArch64 stage-2 descriptor format

    // dword3: S2TTB. The field is bits [51:4] holding table-base bits [51:4]; a
    // 4 KiB-aligned base has zero low bits, so it drops straight in.
    ste[3] = s2ttb & S2TTB_MASK;

    ste
}

/// STE.S2TTB occupies dword3 bits [51:4]; the base must be at least 4 KiB aligned
/// so bits [11:0] are already zero.
const S2TTB_MASK: u64 = 0x000f_ffff_ffff_f000;

/// An **abort** STE — all zero, i.e. `V = 0`. A stream whose entry is invalid is
/// aborted by the SMMU. This is both the default-deny ground state and what we
/// write back on unbind.
#[must_use]
pub const fn abort_ste() -> [u64; STE_DWORDS] {
    [0u64; STE_DWORDS]
}

// --- Stage-2 translation-table descriptors (VMSAv8-64, 4 KiB granule) ---------
const DESC_VALID: u64 = 0b01;
const DESC_TABLE: u64 = 0b11; // table/page descriptor: bits[1:0] = 0b11
const DESC_ADDR_MASK: u64 = 0x0000_ffff_ffff_f000; // output/next-table bits [47:12]

// Stage-2 leaf (page) attributes.
const S2_MEMATTR_NC: u64 = 0b0101 << 2; // Normal Inner-NC Outer-NC (matches our DMA map)
const S2AP_RW: u64 = 0b11 << 6; // S2AP: device read+write
const S2_SH_INNER: u64 = 0b11 << 8; // ignored for NC memory, set for form's sake
const S2_AF: u64 = 1 << 10; // access flag (else an access faults)
const S2_XN: u64 = 1 << 54; // execute-never — a data buffer, never fetched from

/// A stage-2 **table** descriptor (levels 0–2) pointing at the next-level table
/// at `next_phys`. Stage-2 tables carry no hierarchical attributes, so this is
/// just the address plus the table tag.
#[must_use]
pub const fn stage2_table(next_phys: u64) -> u64 {
    (next_phys & DESC_ADDR_MASK) | DESC_TABLE
}

/// A stage-2 **page** descriptor (level 3) mapping input → `output_phys`,
/// read/write, non-cacheable (matching how the CPU maps a DMA buffer), and
/// execute-never. This is the only kind of leaf a confined device gets.
#[must_use]
pub const fn stage2_page(output_phys: u64) -> u64 {
    (output_phys & DESC_ADDR_MASK)
        | DESC_TABLE // bits[1:0] = 0b11 marks a valid page at level 3
        | S2_MEMATTR_NC
        | S2AP_RW
        | S2_SH_INNER
        | S2_AF
        | S2_XN
}

/// Is a descriptor a live (valid) entry? Bit 0 set.
#[must_use]
pub const fn descriptor_is_valid(desc: u64) -> bool {
    desc & DESC_VALID != 0
}

// --- Commands (posted to CMDQ) -----------------------------------------------
const CMD_CFGI_STE: u64 = 0x03;
const CMD_SYNC: u64 = 0x46;

/// `CMD_CFGI_STE`: invalidate the SMMU's cached copy of one StreamID's STE, so
/// the next transaction re-fetches the entry we just wrote. `Leaf = 1` because a
/// linear stream table has no upper levels to invalidate.
#[must_use]
pub const fn cmd_cfgi_ste(streamid: u32) -> [u64; CMD_DWORDS] {
    [CMD_CFGI_STE | ((streamid as u64) << 32), 1]
}

/// `CMD_SYNC` with no completion signal (`CS = 0`): the SMMU drains every command
/// queued before it, then advances `CMDQ_CONS`. We poll `CONS` to know the STE
/// write has taken effect.
#[must_use]
pub const fn cmd_sync() -> [u64; CMD_DWORDS] {
    [CMD_SYNC, 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Extract a bit field [hi:lo] from a dword — the tests read fields back the
    // way the hardware would, so a shifted constant fails loudly.
    fn field(word: u64, lo: u32, hi: u32) -> u64 {
        let width = hi - lo + 1;
        (word >> lo) & ((1u64 << width) - 1)
    }

    #[test]
    fn stage2_ste_places_every_field_where_the_spec_says() {
        let s2ttb = 0x4_8321_0000; // 4 KiB-aligned, > 32-bit to exercise high bits
        let ste = stage2_ste(s2ttb, Stage2Regime::stage2(7));

        // dword0: V=1, Config=0b110 (stage-2 only).
        assert_eq!(field(ste[0], 0, 0), 1, "V");
        assert_eq!(field(ste[0], 1, 3), 0b110, "Config");

        // dword2: the stage-2 regime.
        assert_eq!(field(ste[2], 0, 15), 7, "S2VMID");
        assert_eq!(field(ste[2], 32, 37), 24, "S2T0SZ = 40-bit IPA");
        assert_eq!(field(ste[2], 38, 39), 0b10, "S2SL0 = start level 0");
        assert_eq!(field(ste[2], 46, 47), 0b00, "S2TG = 4 KiB (STE encoding)");
        assert_eq!(field(ste[2], 48, 50), 0b100, "S2PS = 44-bit");
        assert_eq!(field(ste[2], 51, 51), 1, "S2AA64");

        // dword3: S2TTB = the table base, aligned.
        assert_eq!(ste[3], s2ttb, "S2TTB");

        // Untouched dwords stay zero (no stage-1, no fault overrides).
        assert_eq!(ste[1], 0);
        assert_eq!(ste[4], 0);
        assert_eq!(ste[7], 0);
    }

    #[test]
    fn low_bits_of_a_misaligned_s2ttb_are_masked_off() {
        // A caller must pass an aligned base; if bits leak in, they must not land
        // in the field (they belong to reserved/other bits).
        let ste = stage2_ste(0x4_8321_0FFF, Stage2Regime::stage2(0));
        assert_eq!(ste[3], 0x4_8321_0000);
    }

    #[test]
    fn abort_ste_is_invalid() {
        let ste = abort_ste();
        assert_eq!(field(ste[0], 0, 0), 0, "V = 0 ⇒ abort");
        assert!(ste.iter().all(|&w| w == 0));
    }

    #[test]
    fn stage2_page_is_a_valid_rw_xn_leaf() {
        let out = 0x4_8100_0000;
        let d = stage2_page(out);
        assert!(descriptor_is_valid(d));
        assert_eq!(d & DESC_ADDR_MASK, out, "output address");
        assert_eq!(field(d, 0, 1), 0b11, "valid page");
        assert_eq!(field(d, 6, 7), 0b11, "S2AP = read/write");
        assert_eq!(field(d, 10, 10), 1, "access flag");
        assert_eq!(field(d, 54, 54), 1, "execute-never");
    }

    #[test]
    fn stage2_table_points_at_next_level() {
        let next = 0x4_8200_0000;
        let d = stage2_table(next);
        assert!(descriptor_is_valid(d));
        assert_eq!(d & DESC_ADDR_MASK, next);
        assert_eq!(field(d, 0, 1), 0b11, "table descriptor");
    }

    #[test]
    fn cfgi_ste_carries_the_streamid_and_opcode() {
        let c = cmd_cfgi_ste(0x1234);
        assert_eq!(field(c[0], 0, 7), 0x03, "opcode CMD_CFGI_STE");
        assert_eq!(field(c[0], 32, 63), 0x1234, "StreamID");
        assert_eq!(field(c[1], 0, 0), 1, "Leaf");
    }

    #[test]
    fn sync_has_no_completion_signal() {
        let c = cmd_sync();
        assert_eq!(field(c[0], 0, 7), 0x46, "opcode CMD_SYNC");
        assert_eq!(field(c[0], 12, 15), 0, "CS = 0 (poll CONS instead)");
    }
}
