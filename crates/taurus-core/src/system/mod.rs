//! What the download needs from the operating system: free space and the
//! file system's limits before it starts, the "downloaded from the
//! internet" mark once it is done; and what runs once the queue is done.

pub mod awake;
pub mod marks;
pub mod power;
pub mod space;

pub use awake::AwakeGuard;
pub use marks::mark_downloaded;
pub use power::command_for;
pub use space::{FsKind, check_fits, check_fits_with, fs_kind};
