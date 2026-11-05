//! The EDIDs a connector without a monitor is lit with. Their one mode is small: a desktop that
//! turns the display on by itself, or picks a primary output by its preferred mode (KDE's
//! kscreen takes the largest), does nothing much with it, and the size asked for comes from a
//! mode the session adds through RandR. 1280x720 with CVT reduced blanking, 64 MHz, is also slow
//! enough for the DisplayPort link a connector without a sink is trained at, which the scaled
//! kind runs as it is. No physical size, as the vkms driver gives none: a desktop has no DPI to
//! scale itself by.

/// The name the EDID gives its monitor, to tell ours apart.
pub const MONITOR_NAME: &str = "RustDesk VD";

/// What the EDID makes of the sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sink {
    /// The base block alone: a DisplayPort or DVI monitor. amdgpu makes a forced connector with
    /// such an EDID a virtual sink, without any clock limit; with an HDMI block, an HDMI sink.
    Plain,
    /// An HDMI monitor, up to 340 MHz: Intel's and nouveau's HDMI ports take a sink without the
    /// HDMI block for DVI, up to 165 MHz.
    Hdmi,
    /// An HDMI 2.0 monitor, up to 600 MHz (scrambling over SCDC, which without a monitor fails
    /// quietly).
    Hdmi20,
}

impl Sink {
    /// Its file for the firmware loader.
    pub fn file_name(&self) -> &'static str {
        match self {
            Sink::Plain => "rustdesk-vd-plain.bin",
            Sink::Hdmi => "rustdesk-vd-hdmi.bin",
            Sink::Hdmi20 => "rustdesk-vd-hdmi20.bin",
        }
    }
}

/// EDID 1.4, with a CTA-861 extension for an HDMI sink.
pub fn generate(sink: Sink) -> Vec<u8> {
    let mut edid = base(if sink == Sink::Plain { 0 } else { 1 }).to_vec();
    if sink != Sink::Plain {
        edid.extend_from_slice(&cta(sink));
    }
    edid
}

fn base(extensions: u8) -> [u8; 128] {
    let mut e = [0u8; 128];
    e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Manufacturer "RDK", three letters of five bits each.
    let id = (letter(b'R') << 10) | (letter(b'D') << 5) | letter(b'K');
    e[8..10].copy_from_slice(&id.to_be_bytes());
    e[10..12].copy_from_slice(&0x5644u16.to_le_bytes()); // product "VD"
    e[17] = (2026u16 - 1990) as u8; // year of manufacture
    e[18] = 1; // version 1.4
    e[19] = 4;
    e[20] = 0xA0; // digital, 8 bits per colour
    // No size (e[21], e[22]): unknown.
    e[23] = 120; // gamma 2.2
    // sRGB; the first detailed timing is the preferred one; no continuous frequencies, so no
    // modes are made up from ranges.
    e[24] = 0x06;
    // sRGB chromaticity.
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // No established timings (35..38); no standard timings.
    for i in 0..8 {
        e[38 + i * 2] = 0x01;
        e[39 + i * 2] = 0x01;
    }
    e[54..72].copy_from_slice(&detailed_timing());
    e[72..90].copy_from_slice(&text_descriptor(0xFC, MONITOR_NAME));
    e[90..108].copy_from_slice(&dummy_descriptor());
    e[108..126].copy_from_slice(&dummy_descriptor());
    e[126] = extensions;
    e[127] = checksum(&e[..127]);
    e
}

// The HDMI blocks, nothing else: no audio (a virtual display must not turn up as a sound card),
// no video modes of its own.
fn cta(sink: Sink) -> [u8; 128] {
    // HDMI Licensing's block: OUI 00-0C-03, address 1.0.0.0, no deep colour, 340 MHz.
    const HDMI: [u8; 8] = [0x60 | 7, 0x03, 0x0C, 0x00, 0x10, 0x00, 0x00, (340 / 5) as u8];
    // HDMI Forum's: OUI C4-5D-D8, version 1, 600 MHz, SCDC present.
    const HDMI_FORUM: [u8; 8] = [0x60 | 7, 0xD8, 0x5D, 0xC4, 0x01, (600 / 5) as u8, 0x80, 0x00];
    let mut blocks = HDMI.to_vec();
    if sink == Sink::Hdmi20 {
        blocks.extend_from_slice(&HDMI_FORUM);
    }
    let mut e = [0u8; 128];
    e[0] = 0x02; // CTA extension
    e[1] = 0x03; // revision 3, which has data blocks
    e[2] = 4 + blocks.len() as u8; // where detailed timings would start: there are none
    e[3] = 0x00; // no underscan, audio or YCbCr; no native timings
    e[4..4 + blocks.len()].copy_from_slice(&blocks);
    e[127] = checksum(&e[..127]);
    e
}

// 1280x720@60 CVT-RB: 64.00 MHz; 1280 1328 1360 1440; 720 723 728 741; +hsync -vsync.
fn detailed_timing() -> [u8; 18] {
    let (h_active, h_blank, h_front, h_sync) = (1280u16, 160u16, 48u16, 32u16);
    let (v_active, v_blank, v_front, v_sync) = (720u16, 21u16, 3u16, 5u16);
    let clock = (6400u16).to_le_bytes(); // in 10 kHz units
    [
        clock[0],
        clock[1],
        (h_active & 0xFF) as u8,
        (h_blank & 0xFF) as u8,
        (((h_active >> 8) << 4) | (h_blank >> 8)) as u8,
        (v_active & 0xFF) as u8,
        (v_blank & 0xFF) as u8,
        (((v_active >> 8) << 4) | (v_blank >> 8)) as u8,
        (h_front & 0xFF) as u8,
        (h_sync & 0xFF) as u8,
        (((v_front & 0x0F) << 4) | (v_sync & 0x0F)) as u8,
        (((h_front >> 8) << 6) | ((h_sync >> 8) << 4) | ((v_front >> 4) << 2) | (v_sync >> 4))
            as u8,
        0, // no image size
        0,
        0,
        0, // no borders
        0,
        0x1A, // digital separate sync, +hsync, -vsync
    ]
}

fn text_descriptor(tag: u8, text: &str) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = tag;
    let mut body = [0x20u8; 13];
    let bytes = text.as_bytes();
    let n = bytes.len().min(13);
    body[..n].copy_from_slice(&bytes[..n]);
    if n < 13 {
        body[n] = 0x0A;
    }
    d[5..].copy_from_slice(&body);
    d
}

fn dummy_descriptor() -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = 0x10;
    d
}

fn letter(c: u8) -> u16 {
    (c - b'A' + 1) as u16
}

fn checksum(bytes: &[u8]) -> u8 {
    let sum = bytes.iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
    0u8.wrapping_sub(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sums_to_zero(block: &[u8]) -> bool {
        block.iter().fold(0u8, |s, b| s.wrapping_add(*b)) == 0
    }

    #[test]
    fn the_edid_is_well_formed() {
        for sink in [Sink::Plain, Sink::Hdmi, Sink::Hdmi20] {
            let e = generate(sink);
            assert_eq!(&e[..8], &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
            assert_eq!(e.len(), 128 * (1 + e[126] as usize), "{:?}", sink);
            assert!(e.chunks(128).all(sums_to_zero), "{:?}", sink);
            assert_eq!((e[18], e[19]), (1, 4));
            // "RDK", unpacked.
            let id = u16::from_be_bytes([e[8], e[9]]);
            let unpack = |shift: u16| (((id >> shift) & 0x1F) as u8 + b'A' - 1) as char;
            assert_eq!((unpack(10), unpack(5), unpack(0)), ('R', 'D', 'K'));
            // No size, no continuous frequencies.
            assert_eq!((e[21], e[22]), (0, 0));
            assert_eq!(e[24] & 1, 0);
        }
        assert_eq!(generate(Sink::Plain).len(), 128);
    }

    #[test]
    fn the_mode_is_1280x720_reduced_blanking() {
        let e = generate(Sink::Plain);
        let d = &e[54..72];
        let clock_khz = u16::from_le_bytes([d[0], d[1]]) as u32 * 10;
        let h_active = d[2] as u32 | ((d[4] as u32 >> 4) << 8);
        let h_blank = d[3] as u32 | ((d[4] as u32 & 0x0F) << 8);
        let v_active = d[5] as u32 | ((d[7] as u32 >> 4) << 8);
        let v_blank = d[6] as u32 | ((d[7] as u32 & 0x0F) << 8);
        assert_eq!((h_active, v_active), (1280, 720));
        assert_eq!((h_active + h_blank, v_active + v_blank), (1440, 741));
        assert_eq!(clock_khz, 64_000);
        // 60 Hz, near enough.
        let refresh = clock_khz as f64 * 1000.0 / (1440.0 * 741.0);
        assert!((refresh - 60.0).abs() < 0.1, "{}", refresh);
        // Front porch and sync: 1328-1360 and 723-728.
        assert_eq!((d[8], d[9], d[10] >> 4, d[10] & 0x0F), (48, 32, 3, 5));
        assert_eq!(&d[12..15], &[0, 0, 0]);
    }

    #[test]
    fn the_monitor_is_named() {
        let e = generate(Sink::Plain);
        let d = &e[72..90];
        assert_eq!(d[3], 0xFC);
        assert_eq!(&d[5..5 + MONITOR_NAME.len()], MONITOR_NAME.as_bytes());
        assert_eq!(d[5 + MONITOR_NAME.len()], 0x0A);
    }

    // As the kernel reads them (drm_edid.c): vendor blocks of a CTA extension of revision 3.
    fn vendor_blocks(ext: &[u8]) -> Vec<&[u8]> {
        assert_eq!((ext[0], ext[1]), (0x02, 0x03));
        let end = ext[2] as usize;
        let mut blocks = Vec::new();
        let mut i = 4;
        while i < end {
            let (tag, len) = (ext[i] >> 5, (ext[i] & 0x1F) as usize);
            if tag == 3 {
                blocks.push(&ext[i..i + 1 + len]);
            }
            i += 1 + len;
        }
        assert_eq!(i, end);
        blocks
    }

    #[test]
    fn hdmi_sinks_say_so_up_to_their_clock() {
        let e = generate(Sink::Hdmi);
        let blocks = vendor_blocks(&e[128..]);
        assert_eq!(blocks, vec![&[0x67, 0x03, 0x0C, 0x00, 0x10, 0x00, 0x00, 0x44][..]]);
        // Max TMDS clock, in 5 MHz.
        assert_eq!(blocks[0][7] as u32 * 5, 340);
        // No audio, no YCbCr, no native timings; no detailed timings.
        assert_eq!(e[128 + 3], 0);
        assert!(e[128 + e[128 + 2] as usize..255].iter().all(|b| *b == 0));
        let e = generate(Sink::Hdmi20);
        let blocks = vendor_blocks(&e[128..]);
        assert_eq!(blocks.len(), 2);
        assert_eq!(&blocks[1][1..4], &[0xD8, 0x5D, 0xC4]);
        assert_eq!(blocks[1][5] as u32 * 5, 600);
        assert_eq!(blocks[1][6] & 0x80, 0x80, "SCDC present");
    }
}
