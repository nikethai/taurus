//! What the download needs from the operating system: free space and the
//! file system's limits before it starts, the "downloaded from the
//! internet" mark once it is done.

pub mod marks;
pub mod space;

pub use marks::mark_downloaded;
pub use space::{FsKind, check_fits, check_fits_with, fs_kind};
