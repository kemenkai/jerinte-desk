//! The Windows backend: displays of the amyuni IDD driver (`usbmmidd_v2`).
//!
//! Unplugging takes the newest monitor, there being no way to name one, and a display's modes are
//! read from the driver's table when it is plugged in. So there is one display at most, of an exact
//! size written to the table just before, among sizes around it that later resizes land on.

#[cfg(windows)]
mod backend;
#[cfg(windows)]
mod display;
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) mod plan;

#[cfg(windows)]
pub(super) use backend::{clean_leftover, new_backend};
#[cfg(windows)]
pub use backend::{ctrl_p, taking_over};
