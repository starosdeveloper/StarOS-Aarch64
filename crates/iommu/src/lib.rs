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
/// One event record is 32 bytes = four doublewords.
pub const EVT_DWORDS: usize = 4;

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

/// `STE.S2R` (dword2 bit 58) — **record** stage-2 faults in the event queue.
///
/// Aborting and *reporting* are separate switches. With `S2R` clear the SMMU still
/// terminates a forbidden transaction, but writes no event record: the fault log
/// stays empty and an abort is only visible as an absence. This bit is what makes a
/// blocked DMA legible (StreamID, address, reason). `S2S` (bit 57, *stall*) stays
/// clear — we terminate faulting transactions rather than stalling the device.
const STE_S2R: u64 = 1 << 58;

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
        | (1 << 51)  // S2AA64 [51] = AArch64 stage-2 descriptor format
        | STE_S2R; // S2R      [58] = record stage-2 faults in the event queue

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

// --- Event records (the SMMU's fault log) ------------------------------------
//
// When the SMMU aborts a transaction it does not merely drop it: it writes a
// 32-byte record into the event queue saying *which* stream faulted, at *which*
// address, and *why*. Without reading that queue an abort is only observable
// indirectly (a buffer that stayed untouched); with it, a fault is a named
// record — the difference between "DMA didn't land" and "StreamID 0x8 faulted on
// IOVA 0x202000, stage-2 translation, write". On a real board this is the
// difference between debugging and guessing.
//
// Layout below follows the SMMUv3 spec (IHI 0070, "Event queue recorded event
// types"); field positions match Linux's `EVTQ_*` masks in `arm-smmu-v3.c`.

/// Recorded event types we name. The numeric code is what lands in the record;
/// anything outside this list is reported as its raw code.
///
/// `C_*` are *configuration* errors (the kernel programmed something the SMMU
/// rejected); `F_*` are *faults* raised by a device transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// `0x01 F_UUT` — unsupported upstream transaction.
    Uut,
    /// `0x02 C_BAD_STREAMID` — StreamID outside the configured table.
    BadStreamId,
    /// `0x03 F_STE_FETCH` — external abort fetching the STE.
    SteFetch,
    /// `0x04 C_BAD_STE` — the STE itself is malformed.
    BadSte,
    /// `0x06 F_STREAM_DISABLED` — the stream's entry is invalid (default-deny).
    StreamDisabled,
    /// `0x0b F_WALK_EABT` — external abort during a translation-table walk.
    WalkEabt,
    /// `0x10 F_TRANSLATION` — no valid mapping for the address. The ordinary
    /// "device reached outside its buffer" fault.
    Translation,
    /// `0x11 F_ADDR_SIZE` — address outside the configured input size.
    AddrSize,
    /// `0x12 F_ACCESS` — access flag was clear.
    Access,
    /// `0x13 F_PERMISSION` — mapped, but not with the permissions asked for.
    Permission,
    /// Any other code, kept raw rather than guessed at.
    Other(u8),
}

impl EventKind {
    /// Decode the event-type field.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0x01 => Self::Uut,
            0x02 => Self::BadStreamId,
            0x03 => Self::SteFetch,
            0x04 => Self::BadSte,
            0x06 => Self::StreamDisabled,
            0x0b => Self::WalkEabt,
            0x10 => Self::Translation,
            0x11 => Self::AddrSize,
            0x12 => Self::Access,
            0x13 => Self::Permission,
            other => Self::Other(other),
        }
    }

    /// A short name for a boot log. `Other` reports as `"unknown"`; print the raw
    /// [`EventRecord::code`] alongside it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Uut => "F_UUT",
            Self::BadStreamId => "C_BAD_STREAMID",
            Self::SteFetch => "F_STE_FETCH",
            Self::BadSte => "C_BAD_STE",
            Self::StreamDisabled => "F_STREAM_DISABLED",
            Self::WalkEabt => "F_WALK_EABT",
            Self::Translation => "F_TRANSLATION",
            Self::AddrSize => "F_ADDR_SIZE",
            Self::Access => "F_ACCESS",
            Self::Permission => "F_PERMISSION",
            Self::Other(_) => "unknown",
        }
    }
}

/// One decoded entry from the event queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRecord {
    /// Raw event-type code, kept even when [`kind`](Self::kind) names it.
    pub code: u8,
    /// What the code means.
    pub kind: EventKind,
    /// The StreamID whose transaction faulted — *which device*.
    pub streamid: u32,
    /// The address the device emitted (the input address of the failed
    /// translation). For a stage-2-only stream this is the IOVA.
    pub address: u64,
    /// Stage-2 input address recorded separately by the SMMU (`IPA`, bits
    /// [51:12] of dword 3, already shifted back to an address). Zero when the
    /// record carries no IPA.
    pub ipa: u64,
    /// Was the faulting access a **read**? (`RnW`) — false means a write.
    pub read: bool,
    /// Did the fault occur at **stage 2**? (`S2`) — our streams are stage-2 only,
    /// so a translation fault from a bound stream should have this set.
    pub stage2: bool,
    /// Was the access to **privileged** memory (`PnU` = privileged, not user)?
    pub privileged: bool,
    /// Was it an **instruction** fetch (`InD`)? Data buffers are mapped XN, so a
    /// set bit here means a device tried to execute.
    pub instruction: bool,
}

/// Decode one 32-byte event record.
///
/// Returns `None` for an all-zero record — the queue is zeroed at bring-up, so a
/// zero record is an empty slot, never a real event (event code 0 is reserved).
#[must_use]
pub fn parse_event(rec: &[u64; EVT_DWORDS]) -> Option<EventRecord> {
    if rec.iter().all(|&w| w == 0) {
        return None;
    }
    let code = (rec[0] & 0xff) as u8;
    if code == 0 {
        return None;
    }
    Some(EventRecord {
        code,
        kind: EventKind::from_code(code),
        streamid: (rec[0] >> 32) as u32,
        address: rec[2],
        ipa: (rec[3] >> 12) << 12, // IPA field is [51:12] of dword 3
        read: rec[1] & (1 << 35) != 0,   // RnW
        stage2: rec[1] & (1 << 39) != 0, // S2
        privileged: rec[1] & (1 << 33) != 0, // PnU
        instruction: rec[1] & (1 << 34) != 0, // InD
    })
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
        assert_eq!(field(ste[2], 57, 57), 0, "S2S = terminate, do not stall");
        assert_eq!(field(ste[2], 58, 58), 1, "S2R = record faults in the event queue");

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

    // --- event records --------------------------------------------------------

    /// Build the record the SMMU would write for a stage-2 translation fault on a
    /// device *write* — exactly the shape our `edu` blocked path must produce.
    fn translation_fault(streamid: u32, iova: u64) -> [u64; EVT_DWORDS] {
        [
            0x10 | (u64::from(streamid) << 32), // F_TRANSLATION + SID
            1 << 39,                            // S2 set, RnW clear (a write)
            iova,                               // input address
            iova & !0xfff,                      // IPA field [51:12]
        ]
    }

    #[test]
    fn parses_a_stage2_write_translation_fault() {
        let e = parse_event(&translation_fault(0x8, 0x0020_2000)).expect("a real record");
        assert_eq!(e.code, 0x10);
        assert_eq!(e.kind, EventKind::Translation);
        assert_eq!(e.kind.name(), "F_TRANSLATION");
        assert_eq!(e.streamid, 0x8);
        assert_eq!(e.address, 0x0020_2000);
        assert_eq!(e.ipa, 0x0020_2000);
        assert!(e.stage2, "S2");
        assert!(!e.read, "a write, so RnW is clear");
        assert!(!e.instruction);
    }

    #[test]
    fn read_and_write_are_distinguished() {
        let mut rec = translation_fault(1, 0x1000);
        assert!(!parse_event(&rec).unwrap().read);
        rec[1] |= 1 << 35; // RnW
        assert!(parse_event(&rec).unwrap().read);
    }

    #[test]
    fn streamid_comes_from_the_high_half_not_the_code() {
        // A high StreamID must not bleed into the event code, and vice versa.
        let e = parse_event(&translation_fault(0xffff_0001, 0)).unwrap();
        assert_eq!(e.streamid, 0xffff_0001);
        assert_eq!(e.code, 0x10);
    }

    #[test]
    fn an_empty_slot_is_not_an_event() {
        assert_eq!(parse_event(&[0; EVT_DWORDS]), None);
        // A record whose only content is elsewhere still has no type: not an event.
        assert_eq!(parse_event(&[0, 0, 0xdead_beef, 0]), None);
    }

    #[test]
    fn every_named_code_round_trips_and_the_rest_stay_raw() {
        for code in [0x01u8, 0x02, 0x03, 0x04, 0x06, 0x0b, 0x10, 0x11, 0x12, 0x13] {
            let kind = EventKind::from_code(code);
            assert_ne!(kind, EventKind::Other(code), "code {code:#x} should be named");
            assert_ne!(kind.name(), "unknown");
        }
        // An unassigned code is preserved rather than guessed at.
        assert_eq!(EventKind::from_code(0x7f), EventKind::Other(0x7f));
        assert_eq!(EventKind::from_code(0x7f).name(), "unknown");
    }

    #[test]
    fn ipa_field_ignores_the_low_bits_the_spec_reserves() {
        // Bits [11:0] of dword 3 are not part of the IPA; they must not leak in.
        let rec = [0x10, 1 << 39, 0, 0x4_8123_4fff];
        assert_eq!(parse_event(&rec).unwrap().ipa, 0x4_8123_4000);
    }

    #[test]
    fn parser_never_panics_on_arbitrary_bit_patterns() {
        // Every single-bit record, plus all-ones: decoding is total.
        for bit in 0..256u32 {
            let mut rec = [0u64; EVT_DWORDS];
            rec[(bit / 64) as usize] = 1u64 << (bit % 64);
            let _ = parse_event(&rec);
        }
        let all_ones = parse_event(&[u64::MAX; EVT_DWORDS]).unwrap();
        assert_eq!(all_ones.code, 0xff);
        assert_eq!(all_ones.kind, EventKind::Other(0xff));
    }
}
