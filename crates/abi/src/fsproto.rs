//! The file-server protocol, spoken between `services/fssrv` and its clients.
//!
//! It lives here rather than in the server because it is an ABI in exactly the
//! sense this crate exists for: two separately compiled programs have to agree on
//! it, and the last time a number like this was written down twice — the endpoint
//! ids in `kmain` and in `ipc.rs` — the two copies disagreed and a server spent its
//! life rejecting messages it had no business receiving.
//!
//! Bulk data travels through a shared buffer the *client* allocates and delegates
//! with the message; the words carry only sizes, offsets and handles.
//!
//! ```text
//! tag = 1 Open   words[0] = path length,  cap = buffer holding the path
//!                -> words[0] = handle, words[1] = size in bytes
//! tag = 2 Read   words[0] = handle, words[1] = offset, words[2] = max bytes,
//!                cap = buffer to fill
//!                -> words[0] = bytes written, words[1] = bytes left after them
//! tag = 3 Stat   words[0] = path length,  cap = buffer holding the path
//!                -> words[0] = size, words[1] = mode bits
//! tag = 4 Close  words[0] = handle
//! tag = 5 List   words[0] = index, cap = buffer to receive the name
//!                -> words[0] = name length, words[1] = size, words[2] = mode
//! tag = 9 Bye    the last client is done; the server may exit
//! ```
//!
//! A refusal is [`TAG_ERROR`] with one of the `ERR_*` codes in `words[0]`, never a
//! zero-length read: "the file is empty" and "there is no such file" are different
//! answers, and a client that cannot tell them apart will one day ship a blank
//! screen instead of an error.
//!
//! `List` takes an index rather than opening a directory handle, which makes it
//! stateless: the server holds nothing between calls, so a client that walks half a
//! directory and dies costs nothing, and two clients listing at once cannot see each
//! other's position. The price is that a listing is not a snapshot — but the archive
//! is read-only and never changes, so there is nothing to be inconsistent about.

/// Open a file by path.
pub const TAG_OPEN: u64 = 1;
/// Read from an open handle at an offset.
pub const TAG_READ: u64 = 2;
/// Size and mode of a path, without opening it.
pub const TAG_STAT: u64 = 3;
/// Release a handle.
pub const TAG_CLOSE: u64 = 4;
/// The name, size and mode of the archive's `index`-th member.
pub const TAG_LIST: u64 = 5;
/// No more requests are coming; the server may report and exit.
pub const TAG_BYE: u64 = 9;

/// A refused request. The reason is in `words[0]`.
pub const TAG_ERROR: u64 = 0;

/// No archive member by that name.
pub const ERR_NO_FILE: u64 = 1;
/// The handle names no open file — never opened, already closed, or reused.
pub const ERR_BAD_HANDLE: u64 = 2;
/// The request itself is wrong: unknown tag, missing buffer, unusable path.
pub const ERR_MALFORMED: u64 = 3;

/// The longest path the server will look at. Clients that build longer ones get a
/// refusal, so the bound belongs to both sides.
pub const MAX_PATH: usize = 128;
