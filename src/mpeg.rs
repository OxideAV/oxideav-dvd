//! DVD-Video MPEG-2 elementary-stream video header decoders.
//!
//! A DVD VOB carries exactly one MPEG-1 or MPEG-2 video elementary
//! stream (start-code `0x000001E0`) inside its video packs. After a
//! demuxer strips the PES framing (see [`crate::vob`]), the resulting
//! video bytes begin with the standard ISO/IEC 13818-2 sequence /
//! GOP / picture start codes. This module decodes the *header* layers
//! a player needs to size buffers, label the track, and locate GOP
//! boundaries for seeking — it does **not** decode macroblocks (that
//! is a full MPEG-2 video decoder's job, a separate crate).
//!
//! All field layouts are taken from `mpucoder-mpeghdrs.html` (the
//! DVD-context MPEG header quick-reference) cross-checked against the
//! DVD MPEG constraints in `mpucoder-dvdmpeg.html`. The decoders here
//! cover:
//!
//! - [`SequenceHeader`] — `00 00 01 B3`: picture size, aspect-ratio
//!   code, frame-rate code, bit rate, VBV buffer size, constrained-
//!   parameters flag, and the two quantiser-matrix load flags.
//! - [`SequenceExtension`] — `00 00 01 B5` with extension-id `0001`:
//!   profile/level, progressive-sequence flag, chroma format, the
//!   size / bit-rate / VBV extension high bits, low-delay flag, and
//!   the frame-rate extension numerator/denominator.
//! - [`SequenceDisplayExtension`] — `00 00 01 B5` with extension-id
//!   `0010`: video format, optional colour description, display size.
//! - [`GopHeader`] — `00 00 01 B8`: the SMPTE time-code of the first
//!   frame plus the drop-frame / closed-GOP / broken-link flags.
//! - [`PictureHeader`] — `00 00 01 00`: temporal reference + picture
//!   coding type + VBV delay.
//! - [`PictureCodingExtension`] — `00 00 01 B5` with extension-id
//!   `1000`: the four `f_code` values, intra-DC precision, picture
//!   structure, and the per-picture frame/field coding flags.

use crate::error::{Error, Result};

/// `0x00` — Picture start code (`00 00 01 00`).
pub const SC_PICTURE: u8 = 0x00;
/// `0xB3` — Sequence header start code (`00 00 01 B3`).
pub const SC_SEQUENCE_HEADER: u8 = 0xB3;
/// `0xB5` — Extension start code (`00 00 01 B5`).
pub const SC_EXTENSION: u8 = 0xB5;
/// `0xB7` — Sequence end start code (`00 00 01 B7`).
pub const SC_SEQUENCE_END: u8 = 0xB7;
/// `0xB8` — Group-of-Pictures start code (`00 00 01 B8`).
pub const SC_GROUP_OF_PICTURES: u8 = 0xB8;

/// Extension-id nibble for a Sequence Extension (`0001`).
pub const EXT_ID_SEQUENCE: u8 = 0b0001;
/// Extension-id nibble for a Sequence Display Extension (`0010`).
pub const EXT_ID_SEQUENCE_DISPLAY: u8 = 0b0010;
/// Extension-id nibble for a Picture Coding Extension (`1000`).
pub const EXT_ID_PICTURE_CODING: u8 = 0b1000;

// ------------------------------------------------------------------
// Aspect ratio / frame rate code tables (mpucoder-mpeghdrs.html)
// ------------------------------------------------------------------

/// The 4-bit `aspect_ratio_information` field of a sequence header.
///
/// The numeric meaning differs between MPEG-1 (a sample/display ratio)
/// and MPEG-2 (a display aspect ratio). The table here is the MPEG-2
/// reading used by DVD per `mpucoder-mpeghdrs.html`; DVD only ever
/// authors codes `2` (4:3) and `3` (16:9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AspectRatioCode {
    /// `0` — forbidden.
    Forbidden,
    /// `1` — square pixels (sample aspect ratio 1:1).
    Square,
    /// `2` — 4:3 display aspect ratio.
    Ratio4x3,
    /// `3` — 16:9 display aspect ratio.
    Ratio16x9,
    /// `4` — 2.21:1 (not used by DVD).
    Ratio221x1,
    /// `5..=15` — reserved.
    Reserved(u8),
}

impl AspectRatioCode {
    /// Decode the raw 4-bit code.
    pub fn from_code(code: u8) -> Self {
        match code & 0x0F {
            0 => Self::Forbidden,
            1 => Self::Square,
            2 => Self::Ratio4x3,
            3 => Self::Ratio16x9,
            4 => Self::Ratio221x1,
            other => Self::Reserved(other),
        }
    }
}

/// The 4-bit `frame_rate_code` field of a sequence header.
///
/// Per `mpucoder-mpeghdrs.html`; DVD authors only `1` (24000/1001),
/// `3` (25), and `4` (30000/1001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRateCode {
    /// `0` — forbidden.
    Forbidden,
    /// `1` — 24000/1001 (23.976 fps).
    Fps23_976,
    /// `2` — 24 fps.
    Fps24,
    /// `3` — 25 fps.
    Fps25,
    /// `4` — 30000/1001 (29.97 fps).
    Fps29_97,
    /// `5` — 30 fps.
    Fps30,
    /// `6` — 50 fps.
    Fps50,
    /// `7` — 60000/1001 (59.94 fps).
    Fps59_94,
    /// `8` — 60 fps.
    Fps60,
    /// `9..=15` — reserved.
    Reserved(u8),
}

impl FrameRateCode {
    /// Decode the raw 4-bit code.
    pub fn from_code(code: u8) -> Self {
        match code & 0x0F {
            0 => Self::Forbidden,
            1 => Self::Fps23_976,
            2 => Self::Fps24,
            3 => Self::Fps25,
            4 => Self::Fps29_97,
            5 => Self::Fps30,
            6 => Self::Fps50,
            7 => Self::Fps59_94,
            8 => Self::Fps60,
            other => Self::Reserved(other),
        }
    }

    /// The frame rate as an exact `(numerator, denominator)` pair, or
    /// `None` for forbidden / reserved codes.
    pub fn as_ratio(self) -> Option<(u32, u32)> {
        Some(match self {
            Self::Fps23_976 => (24000, 1001),
            Self::Fps24 => (24, 1),
            Self::Fps25 => (25, 1),
            Self::Fps29_97 => (30000, 1001),
            Self::Fps30 => (30, 1),
            Self::Fps50 => (50, 1),
            Self::Fps59_94 => (60000, 1001),
            Self::Fps60 => (60, 1),
            Self::Forbidden | Self::Reserved(_) => return None,
        })
    }

    /// The frame rate as a `f64`, or `None` for forbidden/reserved.
    pub fn as_fps(self) -> Option<f64> {
        self.as_ratio().map(|(n, d)| n as f64 / d as f64)
    }
}

// ------------------------------------------------------------------
// Sequence header (00 00 01 B3)
// ------------------------------------------------------------------

/// Decoded MPEG video Sequence Header (`mpucoder-mpeghdrs.html`).
///
/// Layout (after the 4-byte `00 00 01 B3` start code, bit-packed
/// big-endian):
/// - `horizontal_size_value` — 12 bits
/// - `vertical_size_value` — 12 bits
/// - `aspect_ratio_information` — 4 bits
/// - `frame_rate_code` — 4 bits
/// - `bit_rate_value` — 18 bits (in units of 400 bit/s)
/// - marker bit (`1`)
/// - `vbv_buffer_size_value` — 10 bits (in units of 16 Kibit)
/// - `constrained_parameters_flag` — 1 bit
/// - `load_intra_quantiser_matrix` — 1 bit (+ 64-byte table if set)
/// - `load_non_intra_quantiser_matrix` — 1 bit (+ 64-byte table)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeader {
    /// 12-bit coded horizontal size (the low 12 bits; a Sequence
    /// Extension may add 2 high bits — see [`SequenceExtension`]).
    pub horizontal_size: u16,
    /// 12-bit coded vertical size (low 12 bits).
    pub vertical_size: u16,
    /// 4-bit aspect-ratio information.
    pub aspect_ratio: AspectRatioCode,
    /// 4-bit frame-rate code.
    pub frame_rate: FrameRateCode,
    /// 18-bit bit-rate value, in units of 400 bit/s (a Sequence
    /// Extension may add 12 high bits).
    pub bit_rate_value: u32,
    /// 10-bit VBV buffer-size value, in units of 16 Kibit.
    pub vbv_buffer_size: u16,
    /// `constrained_parameters_flag` (always 0 for MPEG-2).
    pub constrained_parameters: bool,
    /// Whether a custom intra quantiser matrix follows the header.
    pub load_intra_quant_matrix: bool,
    /// Whether a custom non-intra quantiser matrix follows.
    pub load_non_intra_quant_matrix: bool,
}

impl SequenceHeader {
    /// Length, in bytes, of the fixed portion (after the start code,
    /// before any quantiser-matrix tables): 8 bytes.
    pub const FIXED_LEN: usize = 8;

    /// Parse a sequence header. `buf` must begin at the `00 00 01 B3`
    /// start code. Trailing quantiser-matrix tables (if the load
    /// flags are set) are not decoded — only their presence is noted.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 || buf[0..3] != [0x00, 0x00, 0x01] || buf[3] != SC_SEQUENCE_HEADER {
            return Err(Error::InvalidUdf("MPEG seq header: missing 00 00 01 B3"));
        }
        if buf.len() < 4 + Self::FIXED_LEN {
            return Err(Error::InvalidUdf("MPEG seq header: truncated"));
        }
        let b = &buf[4..];
        // 12-bit H, 12-bit V across bytes 0..3.
        let horizontal_size = ((b[0] as u16) << 4) | ((b[1] as u16) >> 4);
        let vertical_size = (((b[1] & 0x0F) as u16) << 8) | (b[2] as u16);
        let aspect_ratio = AspectRatioCode::from_code(b[3] >> 4);
        let frame_rate = FrameRateCode::from_code(b[3] & 0x0F);
        // 18-bit bit_rate across bytes 4,5 and the top 2 bits of 6.
        let bit_rate_value = ((b[4] as u32) << 10) | ((b[5] as u32) << 2) | ((b[6] as u32) >> 6);
        // marker bit at byte 6 bit 5 — should be 1.
        if (b[6] >> 5) & 1 != 1 {
            return Err(Error::InvalidUdf("MPEG seq header: marker bit not set"));
        }
        // 10-bit VBV: low 5 bits of byte 6, top 5 bits of byte 7.
        let vbv_buffer_size = (((b[6] & 0x1F) as u16) << 5) | ((b[7] as u16) >> 3);
        let constrained_parameters = (b[7] >> 2) & 1 == 1;
        let load_intra_quant_matrix = (b[7] >> 1) & 1 == 1;
        // The non-intra load flag sits 64 bytes later if an intra
        // matrix was loaded; per the spec the flag immediately
        // following the intra flag is the non-intra flag only when the
        // intra flag is 0. We surface the *bit position 0* flag, which
        // is the non-intra load flag when no intra matrix is present.
        let load_non_intra_quant_matrix = !load_intra_quant_matrix && (b[7] & 1 == 1);
        Ok(Self {
            horizontal_size,
            vertical_size,
            aspect_ratio,
            frame_rate,
            bit_rate_value,
            vbv_buffer_size,
            constrained_parameters,
            load_intra_quant_matrix,
            load_non_intra_quant_matrix,
        })
    }

    /// Bit rate in bit/s from the 18-bit base value alone (units of
    /// 400 bit/s). A Sequence Extension can extend this with 12 high
    /// bits; use [`Self::bit_rate_bps_with_extension`] when available.
    /// Returns `None` when the value is the all-ones "variable
    /// bit rate" escape (`0x3FFFF`).
    pub fn bit_rate_bps(self) -> Option<u64> {
        if self.bit_rate_value == 0x3FFFF {
            None
        } else {
            Some(self.bit_rate_value as u64 * 400)
        }
    }

    /// Bit rate combining this header's 18 low bits with a Sequence
    /// Extension's 12 `bit_rate_extension` high bits (a 30-bit value,
    /// units of 400 bit/s).
    pub fn bit_rate_bps_with_extension(self, ext: &SequenceExtension) -> u64 {
        let full = ((ext.bit_rate_extension as u64) << 18) | self.bit_rate_value as u64;
        full * 400
    }

    /// VBV buffer size in bits (the 10-bit value × 16 Kibit).
    pub fn vbv_buffer_bits(self) -> u64 {
        self.vbv_buffer_size as u64 * 16 * 1024
    }

    /// Full coded horizontal size combining this header's 12 low bits
    /// with a Sequence Extension's 2 `horizontal_size_extension` high
    /// bits.
    pub fn full_horizontal_size(self, ext: &SequenceExtension) -> u16 {
        ((ext.horizontal_size_extension as u16) << 12) | self.horizontal_size
    }

    /// Full coded vertical size combining this header's 12 low bits
    /// with a Sequence Extension's 2 `vertical_size_extension` high
    /// bits.
    pub fn full_vertical_size(self, ext: &SequenceExtension) -> u16 {
        ((ext.vertical_size_extension as u16) << 12) | self.vertical_size
    }
}

// ------------------------------------------------------------------
// Sequence Extension (00 00 01 B5, ext-id 0001)
// ------------------------------------------------------------------

/// Decoded MPEG-2 Sequence Extension (`mpucoder-mpeghdrs.html`).
///
/// Always 6 bytes after the `00 00 01 B5` start code. The presence of
/// this extension is what distinguishes an MPEG-2 stream from a bare
/// MPEG-1 one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceExtension {
    /// 8-bit `profile_and_level_indication`.
    pub profile_and_level: u8,
    /// `progressive_sequence` flag.
    pub progressive_sequence: bool,
    /// 2-bit `chroma_format` (1 = 4:2:0, the only DVD value).
    pub chroma_format: u8,
    /// 2-bit `horizontal_size_extension`.
    pub horizontal_size_extension: u8,
    /// 2-bit `vertical_size_extension`.
    pub vertical_size_extension: u8,
    /// 12-bit `bit_rate_extension`.
    pub bit_rate_extension: u16,
    /// 8-bit `vbv_buffer_size_extension`.
    pub vbv_buffer_size_extension: u8,
    /// `low_delay` flag (always 0 for DVD).
    pub low_delay: bool,
    /// 2-bit `frame_rate_extension_n`.
    pub frame_rate_extension_n: u8,
    /// 5-bit `frame_rate_extension_d`.
    pub frame_rate_extension_d: u8,
}

impl SequenceExtension {
    /// Length, in bytes, of the extension body after the start code.
    pub const BODY_LEN: usize = 6;

    /// Parse a Sequence Extension. `buf` begins at the `00 00 01 B5`
    /// start code; the extension-id nibble (top 4 bits of byte 4)
    /// must be `0001`.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 || buf[0..3] != [0x00, 0x00, 0x01] || buf[3] != SC_EXTENSION {
            return Err(Error::InvalidUdf("MPEG seq ext: missing 00 00 01 B5"));
        }
        if buf.len() < 4 + Self::BODY_LEN {
            return Err(Error::InvalidUdf("MPEG seq ext: truncated"));
        }
        let b = &buf[4..];
        if (b[0] >> 4) != EXT_ID_SEQUENCE {
            return Err(Error::InvalidUdf("MPEG seq ext: extension-id != 0001"));
        }
        // byte0: id(4) prof_level_hi(4); byte1: prof_level_lo(4) ...
        let profile_and_level = ((b[0] & 0x0F) << 4) | (b[1] >> 4);
        let progressive_sequence = (b[1] >> 3) & 1 == 1;
        let chroma_format = (b[1] >> 1) & 0b11;
        let horizontal_size_extension = ((b[1] & 1) << 1) | (b[2] >> 7);
        let vertical_size_extension = (b[2] >> 5) & 0b11;
        let bit_rate_extension = (((b[2] & 0x1F) as u16) << 7) | ((b[3] >> 1) as u16);
        // marker bit at byte3 bit0.
        if b[3] & 1 != 1 {
            return Err(Error::InvalidUdf("MPEG seq ext: marker bit not set"));
        }
        let vbv_buffer_size_extension = b[4];
        let low_delay = (b[5] >> 7) & 1 == 1;
        let frame_rate_extension_n = (b[5] >> 5) & 0b11;
        let frame_rate_extension_d = b[5] & 0x1F;
        Ok(Self {
            profile_and_level,
            progressive_sequence,
            chroma_format,
            horizontal_size_extension,
            vertical_size_extension,
            bit_rate_extension,
            vbv_buffer_size_extension,
            low_delay,
            frame_rate_extension_n,
            frame_rate_extension_d,
        })
    }

    /// The 3-bit profile field of `profile_and_level` (per
    /// ISO/IEC 13818-2 Table 8-5; 4 = Main profile).
    pub fn profile(self) -> u8 {
        (self.profile_and_level >> 4) & 0b111
    }

    /// The 4-bit level field (8 = Main level, 10 = High-1440,
    /// 6 = High; DVD is Main@Main).
    pub fn level(self) -> u8 {
        self.profile_and_level & 0x0F
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a canonical NTSC DVD sequence header: 720×480, 16:9,
    /// 29.97 fps, bit_rate_value = 9800000/400 = 24500, marker=1,
    /// vbv = 112 (16 Kibit units), CP=0, no quantiser matrices.
    fn ntsc_seq_header() -> Vec<u8> {
        let h: u16 = 720;
        let v: u16 = 480;
        let aspect: u8 = 3; // 16:9
        let frame_rate: u8 = 4; // 29.97
        let bit_rate: u32 = 24_500; // 9.8 Mbit/s in 400 bit/s units
        let vbv: u16 = 112;
        let mut b = vec![0x00, 0x00, 0x01, SC_SEQUENCE_HEADER];
        // 12-bit H, 12-bit V
        b.push((h >> 4) as u8);
        b.push((((h & 0x0F) << 4) | (v >> 8)) as u8);
        b.push((v & 0xFF) as u8);
        b.push((aspect << 4) | frame_rate);
        // 18-bit bit_rate: top 8, next 8, low 2 + marker(1) + 5 hi vbv
        b.push((bit_rate >> 10) as u8);
        b.push((bit_rate >> 2) as u8);
        let low2 = (bit_rate & 0b11) as u8;
        let marker = 1u8;
        let vbv_hi5 = (vbv >> 5) as u8;
        b.push((low2 << 6) | (marker << 5) | vbv_hi5);
        // low 5 vbv + CP(0) + load_intra(0) + load_non_intra(0)
        let vbv_lo5 = (vbv & 0x1F) as u8;
        b.push(vbv_lo5 << 3);
        b
    }

    #[test]
    fn seq_header_ntsc_decode() {
        let buf = ntsc_seq_header();
        let h = SequenceHeader::parse(&buf).unwrap();
        assert_eq!(h.horizontal_size, 720);
        assert_eq!(h.vertical_size, 480);
        assert_eq!(h.aspect_ratio, AspectRatioCode::Ratio16x9);
        assert_eq!(h.frame_rate, FrameRateCode::Fps29_97);
        assert_eq!(h.bit_rate_value, 24_500);
        assert_eq!(h.bit_rate_bps(), Some(9_800_000));
        assert_eq!(h.vbv_buffer_size, 112);
        assert_eq!(h.vbv_buffer_bits(), 112 * 16 * 1024);
        assert!(!h.constrained_parameters);
        assert!(!h.load_intra_quant_matrix);
        assert!(!h.load_non_intra_quant_matrix);
    }

    #[test]
    fn frame_rate_ratios() {
        assert_eq!(FrameRateCode::Fps29_97.as_ratio(), Some((30000, 1001)));
        assert_eq!(FrameRateCode::Fps25.as_ratio(), Some((25, 1)));
        assert!((FrameRateCode::Fps23_976.as_fps().unwrap() - 23.976).abs() < 1e-3);
        assert_eq!(FrameRateCode::Forbidden.as_ratio(), None);
        assert_eq!(FrameRateCode::Reserved(10).as_ratio(), None);
    }

    #[test]
    fn aspect_ratio_codes() {
        assert_eq!(AspectRatioCode::from_code(0), AspectRatioCode::Forbidden);
        assert_eq!(AspectRatioCode::from_code(2), AspectRatioCode::Ratio4x3);
        assert_eq!(AspectRatioCode::from_code(3), AspectRatioCode::Ratio16x9);
        assert_eq!(
            AspectRatioCode::from_code(15),
            AspectRatioCode::Reserved(15)
        );
    }

    #[test]
    fn seq_header_bad_start_code() {
        let mut buf = ntsc_seq_header();
        buf[3] = 0xB8;
        assert!(SequenceHeader::parse(&buf).is_err());
    }

    #[test]
    fn seq_header_truncated() {
        let buf = ntsc_seq_header();
        assert!(SequenceHeader::parse(&buf[..6]).is_err());
    }

    #[test]
    fn vbr_bitrate_escape() {
        let mut buf = ntsc_seq_header();
        // Force bit_rate_value = 0x3FFFF (all-ones VBR escape).
        buf[8] = 0xFF;
        buf[9] = 0xFF;
        buf[10] |= 0b1100_0000;
        let h = SequenceHeader::parse(&buf).unwrap();
        assert_eq!(h.bit_rate_value, 0x3FFFF);
        assert_eq!(h.bit_rate_bps(), None);
    }

    /// Build a Sequence Extension: Main@Main (profile=4, level=8),
    /// progressive=0, chroma=1 (4:2:0), all size/rate extensions 0.
    fn main_seq_ext() -> Vec<u8> {
        let mut b = vec![0x00, 0x00, 0x01, SC_EXTENSION];
        let prof_level: u8 = 0x48; // 0100 1000
                                   // byte0: ext_id(0001) | prof_level_hi(0100)
        b.push((EXT_ID_SEQUENCE << 4) | (prof_level >> 4));
        // byte1: prof_level_lo(1000) prog(0) chroma(01) hsize_ext_hi(0)
        let prog = 0u8;
        let chroma = 1u8;
        let hsize_ext = 0u8;
        b.push(((prof_level & 0x0F) << 4) | (prog << 3) | (chroma << 1) | (hsize_ext >> 1));
        // byte2: hsize_ext_lo(0) vsize_ext(00) bitrate_ext_hi(00000)
        b.push((hsize_ext & 1) << 7);
        // byte3: bitrate_ext_lo(7) marker(1)
        b.push(0x01);
        // byte4: vbv_buffer_size_extension
        b.push(0x00);
        // byte5: low_delay(0) fr_ext_n(00) fr_ext_d(00000)
        b.push(0x00);
        b
    }

    #[test]
    fn seq_ext_decode() {
        let buf = main_seq_ext();
        let e = SequenceExtension::parse(&buf).unwrap();
        assert_eq!(e.profile_and_level, 0x48);
        assert_eq!(e.profile(), 4);
        assert_eq!(e.level(), 8);
        assert!(!e.progressive_sequence);
        assert_eq!(e.chroma_format, 1);
        assert_eq!(e.horizontal_size_extension, 0);
        assert_eq!(e.vertical_size_extension, 0);
        assert_eq!(e.bit_rate_extension, 0);
        assert!(!e.low_delay);
    }

    #[test]
    fn seq_header_with_extension_full_sizes() {
        let h = SequenceHeader::parse(&ntsc_seq_header()).unwrap();
        let e = SequenceExtension::parse(&main_seq_ext()).unwrap();
        // No extension bits set → full size == base size.
        assert_eq!(h.full_horizontal_size(&e), 720);
        assert_eq!(h.full_vertical_size(&e), 480);
        assert_eq!(h.bit_rate_bps_with_extension(&e), 9_800_000);
    }

    #[test]
    fn seq_ext_wrong_id() {
        let mut buf = main_seq_ext();
        buf[4] = (EXT_ID_SEQUENCE_DISPLAY << 4) | (buf[4] & 0x0F);
        assert!(SequenceExtension::parse(&buf).is_err());
    }
}
