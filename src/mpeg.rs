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

// ------------------------------------------------------------------
// Sequence Display Extension (00 00 01 B5, ext-id 0010)
// ------------------------------------------------------------------

/// Optional colour-description triple carried by a
/// [`SequenceDisplayExtension`] when its `colour_description_flag`
/// is set (per `mpucoder-mpeghdrs.html`; the same ISO/IEC 13818-2
/// §6.3.6 code points DVD authors per `mpucoder-dvdmpeg.html`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourDescription {
    /// `colour_primaries` (NTSC = 4 BT.470 M or 6 SMPTE 170M;
    /// PAL = 5 BT.470 B/G).
    pub colour_primaries: u8,
    /// `transfer_characteristics`.
    pub transfer_characteristics: u8,
    /// `matrix_coefficients` (NTSC = 4/6, PAL = 5).
    pub matrix_coefficients: u8,
}

/// Decoded MPEG-2 Sequence Display Extension (`mpucoder-mpeghdrs.html`).
///
/// Layout after the `00 00 01 B5` start code:
/// - extension-id — 4 bits (`0010`)
/// - `video_format` — 3 bits
/// - `colour_description` — 1 bit (+ 3 colour bytes when set)
/// - `display_horizontal_size` — 14 bits
/// - marker bit — 1
/// - `display_vertical_size` — 14 bits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceDisplayExtension {
    /// 3-bit `video_format` (0 = component, 1 = PAL, 2 = NTSC,
    /// 3 = SECAM, 4 = MAC, 5 = unspecified).
    pub video_format: u8,
    /// Optional colour description (present iff
    /// `colour_description_flag` was 1).
    pub colour: Option<ColourDescription>,
    /// 14-bit `display_horizontal_size`.
    pub display_horizontal_size: u16,
    /// 14-bit `display_vertical_size`.
    pub display_vertical_size: u16,
}

impl SequenceDisplayExtension {
    /// Parse a Sequence Display Extension. `buf` begins at the
    /// `00 00 01 B5` start code; the extension-id must be `0010`.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 || buf[0..3] != [0x00, 0x00, 0x01] || buf[3] != SC_EXTENSION {
            return Err(Error::InvalidUdf("MPEG seq disp ext: missing 00 00 01 B5"));
        }
        // Minimum body: id+fmt+flag in byte0, then 4 bytes for the two
        // 14-bit sizes (when no colour description). 5 body bytes.
        if buf.len() < 4 + 5 {
            return Err(Error::InvalidUdf("MPEG seq disp ext: truncated"));
        }
        let b = &buf[4..];
        if (b[0] >> 4) != EXT_ID_SEQUENCE_DISPLAY {
            return Err(Error::InvalidUdf("MPEG seq disp ext: extension-id != 0010"));
        }
        let video_format = (b[0] >> 1) & 0b111;
        let colour_description_flag = b[0] & 1 == 1;
        // The two 14-bit sizes are byte-aligned right after the
        // optional 3 colour bytes: each size spans 2 bytes minus the
        // marker bit between them.
        let (colour, off) = if colour_description_flag {
            if buf.len() < 4 + 8 {
                return Err(Error::InvalidUdf(
                    "MPEG seq disp ext: colour flagged but truncated",
                ));
            }
            (
                Some(ColourDescription {
                    colour_primaries: b[1],
                    transfer_characteristics: b[2],
                    matrix_coefficients: b[3],
                }),
                4usize,
            )
        } else {
            (None, 1usize)
        };
        // display_horizontal_size: 14 bits across b[off], b[off+1] hi6.
        let dh = ((b[off] as u16) << 6) | ((b[off + 1] as u16) >> 2);
        // marker bit at b[off+1] bit1.
        if (b[off + 1] >> 1) & 1 != 1 {
            return Err(Error::InvalidUdf("MPEG seq disp ext: marker bit not set"));
        }
        // display_vertical_size: 14 bits — low 1 bit of b[off+1],
        // b[off+2], top 5 bits of b[off+3].
        let dv = (((b[off + 1] & 1) as u16) << 13)
            | ((b[off + 2] as u16) << 5)
            | ((b[off + 3] as u16) >> 3);
        Ok(Self {
            video_format,
            colour,
            display_horizontal_size: dh,
            display_vertical_size: dv,
        })
    }
}

// ------------------------------------------------------------------
// Group-of-Pictures header (00 00 01 B8)
// ------------------------------------------------------------------

/// Decoded GOP header (`mpucoder-mpeghdrs.html`). Fixed length:
/// 4 bytes after the `00 00 01 B8` start code carry a 25-bit SMPTE
/// time-code plus the drop-frame / closed-GOP / broken-link flags.
///
/// Bit layout (after the start code):
/// - `drop_frame_flag` — 1
/// - `time_code_hours` — 5
/// - `time_code_minutes` — 6
/// - marker bit — 1
/// - `time_code_seconds` — 6
/// - `time_code_pictures` — 6
/// - `closed_gop` — 1
/// - `broken_link` — 1
/// - 5 reserved zero bits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GopHeader {
    /// `drop_frame_flag` (NTSC drop-frame time-code).
    pub drop_frame: bool,
    /// Time-code hours (0..=23).
    pub hours: u8,
    /// Time-code minutes (0..=59).
    pub minutes: u8,
    /// Time-code seconds (0..=59).
    pub seconds: u8,
    /// Time-code frame/picture number (0..=59).
    pub frames: u8,
    /// `closed_gop` — the GOP can be decoded without the previous one.
    pub closed_gop: bool,
    /// `broken_link` — editing severed the prior reference (display
    /// the leading B-frames at the player's discretion).
    pub broken_link: bool,
}

impl GopHeader {
    /// Length of the fixed body after the start code (4 bytes).
    pub const BODY_LEN: usize = 4;

    /// Parse a GOP header. `buf` begins at the `00 00 01 B8` start
    /// code.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 || buf[0..3] != [0x00, 0x00, 0x01] || buf[3] != SC_GROUP_OF_PICTURES {
            return Err(Error::InvalidUdf("MPEG GOP: missing 00 00 01 B8"));
        }
        if buf.len() < 4 + Self::BODY_LEN {
            return Err(Error::InvalidUdf("MPEG GOP: truncated"));
        }
        let b = &buf[4..];
        let drop_frame = (b[0] >> 7) & 1 == 1;
        let hours = (b[0] >> 2) & 0x1F;
        let minutes = ((b[0] & 0b11) << 4) | (b[1] >> 4);
        // marker bit at b[1] bit3.
        if (b[1] >> 3) & 1 != 1 {
            return Err(Error::InvalidUdf("MPEG GOP: marker bit not set"));
        }
        let seconds = ((b[1] & 0b111) << 3) | (b[2] >> 5);
        let frames = ((b[2] & 0x1F) << 1) | (b[3] >> 7);
        let closed_gop = (b[3] >> 6) & 1 == 1;
        let broken_link = (b[3] >> 5) & 1 == 1;
        Ok(Self {
            drop_frame,
            hours,
            minutes,
            seconds,
            frames,
            closed_gop,
            broken_link,
        })
    }
}

// ------------------------------------------------------------------
// Picture header (00 00 01 00)
// ------------------------------------------------------------------

/// `picture_coding_type` — the per-picture frame type.
///
/// Per `mpucoder-mpeghdrs.html` (`frame type 1=I, 2=P, 3=B, 4=D`);
/// DVD MPEG-2 uses only I/P/B (D-pictures are MPEG-1-only and never
/// authored on DVD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureCodingType {
    /// `1` — intra-coded (I) picture.
    Intra,
    /// `2` — predictive (P) picture.
    Predictive,
    /// `3` — bidirectionally-predictive (B) picture.
    Bidirectional,
    /// `4` — DC intra-coded (D) picture (MPEG-1 only).
    DcIntra,
    /// `0` / `5..=7` — forbidden / reserved.
    Reserved(u8),
}

impl PictureCodingType {
    /// Decode the raw 3-bit code.
    pub fn from_code(code: u8) -> Self {
        match code & 0b111 {
            1 => Self::Intra,
            2 => Self::Predictive,
            3 => Self::Bidirectional,
            4 => Self::DcIntra,
            other => Self::Reserved(other),
        }
    }

    /// Whether this picture is a GOP entry point (an I-picture a
    /// seeker can decode without earlier references).
    pub fn is_intra(self) -> bool {
        matches!(self, Self::Intra)
    }
}

/// Decoded MPEG video Picture Header (`mpucoder-mpeghdrs.html`).
///
/// Bit layout after the `00 00 01 00` start code:
/// - `temporal_reference` — 10 bits
/// - `picture_coding_type` — 3 bits
/// - `vbv_delay` — 16 bits
///
/// The MPEG-1 `full_pel`/`f_code` tail for P/B pictures is not
/// decoded (on DVD MPEG-2 those bits are the fixed `0111` placeholder
/// and the real motion `f_code` values live in the
/// [`PictureCodingExtension`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureHeader {
    /// 10-bit `temporal_reference` (display order within the GOP).
    pub temporal_reference: u16,
    /// 3-bit `picture_coding_type`.
    pub coding_type: PictureCodingType,
    /// 16-bit `vbv_delay`.
    pub vbv_delay: u16,
}

impl PictureHeader {
    /// Minimum bytes after the start code needed to read the three
    /// fixed fields (29 bits → 4 bytes).
    pub const FIXED_LEN: usize = 4;

    /// Parse a picture header. `buf` begins at the `00 00 01 00`
    /// start code.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 || buf[0..3] != [0x00, 0x00, 0x01] || buf[3] != SC_PICTURE {
            return Err(Error::InvalidUdf("MPEG picture: missing 00 00 01 00"));
        }
        if buf.len() < 4 + Self::FIXED_LEN {
            return Err(Error::InvalidUdf("MPEG picture: truncated"));
        }
        let b = &buf[4..];
        // temporal_reference: byte0 (8 bits) + byte1 top 2.
        let temporal_reference = ((b[0] as u16) << 2) | ((b[1] >> 6) as u16);
        let coding_type = PictureCodingType::from_code((b[1] >> 3) & 0b111);
        // vbv_delay: byte1 low 3 + byte2 (8) + byte3 top 5.
        let vbv_delay =
            (((b[1] & 0b111) as u16) << 13) | ((b[2] as u16) << 5) | ((b[3] >> 3) as u16);
        Ok(Self {
            temporal_reference,
            coding_type,
            vbv_delay,
        })
    }
}

// ------------------------------------------------------------------
// Picture Coding Extension (00 00 01 B5, ext-id 1000)
// ------------------------------------------------------------------

/// `picture_structure` — how the picture maps onto display fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureStructure {
    /// `0` — reserved.
    Reserved,
    /// `1` — top field.
    TopField,
    /// `2` — bottom field.
    BottomField,
    /// `3` — frame picture (both fields).
    FramePicture,
}

impl PictureStructure {
    /// Decode the raw 2-bit code.
    pub fn from_code(code: u8) -> Self {
        match code & 0b11 {
            1 => Self::TopField,
            2 => Self::BottomField,
            3 => Self::FramePicture,
            _ => Self::Reserved,
        }
    }
}

/// Decoded MPEG-2 Picture Coding Extension (`mpucoder-mpeghdrs.html`).
///
/// Body after the `00 00 01 B5` start code (extension-id `1000`):
/// - 4 × 4-bit `f_code[s][t]`
/// - `intra_dc_precision` — 2 bits
/// - `picture_structure` — 2 bits
/// - `top_field_first` / `frame_pred_frame_dct` /
///   `concealment_motion_vectors` / `q_scale_type` /
///   `intra_vlc_format` / `alternate_scan` / `repeat_first_field` /
///   `chroma_420_type` — 1 bit each
/// - `progressive_frame` — 1 bit
/// - `composite_display_flag` — 1 bit (+ trailer when set; not decoded)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureCodingExtension {
    /// `f_code[0][0]` — forward horizontal.
    pub f_code_fwd_horiz: u8,
    /// `f_code[0][1]` — forward vertical.
    pub f_code_fwd_vert: u8,
    /// `f_code[1][0]` — backward horizontal.
    pub f_code_bwd_horiz: u8,
    /// `f_code[1][1]` — backward vertical.
    pub f_code_bwd_vert: u8,
    /// 2-bit `intra_dc_precision`.
    pub intra_dc_precision: u8,
    /// 2-bit `picture_structure`.
    pub picture_structure: PictureStructure,
    /// `top_field_first`.
    pub top_field_first: bool,
    /// `frame_pred_frame_dct`.
    pub frame_pred_frame_dct: bool,
    /// `concealment_motion_vectors`.
    pub concealment_motion_vectors: bool,
    /// `q_scale_type`.
    pub q_scale_type: bool,
    /// `intra_vlc_format`.
    pub intra_vlc_format: bool,
    /// `alternate_scan`.
    pub alternate_scan: bool,
    /// `repeat_first_field`.
    pub repeat_first_field: bool,
    /// `chroma_420_type`.
    pub chroma_420_type: bool,
    /// `progressive_frame`.
    pub progressive_frame: bool,
    /// `composite_display_flag`.
    pub composite_display_flag: bool,
}

impl PictureCodingExtension {
    /// Minimum body bytes after the start code (5 bytes covers
    /// through `composite_display_flag`).
    pub const MIN_BODY_LEN: usize = 5;

    /// Parse a Picture Coding Extension. `buf` begins at the
    /// `00 00 01 B5` start code; the extension-id must be `1000`.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 || buf[0..3] != [0x00, 0x00, 0x01] || buf[3] != SC_EXTENSION {
            return Err(Error::InvalidUdf(
                "MPEG pic-coding ext: missing 00 00 01 B5",
            ));
        }
        if buf.len() < 4 + Self::MIN_BODY_LEN {
            return Err(Error::InvalidUdf("MPEG pic-coding ext: truncated"));
        }
        let b = &buf[4..];
        if (b[0] >> 4) != EXT_ID_PICTURE_CODING {
            return Err(Error::InvalidUdf(
                "MPEG pic-coding ext: extension-id != 1000",
            ));
        }
        // byte0: id(4) f00(4); byte1: f01(4) f10(4); byte2: f11(4) ...
        let f_code_fwd_horiz = b[0] & 0x0F;
        let f_code_fwd_vert = b[1] >> 4;
        let f_code_bwd_horiz = b[1] & 0x0F;
        let f_code_bwd_vert = b[2] >> 4;
        let intra_dc_precision = (b[2] >> 2) & 0b11;
        let picture_structure = PictureStructure::from_code(b[2] & 0b11);
        // byte3: TFF FPFD CMV QST IVF AS RFF C420
        let top_field_first = (b[3] >> 7) & 1 == 1;
        let frame_pred_frame_dct = (b[3] >> 6) & 1 == 1;
        let concealment_motion_vectors = (b[3] >> 5) & 1 == 1;
        let q_scale_type = (b[3] >> 4) & 1 == 1;
        let intra_vlc_format = (b[3] >> 3) & 1 == 1;
        let alternate_scan = (b[3] >> 2) & 1 == 1;
        let repeat_first_field = (b[3] >> 1) & 1 == 1;
        let chroma_420_type = b[3] & 1 == 1;
        // byte4: progressive_frame composite_display_flag ...
        let progressive_frame = (b[4] >> 7) & 1 == 1;
        let composite_display_flag = (b[4] >> 6) & 1 == 1;
        Ok(Self {
            f_code_fwd_horiz,
            f_code_fwd_vert,
            f_code_bwd_horiz,
            f_code_bwd_vert,
            intra_dc_precision,
            picture_structure,
            top_field_first,
            frame_pred_frame_dct,
            concealment_motion_vectors,
            q_scale_type,
            intra_vlc_format,
            alternate_scan,
            repeat_first_field,
            chroma_420_type,
            progressive_frame,
            composite_display_flag,
        })
    }

    /// `intra_dc_precision` as the actual bit depth (8 + value).
    pub fn intra_dc_bits(self) -> u8 {
        8 + self.intra_dc_precision
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

    /// Sequence Display Extension with no colour description:
    /// video_format = NTSC (2), display 720×480.
    fn disp_ext_no_colour() -> Vec<u8> {
        let mut b = vec![0x00, 0x00, 0x01, SC_EXTENSION];
        let video_format = 2u8;
        // byte0: id(0010) video_format(010) colour_flag(0)
        b.push((EXT_ID_SEQUENCE_DISPLAY << 4) | (video_format << 1));
        // display_horizontal_size = 720 (14 bits): hi8 | lo6
        let dh: u16 = 720;
        b.push((dh >> 6) as u8);
        let dh_lo6 = (dh & 0x3F) as u8;
        let marker = 1u8;
        let dv: u16 = 480;
        let dv_hi1 = (dv >> 13) as u8;
        // byte: dh_lo6(6) marker(1) dv_hi1(1)
        b.push((dh_lo6 << 2) | (marker << 1) | dv_hi1);
        // byte: dv bits 12..5
        b.push((dv >> 5) as u8);
        // byte: dv bits 4..0 (top 5) then 3 zero bits
        b.push(((dv & 0x1F) as u8) << 3);
        b
    }

    #[test]
    fn seq_disp_ext_no_colour() {
        let e = SequenceDisplayExtension::parse(&disp_ext_no_colour()).unwrap();
        assert_eq!(e.video_format, 2);
        assert_eq!(e.colour, None);
        assert_eq!(e.display_horizontal_size, 720);
        assert_eq!(e.display_vertical_size, 480);
    }

    /// Sequence Display Extension with a colour description triple.
    fn disp_ext_colour() -> Vec<u8> {
        let mut b = vec![0x00, 0x00, 0x01, SC_EXTENSION];
        let video_format = 2u8;
        b.push((EXT_ID_SEQUENCE_DISPLAY << 4) | (video_format << 1) | 1); // colour flag
        b.push(6); // colour_primaries (SMPTE 170M)
        b.push(6); // transfer
        b.push(6); // matrix
        let dh: u16 = 720;
        b.push((dh >> 6) as u8);
        let dv: u16 = 480;
        b.push(((dh & 0x3F) as u8) << 2 | (1 << 1) | (dv >> 13) as u8);
        b.push((dv >> 5) as u8);
        b.push(((dv & 0x1F) as u8) << 3);
        b
    }

    #[test]
    fn seq_disp_ext_with_colour() {
        let e = SequenceDisplayExtension::parse(&disp_ext_colour()).unwrap();
        assert_eq!(
            e.colour,
            Some(ColourDescription {
                colour_primaries: 6,
                transfer_characteristics: 6,
                matrix_coefficients: 6,
            })
        );
        assert_eq!(e.display_horizontal_size, 720);
        assert_eq!(e.display_vertical_size, 480);
    }

    #[test]
    fn seq_disp_ext_wrong_id() {
        let mut buf = disp_ext_no_colour();
        buf[4] = (EXT_ID_SEQUENCE << 4) | (buf[4] & 0x0F);
        assert!(SequenceDisplayExtension::parse(&buf).is_err());
    }

    /// GOP header: 01:23:45;12, closed GOP, not broken, drop-frame.
    fn gop_header(drop: bool, closed: bool, broken: bool) -> Vec<u8> {
        let mut b = vec![0x00, 0x00, 0x01, SC_GROUP_OF_PICTURES];
        let (h, m, s, f) = (1u8, 23u8, 45u8, 12u8);
        // byte0: drop(1) hours(5) min_hi(2)
        b.push(((drop as u8) << 7) | (h << 2) | (m >> 4));
        // byte1: min_lo(4) marker(1) sec_hi(3)
        b.push(((m & 0x0F) << 4) | (1 << 3) | (s >> 3));
        // byte2: sec_lo(3) frame_hi(5)
        b.push(((s & 0b111) << 5) | (f >> 1));
        // byte3: frame_lo(1) closed(1) broken(1) zeros(5)
        b.push(((f & 1) << 7) | ((closed as u8) << 6) | ((broken as u8) << 5));
        b
    }

    #[test]
    fn gop_header_decode() {
        let g = GopHeader::parse(&gop_header(true, true, false)).unwrap();
        assert!(g.drop_frame);
        assert_eq!(g.hours, 1);
        assert_eq!(g.minutes, 23);
        assert_eq!(g.seconds, 45);
        assert_eq!(g.frames, 12);
        assert!(g.closed_gop);
        assert!(!g.broken_link);
    }

    #[test]
    fn gop_header_open_broken() {
        let g = GopHeader::parse(&gop_header(false, false, true)).unwrap();
        assert!(!g.drop_frame);
        assert!(!g.closed_gop);
        assert!(g.broken_link);
    }

    #[test]
    fn gop_header_bad_start() {
        let mut buf = gop_header(false, true, false);
        buf[3] = SC_SEQUENCE_HEADER;
        assert!(GopHeader::parse(&buf).is_err());
    }

    /// Picture header: temporal_reference, coding type, vbv_delay.
    fn picture_header(tr: u16, ct: u8, vbv: u16) -> Vec<u8> {
        let mut b = vec![0x00, 0x00, 0x01, SC_PICTURE];
        // byte0: TR[9..2]
        b.push((tr >> 2) as u8);
        // byte1: TR[1..0](2) coding_type(3) vbv[15..13](3)
        b.push((((tr & 0b11) as u8) << 6) | ((ct & 0b111) << 3) | ((vbv >> 13) as u8));
        // byte2: vbv[12..5]
        b.push((vbv >> 5) as u8);
        // byte3: vbv[4..0](5) + 3 trailing bits (0)
        b.push(((vbv & 0x1F) as u8) << 3);
        b
    }

    #[test]
    fn picture_header_i_frame() {
        let p = PictureHeader::parse(&picture_header(0, 1, 0xFFFF)).unwrap();
        assert_eq!(p.temporal_reference, 0);
        assert_eq!(p.coding_type, PictureCodingType::Intra);
        assert!(p.coding_type.is_intra());
        assert_eq!(p.vbv_delay, 0xFFFF);
    }

    #[test]
    fn picture_header_b_frame() {
        let p = PictureHeader::parse(&picture_header(513, 3, 0x1234)).unwrap();
        assert_eq!(p.temporal_reference, 513);
        assert_eq!(p.coding_type, PictureCodingType::Bidirectional);
        assert!(!p.coding_type.is_intra());
        assert_eq!(p.vbv_delay, 0x1234);
    }

    #[test]
    fn picture_coding_type_codes() {
        assert_eq!(PictureCodingType::from_code(1), PictureCodingType::Intra);
        assert_eq!(
            PictureCodingType::from_code(2),
            PictureCodingType::Predictive
        );
        assert_eq!(PictureCodingType::from_code(4), PictureCodingType::DcIntra);
        assert_eq!(
            PictureCodingType::from_code(0),
            PictureCodingType::Reserved(0)
        );
    }

    /// Picture Coding Extension: f_codes 0111 (MPEG-2 placeholder for
    /// fwd) and 0xF for unused, intra_dc=0, frame picture, TFF=1,
    /// frame_pred=1, progressive_frame=0.
    fn pic_coding_ext() -> Vec<u8> {
        let mut b = vec![0x00, 0x00, 0x01, SC_EXTENSION];
        // byte0: id(1000) f_fwd_h(0111)
        b.push((EXT_ID_PICTURE_CODING << 4) | 0b0111);
        // byte1: f_fwd_v(0111) f_bwd_h(1111)
        b.push((0b0111 << 4) | 0b1111);
        // byte2: f_bwd_v(1111) intra_dc(00) pic_struct(11=frame)
        b.push((0b1111 << 4) | 0b11);
        // byte3: TFF(1) FPFD(1) CMV(0) QST(0) IVF(0) AS(0) RFF(0) C420(0)
        b.push(0b1100_0000);
        // byte4: progressive_frame(0) composite(0) ...
        b.push(0x00);
        b
    }

    #[test]
    fn pic_coding_ext_decode() {
        let e = PictureCodingExtension::parse(&pic_coding_ext()).unwrap();
        assert_eq!(e.f_code_fwd_horiz, 0b0111);
        assert_eq!(e.f_code_fwd_vert, 0b0111);
        assert_eq!(e.f_code_bwd_horiz, 0b1111);
        assert_eq!(e.f_code_bwd_vert, 0b1111);
        assert_eq!(e.intra_dc_precision, 0);
        assert_eq!(e.intra_dc_bits(), 8);
        assert_eq!(e.picture_structure, PictureStructure::FramePicture);
        assert!(e.top_field_first);
        assert!(e.frame_pred_frame_dct);
        assert!(!e.concealment_motion_vectors);
        assert!(!e.progressive_frame);
        assert!(!e.composite_display_flag);
    }

    #[test]
    fn pic_coding_ext_wrong_id() {
        let mut buf = pic_coding_ext();
        buf[4] = (EXT_ID_SEQUENCE << 4) | (buf[4] & 0x0F);
        assert!(PictureCodingExtension::parse(&buf).is_err());
    }

    #[test]
    fn picture_structure_codes() {
        assert_eq!(PictureStructure::from_code(0), PictureStructure::Reserved);
        assert_eq!(PictureStructure::from_code(1), PictureStructure::TopField);
        assert_eq!(
            PictureStructure::from_code(2),
            PictureStructure::BottomField
        );
        assert_eq!(
            PictureStructure::from_code(3),
            PictureStructure::FramePicture
        );
    }
}
