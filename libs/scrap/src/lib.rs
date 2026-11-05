#[cfg(quartz)]
extern crate block;
#[macro_use]
extern crate cfg_if;
pub use hbb_common::libc;
#[cfg(dxgi)]
extern crate winapi;

pub use common::*;

#[cfg(quartz)]
pub mod quartz;

#[cfg(x11)]
pub mod x11;

#[cfg(all(x11, feature = "wayland"))]
pub mod wayland;

// Used on Linux; built on macOS too, for the tests of whoever uses it.
#[cfg(any(all(x11, feature = "wayland"), target_os = "macos"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub mod mutter_model;

#[cfg(dxgi)]
pub mod dxgi;

#[cfg(target_os = "android")]
pub mod android;

mod common;
