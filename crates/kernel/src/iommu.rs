//! The kernel's live SMMU handle and the policy around binding DMA to streams.
//!
//! The arch crate brings the SMMU up and hands back a [`Smmu`] mechanism; the
//! kernel owns exactly one, behind a lock, and decides *who* may bind *what*
//! (that gate is in [`syscall`](crate::syscall): a caller must hold device
//! authority). A machine without an SMMU simply never installs one, and every
//! bind returns [`KError::NotSupported`].
//!
//! We also remember each bound stream's stage-2 table root so those table frames
//! go back to the buddy allocator at shutdown — otherwise the `every frame
//! returned` check would (correctly) flag them as leaked.

use alloc::vec::Vec;

use staros_abi::error::{KError, KResult};
use staros_arch_aarch64::smmu::Smmu;

use crate::mem;
use crate::sync::SpinLock;

/// The one live SMMU, or `None` on a machine without one.
static SMMU: SpinLock<Option<Smmu>> = SpinLock::new(None);

/// Bound streams as `(streamid, stage-2 table root)`, kept only so the tables can
/// be reclaimed at teardown.
static BOUND: SpinLock<Vec<(u32, u64)>> = SpinLock::new(Vec::new());

/// Record the brought-up SMMU so later binds can reach it. Call once, at boot.
pub fn install(smmu: Smmu) {
    *SMMU.lock() = Some(smmu);
}

/// Bind a DMA buffer (physical base + page count) to a device `streamid`, so the
/// SMMU permits that device only those pages. The authority check lives in the
/// syscall layer; by here the caller is already entitled.
///
/// # Errors
/// [`KError::NotSupported`] if the machine has no IOMMU; otherwise whatever
/// [`Smmu::bind_stream`] reports (bad StreamID, out of frames, SMMU refused).
pub fn bind(streamid: u32, buf_phys: u64, buf_pages: usize) -> KResult<()> {
    bind_at(streamid, buf_phys, buf_phys, buf_pages)
}

/// Bind a DMA buffer to a device `streamid` with an explicit IOVA: the device sees
/// the buffer at `iova` (and the pages after it), which the SMMU relocates to
/// physical `buf_phys`. Used when a device cannot emit the buffer's physical address
/// directly (a narrow DMA width). [`bind`] is the `iova == buf_phys` case.
///
/// # Errors
/// As [`bind`].
pub fn bind_at(streamid: u32, iova: u64, buf_phys: u64, buf_pages: usize) -> KResult<()> {
    let mut guard = SMMU.lock();
    let smmu = guard.as_mut().ok_or(KError::NotSupported)?;
    // SMMU lock → frame lock, a straight line (nothing takes the SMMU lock while
    // holding the frame lock).
    let s2ttb = mem::with(|f| {
        // SAFETY: `smmu` is the brought-up SMMU; `f` allocates table frames from
        // the linear map; the buffer is a contiguous DMA run.
        unsafe { smmu.bind_stream_at(streamid, iova, buf_phys, buf_pages, f) }
    })?;
    drop(guard);

    // Remember it for reclaim. If the Vec cannot grow, we lose only the ability to
    // free this table at shutdown — never correctness — so don't fail the bind.
    let mut bound = BOUND.lock();
    if bound.try_reserve(1).is_ok() {
        bound.push((streamid, s2ttb));
    }
    Ok(())
}

/// Take the next fault record the SMMU logged, or `None` if it logged nothing (or
/// there is no SMMU).
///
/// This is what makes an abort *legible*: the record names the StreamID, the address
/// the device emitted, and the reason. Callers drain in a loop until `None`.
pub fn next_event() -> Option<staros_iommu::EventRecord> {
    let mut guard = SMMU.lock();
    let smmu = guard.as_mut()?;
    // SAFETY: the installed SMMU, and the lock makes this call non-re-entrant.
    unsafe { smmu.next_event() }
}

/// Free every bound stream's stage-2 table, returning those frames to the pool.
/// Call once at shutdown, before the frame-reclaim check.
pub fn reclaim() {
    // Take the list out from under its lock first, so we never hold BOUND and the
    // SMMU lock at once.
    let items: Vec<(u32, u64)> = {
        let mut bound = BOUND.lock();
        bound.drain(..).collect()
    };
    let mut guard = SMMU.lock();
    let Some(smmu) = guard.as_mut() else { return };
    for (streamid, s2ttb) in items {
        // SAFETY: `s2ttb` is the table this SMMU built for `streamid`, freed once.
        mem::with(|f| unsafe { smmu.unbind_stream(streamid, s2ttb, f) });
    }
}
