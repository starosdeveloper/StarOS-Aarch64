//! The kernel's global heap, wired to Rust's `alloc` via [`GlobalAlloc`].
//!
//! With this in place the kernel can use `Box`, `Vec` and friends instead of
//! fixed-size static arrays. The allocation *algorithm* lives in
//! [`staros_mm::heap`] (portable, host-tested); this module only serializes
//! access and holds the region.
//!
//! **The region is carved out of the RAM the device tree reported**, not baked
//! in as a static array, because its main consumer sizes itself from the machine
//! too: the frame allocator's tree costs
//! [`BuddyFrameAllocator::metadata_bytes`](staros_mm::BuddyFrameAllocator::metadata_bytes)
//! — 8 bytes per frame, i.e. 4 MiB to manage 2 GiB. A fixed heap would either
//! waste memory on a small machine or fail to manage a large one.
//!
//! Access is serialized by a [`SpinLock`], which is doing two jobs at once. Any
//! core may allocate, so the free list needs real mutual exclusion; and an
//! interrupt handler may allocate on a core that is already mid-update, which is
//! why the lock masks interrupts rather than only excluding other cores. Masking
//! alone was enough while one core existed, and stopped being enough the moment
//! a second one did.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

use staros_mm::heap::FreeListAllocator;

use crate::sync::SpinLock;

/// The global allocator: the portable free list plus the region it manages.
struct KernelHeap(SpinLock<FreeListAllocator>);

#[global_allocator]
static ALLOCATOR: KernelHeap = KernelHeap(SpinLock::new(FreeListAllocator::new()));

/// Hand the heap the region `[start, start + len)`, addressed as the *kernel*
/// sees it — these bytes get handed out to Rust code as `&mut` data, so `start`
/// is a kernel virtual address (the caller translates the physical run it carved
/// through `mmu::phys_to_virt`), not the physical address it was carved from.
///
/// # Safety
/// Must be called exactly once, before the first allocation, with a region that
/// is: mapped and writable (i.e. after `mmu::init` has covered it), owned by
/// nobody else (in particular excluded from the frame allocator), 16-byte
/// aligned, and a multiple of 16 bytes long.
pub unsafe fn init(start: usize, len: usize) {
    // SAFETY: called once during early boot; the region meets the allocator's
    // alignment and size contract per this function's own.
    unsafe {
        ALLOCATOR.0.lock().init(start as *mut u8, len);
    }
}

// SAFETY: `alloc`/`dealloc` uphold the `GlobalAlloc` contract — each returns a
// pointer to `layout`-sized, `layout`-aligned memory (or null), and `dealloc`
// only ever receives a pointer from a prior `alloc` with the same layout. The
// lock makes each update atomic against both other cores and this core's own
// interrupt handlers.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // The guard is exclusive access to the free list; `alloc` is safe once
        // you have it.
        let result = self.0.lock().alloc(layout);
        result.map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(nn) = NonNull::new(ptr) else { return };
        // SAFETY: `ptr`/`layout` come from a prior `alloc` per the contract.
        unsafe { self.0.lock().dealloc(nn, layout) };
    }
}
