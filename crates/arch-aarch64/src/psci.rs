//! PSCI: the only way to talk to the firmware that owns the cores.
//!
//! A phone's secondary cores are not ours to start. They are held in reset by
//! firmware, and the only door is the Power State Coordination Interface — a
//! calling convention over a single trapping instruction. The same interface is
//! also the machine's reset and power-off button, which is why
//! [`system_reset`] lands here rather than in some board file: on a device with
//! no power button you can reach, it is the difference between "reboot" and
//! "hold the button for ten seconds".
//!
//! # The conduit is not a constant
//!
//! The trapping instruction is either `hvc` or `smc`, and **which one is a
//! property of the boot, not of the machine**. QEMU `virt` is the proof: with no
//! EL2 present it declares `method = "hvc"`, because the firmware's PSCI sits at
//! the level an `hvc` traps to. Turn EL2 on (`-M virt,virtualization=on`) and the
//! *same machine* declares `method = "smc"` — EL2 is now the kernel's, so an
//! `hvc` from EL1 would trap into our own (nonexistent) EL2 vectors, and the
//! firmware moved up to EL3 where `smc` reaches it.
//!
//! So the conduit comes from the device tree, every time. Hard-coding it would
//! work in exactly one of those two configurations and hang in the other.

use core::arch::asm;
use core::cell::UnsafeCell;

use staros_abi::error::{KError, KResult};

/// Which instruction reaches the firmware's PSCI implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conduit {
    /// `hvc` — PSCI lives at EL2 (no hypervisor of our own).
    Hvc,
    /// `smc` — PSCI lives at EL3, above an EL2 we occupy.
    Smc,
}

impl Conduit {
    /// Parse a device tree `method` property. Returns `None` for anything else,
    /// which means we must not guess.
    #[must_use]
    pub fn from_dt(method: &str) -> Option<Self> {
        match method {
            "hvc" => Some(Self::Hvc),
            "smc" => Some(Self::Smc),
            _ => None,
        }
    }
}

/// The conduit this machine told us to use, or `None` until [`init`] runs.
struct ConduitSlot(UnsafeCell<Option<Conduit>>);
// SAFETY: written once by `init` during single-core early boot, read-only after.
unsafe impl Sync for ConduitSlot {}
static CONDUIT: ConduitSlot = ConduitSlot(UnsafeCell::new(None));

/// Record how to reach PSCI on this machine.
///
/// # Safety
/// Call once, during early boot on the primary core, before any other core or
/// interrupt can observe it.
pub unsafe fn init(conduit: Conduit) {
    // SAFETY: single-core early boot; this is the only writer.
    unsafe { *CONDUIT.0.get() = Some(conduit) };
}

/// The conduit, if the machine declared one.
#[must_use]
pub fn conduit() -> Option<Conduit> {
    // SAFETY: only mutated by `init` before anything else can read it.
    unsafe { *CONDUIT.0.get() }
}

/// Whether PSCI is usable — i.e. the device tree described it.
#[must_use]
pub fn is_available() -> bool {
    conduit().is_some()
}

// PSCI function ids (PSCI 0.2 and later). The `0xC4…` ones are the SMC64
// variants, which take 64-bit arguments — the only ones that can carry a
// physical entry point on this target.
const PSCI_VERSION: u32 = 0x8400_0000;
const CPU_OFF: u32 = 0x8400_0002;
const CPU_ON: u32 = 0xC400_0003;
const AFFINITY_INFO: u32 = 0xC400_0004;
const SYSTEM_OFF: u32 = 0x8400_0008;
const SYSTEM_RESET: u32 = 0x8400_0009;

/// PSCI return code: the call succeeded.
const SUCCESS: i64 = 0;
/// PSCI return code: this core is already on (a benign race, not a failure).
const ALREADY_ON: i64 = -4;

/// Make a PSCI call through whichever conduit the machine declared.
///
/// # Safety
/// The arguments must be valid for `func`. Some calls (`SYSTEM_OFF`,
/// `SYSTEM_RESET`, a successful `CPU_OFF`) do not return; `CPU_ON` starts another
/// core executing at `entry`, which is a machine-wide effect.
unsafe fn call(func: u32, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // SMCCC: x0..x3 carry the arguments and the result, and x4..x17 may come back
    // clobbered — hence the explicit clobber list. x18 and above are preserved by
    // the callee, so the compiler's assumptions hold across the call.
    match conduit() {
        // SAFETY: the caller's contract covers `func` and its arguments; the
        // conduit is the one the machine declared.
        Some(Conduit::Hvc) => unsafe {
            asm!(
                "hvc #0",
                inout("x0") u64::from(func) => ret,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                out("x4") _, out("x5") _, out("x6") _, out("x7") _,
                out("x8") _, out("x9") _, out("x10") _, out("x11") _,
                out("x12") _, out("x13") _, out("x14") _, out("x15") _,
                out("x16") _, out("x17") _,
                options(nostack),
            );
        },
        // SAFETY: as above.
        Some(Conduit::Smc) => unsafe {
            asm!(
                "smc #0",
                inout("x0") u64::from(func) => ret,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                out("x4") _, out("x5") _, out("x6") _, out("x7") _,
                out("x8") _, out("x9") _, out("x10") _, out("x11") _,
                out("x12") _, out("x13") _, out("x14") _, out("x15") _,
                out("x16") _, out("x17") _,
                options(nostack),
            );
        },
        // Callers check `is_available` first; this arm exists so that a missing
        // conduit can never turn into a trapping instruction aimed at whatever
        // happens to be listening.
        None => return KError::NotSupported.as_raw() as i64,
    }
    ret
}

/// The PSCI version the firmware implements, as `(major, minor)`.
///
/// # Errors
/// [`KError::NotSupported`] if the machine declared no PSCI.
pub fn version() -> KResult<(u16, u16)> {
    if !is_available() {
        return Err(KError::NotSupported);
    }
    // SAFETY: PSCI_VERSION takes no arguments and only reports a number.
    let v = unsafe { call(PSCI_VERSION, 0, 0, 0) };
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    Ok((((v >> 16) & 0xffff) as u16, (v & 0xffff) as u16))
}

/// Start the core with affinity `target_mpidr` at **physical** address `entry`,
/// passing `context_id` to it in `x0`.
///
/// `entry` must be physical because the waking core starts with its MMU off —
/// it has no idea the kernel is linked high, and every virtual address in the
/// image means nothing to it yet.
///
/// # Errors
/// [`KError::NotSupported`] if there is no PSCI, [`KError::InvalidArgument`] if
/// the firmware rejected the call. Already-on is reported as success: another
/// core beating us to it is not a failure.
pub fn cpu_on(target_mpidr: u64, entry: u64, context_id: u64) -> KResult<()> {
    if !is_available() {
        return Err(KError::NotSupported);
    }
    // SAFETY: `entry` is a physical address in this image and `target_mpidr` came
    // from the device tree's cpu list. Starting a core is the point.
    match unsafe { call(CPU_ON, target_mpidr, entry, context_id) } {
        SUCCESS | ALREADY_ON => Ok(()),
        _ => Err(KError::InvalidArgument),
    }
}

/// Whether the core with affinity `target_mpidr` is on: `Ok(true)` for on,
/// `Ok(false)` for off.
///
/// # Errors
/// [`KError::NotSupported`] if there is no PSCI, [`KError::InvalidArgument`] if
/// the firmware rejected the query.
pub fn affinity_info(target_mpidr: u64) -> KResult<bool> {
    if !is_available() {
        return Err(KError::NotSupported);
    }
    // SAFETY: a pure query; level 0 means "this exact affinity".
    match unsafe { call(AFFINITY_INFO, target_mpidr, 0, 0) } {
        0 => Ok(true),  // ON
        1 => Ok(false), // OFF
        _ => Err(KError::InvalidArgument),
    }
}

/// Turn the *calling* core off. Does not return if it succeeds.
///
/// # Errors
/// [`KError::NotSupported`] if there is no PSCI; otherwise this only returns if
/// the firmware refused, which it reports as [`KError::InvalidArgument`].
pub fn cpu_off() -> KResult<()> {
    if !is_available() {
        return Err(KError::NotSupported);
    }
    // SAFETY: the caller is asking for its own core to stop. Anything this core
    // still owns is the caller's problem, not this function's.
    unsafe { call(CPU_OFF, 0, 0, 0) };
    Err(KError::InvalidArgument)
}

/// Power the machine off. Returns only if PSCI is absent or refused.
pub fn system_off() {
    if is_available() {
        // SAFETY: takes no arguments; the machine stops.
        unsafe { call(SYSTEM_OFF, 0, 0, 0) };
    }
}

/// Reset the machine. Returns only if PSCI is absent or refused.
///
/// On a device whose buttons you cannot reach from a debugger, this is the
/// reboot button.
pub fn system_reset() {
    if is_available() {
        // SAFETY: takes no arguments; the machine restarts.
        unsafe { call(SYSTEM_RESET, 0, 0, 0) };
    }
}
