//! Error codes returned across the syscall boundary.

/// The result type used throughout the kernel and its ABI.
pub type KResult<T> = Result<T, KError>;

/// A stable, `repr(isize)` error code. Negative values are returned in a
/// register from syscalls; `Ok` values are non-negative. Keeping the
/// discriminants explicit means the numeric contract never shifts when a
/// variant is added.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(isize)]
pub enum KError {
    /// A supplied argument was outside its permitted range.
    InvalidArgument = -1,
    /// The named capability handle does not exist in the caller's space.
    BadHandle = -2,
    /// The caller lacks the rights required for this operation.
    PermissionDenied = -3,
    /// A resource (memory, handle slot, queue space) was exhausted.
    OutOfResources = -4,
    /// The operation would block and non-blocking behaviour was requested.
    WouldBlock = -5,
    /// The requested syscall number is not implemented.
    NoSuchSyscall = -6,
    /// The operation is meaningful but this machine cannot do it — e.g. firmware
    /// that declares no PSCI cannot start a second core or reset the board.
    NotSupported = -7,
}

impl KError {
    /// The raw register value used to signal this error from a syscall.
    #[must_use]
    pub const fn as_raw(self) -> isize {
        self as isize
    }
}
