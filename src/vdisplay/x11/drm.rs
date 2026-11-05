//! What a DRM card says through its device node and not in sysfs: how many CRTCs it has, and which
//! encoders each connector can use. Read-only: no mode is probed, nothing is set.

use std::{io, path::Path};

/// A card, open.
pub struct Card(#[allow(dead_code)] std::fs::File);

impl Card {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let card = Self(file);
        // Whoever first opens a card that has no master becomes it (between sessions, or with
        // the desktop's X server switched away): given up at once, or that X server could fail to
        // take the card back meanwhile. Not being master also keeps a kernel from 5.14 on from
        // probing when asked for a connector.
        #[cfg(target_os = "linux")]
        card.ioctl(ioctl::DROP_MASTER, &mut ()).ok();
        Ok(card)
    }

    pub fn crtcs(&self) -> io::Result<usize> {
        #[cfg(target_os = "linux")]
        {
            let mut res = ioctl::CardRes::default();
            self.ioctl(ioctl::GET_RESOURCES, &mut res)?;
            return Ok(res.count_crtcs as usize);
        }
        #[cfg(not(target_os = "linux"))]
        Err(io::ErrorKind::Unsupported.into())
    }

    /// The encoders connector `id` can be driven by. Two connectors that share one are two ends
    /// of one port (DisplayPort++ makes a DP and an HDMI connector of each of its ports): only one
    /// of them can be on.
    pub fn encoders(&self, id: u32) -> io::Result<Vec<u32>> {
        #[cfg(target_os = "linux")]
        {
            // The count could change between the two calls, for a connector that comes and goes.
            // Room for one mode: asked with none, kernels before 5.14 probe the connector.
            let mut mode = ioctl::ModeInfo::default();
            for _ in 0..3 {
                let mut count = ioctl::GetConnector {
                    connector_id: id,
                    modes_ptr: &mut mode as *mut _ as u64,
                    count_modes: 1,
                    ..Default::default()
                };
                self.ioctl(ioctl::GET_CONNECTOR, &mut count)?;
                let mut encoders = vec![0u32; count.count_encoders as usize];
                if encoders.is_empty() {
                    return Ok(encoders);
                }
                let mut fill = ioctl::GetConnector {
                    connector_id: id,
                    encoders_ptr: encoders.as_mut_ptr() as u64,
                    count_encoders: encoders.len() as u32,
                    modes_ptr: &mut mode as *mut _ as u64,
                    count_modes: 1,
                    ..Default::default()
                };
                self.ioctl(ioctl::GET_CONNECTOR, &mut fill)?;
                if fill.count_encoders as usize <= encoders.len() {
                    encoders.truncate(fill.count_encoders as usize);
                    return Ok(encoders);
                }
            }
            return Err(io::ErrorKind::Interrupted.into());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    #[cfg(target_os = "linux")]
    fn ioctl<T>(&self, request: Option<u64>, arg: &mut T) -> io::Result<()> {
        use hbb_common::libc;
        use std::os::unix::io::AsRawFd;
        let Some(request) = request else {
            return Err(io::ErrorKind::Unsupported.into());
        };
        loop {
            // SAFETY: `request` is a DRM ioctl whose argument has the layout of `T`, which outlives
            // the call; the kernel writes nothing beyond it, the encoder array included, whose
            // length it is told.
            let r = unsafe { libc::ioctl(self.0.as_raw_fd(), request as _, arg as *mut T) };
            if r == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if !matches!(e.raw_os_error(), Some(libc::EINTR) | Some(libc::EAGAIN)) {
                return Err(e);
            }
        }
    }
}

// Built everywhere, for its test; used on Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod ioctl {
    // The kernel's uapi/drm/drm.h and drm_mode.h.
    #[repr(C)]
    #[derive(Default)]
    pub struct CardRes {
        pub fb_id_ptr: u64,
        pub crtc_id_ptr: u64,
        pub connector_id_ptr: u64,
        pub encoder_id_ptr: u64,
        pub count_fbs: u32,
        pub count_crtcs: u32,
        pub count_connectors: u32,
        pub count_encoders: u32,
        pub min_width: u32,
        pub max_width: u32,
        pub min_height: u32,
        pub max_height: u32,
    }

    /// struct drm_mode_modeinfo.
    #[repr(C)]
    #[derive(Default)]
    pub struct ModeInfo {
        pub clock: u32,
        pub timings: [u16; 10],
        pub vrefresh: u32,
        pub flags: u32,
        pub type_: u32,
        pub name: [u8; 32],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct GetConnector {
        pub encoders_ptr: u64,
        pub modes_ptr: u64,
        pub props_ptr: u64,
        pub prop_values_ptr: u64,
        pub count_modes: u32,
        pub count_props: u32,
        pub count_encoders: u32,
        pub encoder_id: u32,
        pub connector_id: u32,
        pub connector_type: u32,
        pub connector_type_id: u32,
        pub connection: u32,
        pub mm_width: u32,
        pub mm_height: u32,
        pub subpixel: u32,
        pub pad: u32,
    }

    // The generic encoding of ioctl numbers (asm-generic/ioctl.h), which x86, ARM, RISC-V and
    // LoongArch use; elsewhere (MIPS, PowerPC, SPARC) none are made, and nothing is asked.
    #[cfg(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    const fn request(write_read: bool, nr: u64, size: usize) -> Option<u64> {
        let dir = if write_read { 3 } else { 0 };
        Some((dir << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | nr)
    }

    #[cfg(not(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    const fn request(_: bool, _: u64, _: usize) -> Option<u64> {
        None
    }

    pub const DROP_MASTER: Option<u64> = request(false, 0x1F, 0);
    pub const GET_RESOURCES: Option<u64> =
        request(true, 0xA0, std::mem::size_of::<CardRes>());
    pub const GET_CONNECTOR: Option<u64> =
        request(true, 0xA7, std::mem::size_of::<GetConnector>());

    #[cfg(test)]
    #[test]
    fn requests_are_numbered_as_libdrm_numbers_them() {
        assert_eq!(std::mem::size_of::<CardRes>(), 64);
        assert_eq!(std::mem::size_of::<ModeInfo>(), 68);
        assert_eq!(std::mem::size_of::<GetConnector>(), 80);
        if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
            assert_eq!(DROP_MASTER, Some(0x641F));
            assert_eq!(GET_RESOURCES, Some(0xC040_64A0));
            assert_eq!(GET_CONNECTOR, Some(0xC050_64A7));
        }
    }
}
