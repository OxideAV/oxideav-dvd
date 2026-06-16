//! DVD-Video DTS (DTS Coherent Acoustics) core frame-header decoder.
//!
//! On DVD-Video, DTS audio is carried inside MPEG-PS `private_stream_1`
//! (`stream_id = 0xBD`) PES packets under the `0x88..=0x8F` substream
//! allocation that the VOB demuxer already routes (see
//! [`crate::vob::DvdSubstream::Dts`]). The demuxer hands the elementary
//! stream to a downstream audio decoder as raw bytes; the very first
//! bytes of that stream are a DTS *core sync frame*, whose 10-byte
//! header pins the frame size, the sample rate, the channel
//! arrangement, and the targeted bit rate. This module decodes those
//! header fields so a player can size buffers, label the track, and
//! seek to a frame boundary without pulling in a full DTS audio
//! decoder.
//!
//! ## Scope
//!
//! The decode covers the first 10 bytes of the DTS core frame: the
//! 32-bit sync word (`7F FE 80 01`) plus the bit-packed header fields
//! `ftype`, `short`, `cpf`, `nblks`, `fsize`, `amode`, `sfreq`, `rate`,
//! and the five trailing 1-bit flags (`mix`, `dynf`, `timef`, `auxf`,
//! `hdcd`). Everything after `hdcd` is the variable-length remainder of
//! the DTS bit stream, which a header-only reader cannot traverse; it
//! stays available to the audio decoder as raw frame bytes.
//!
//! The decode is read-only and allocation-free.
//!
//! ## Clean-room references
//!
//! - `docs/container/dvd/application/stnsoft-dtshdr.html` — the 10-byte
//!   DTS core frame-header layout: the `7F FE 80 01` sync word, the
//!   bit allocation of the nine header fields plus the five trailing
//!   flags, the `amode` audio-channel-arrangement table, the `sfreq`
//!   sampling-rate table, and the two DVD-Video `rate` codes.
//! - `docs/container/dvd/application/mpucoder-dvdmpeg.html` — the
//!   `0x88..=0x8F` substream allocation that locates the DTS
//!   elementary stream inside the `private_stream_1` PES payload.
//!
//! Field layouts derive from the `stnsoft-dtshdr.html` reference cited
//! above.

use crate::error::{Error, Result};

/// DTS core sync word — the big-endian `0x7FFE8001` at the start of
/// every core sync frame.
pub const DTS_SYNC_WORD: u32 = 0x7FFE_8001;

/// Frame type carried in the 1-bit `ftype` field.
///
/// `1 = normal` frame, `0 = termination` frame (a short final frame
/// that falls short of the normal 32-sample core block length by the
/// [`DtsHeader::short`] sample count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtsFrameType {
    /// `ftype = 1` — a normal frame (`short` must be 31).
    Normal,
    /// `ftype = 0` — a termination frame.
    Termination,
}

/// Audio channel arrangement carried in the 6-bit `amode` field.
///
/// The enum names the speaker layout in the spec's notation;
/// [`Self::channel_count`] returns the number of channels. Codes
/// `0x10..=0x3F` are user-defined and surface as
/// [`Self::UserDefined`] carrying the raw code; [`Self::channel_count`]
/// returns `None` for them since the layout is not standardised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtsAudioMode {
    /// `0x00` — 1 channel: A (mono).
    Mono,
    /// `0x01` — 2 channels: A + B (dual mono).
    DualMono,
    /// `0x02` — 2 channels: L + R (stereo).
    Stereo,
    /// `0x03` — 2 channels: (L+R) + (L-R) (sum and difference).
    SumDifference,
    /// `0x04` — 2 channels: LT + RT (left and right total).
    LeftRightTotal,
    /// `0x05` — 3 channels: C + L + R.
    ThreeChannel,
    /// `0x06` — 3 channels: L + R + S.
    StereoSurround,
    /// `0x07` — 4 channels: C + L + R + S.
    ThreeOneSurround,
    /// `0x08` — 4 channels: L + R + SL + SR.
    QuadSurround,
    /// `0x09` — 5 channels: C + L + R + SL + SR.
    FiveChannel,
    /// `0x0A` — 6 channels: CL + CR + L + R + SL + SR.
    SixChannelA,
    /// `0x0B` — 6 channels: C + L + R + LR + RR + OV.
    SixChannelB,
    /// `0x0C` — 6 channels: CF + CR + LF + RF + LR + RR.
    SixChannelC,
    /// `0x0D` — 7 channels: CL + C + CR + L + R + SL + SR.
    SevenChannel,
    /// `0x0E` — 8 channels: CL + CR + L + R + SL1 + SL2 + SR1 + SR2.
    EightChannelA,
    /// `0x0F` — 8 channels: CL + C + CR + L + R + SL + S + SR.
    EightChannelB,
    /// `0x10..=0x3F` — user-defined arrangement (raw code preserved).
    UserDefined(u8),
}

impl DtsAudioMode {
    fn from_code(code: u8) -> Self {
        match code & 0b0011_1111 {
            0x00 => Self::Mono,
            0x01 => Self::DualMono,
            0x02 => Self::Stereo,
            0x03 => Self::SumDifference,
            0x04 => Self::LeftRightTotal,
            0x05 => Self::ThreeChannel,
            0x06 => Self::StereoSurround,
            0x07 => Self::ThreeOneSurround,
            0x08 => Self::QuadSurround,
            0x09 => Self::FiveChannel,
            0x0A => Self::SixChannelA,
            0x0B => Self::SixChannelB,
            0x0C => Self::SixChannelC,
            0x0D => Self::SevenChannel,
            0x0E => Self::EightChannelA,
            0x0F => Self::EightChannelB,
            other => Self::UserDefined(other),
        }
    }

    /// The raw 6-bit `amode` code.
    pub fn code(self) -> u8 {
        match self {
            Self::Mono => 0x00,
            Self::DualMono => 0x01,
            Self::Stereo => 0x02,
            Self::SumDifference => 0x03,
            Self::LeftRightTotal => 0x04,
            Self::ThreeChannel => 0x05,
            Self::StereoSurround => 0x06,
            Self::ThreeOneSurround => 0x07,
            Self::QuadSurround => 0x08,
            Self::FiveChannel => 0x09,
            Self::SixChannelA => 0x0A,
            Self::SixChannelB => 0x0B,
            Self::SixChannelC => 0x0C,
            Self::SevenChannel => 0x0D,
            Self::EightChannelA => 0x0E,
            Self::EightChannelB => 0x0F,
            Self::UserDefined(code) => code,
        }
    }

    /// Number of audio channels the arrangement carries; `None` for a
    /// user-defined code (`0x10..=0x3F`), whose channel count is not
    /// standardised.
    pub fn channel_count(self) -> Option<u8> {
        let n = match self {
            Self::Mono => 1,
            Self::DualMono | Self::Stereo | Self::SumDifference | Self::LeftRightTotal => 2,
            Self::ThreeChannel | Self::StereoSurround => 3,
            Self::ThreeOneSurround | Self::QuadSurround => 4,
            Self::FiveChannel => 5,
            Self::SixChannelA | Self::SixChannelB | Self::SixChannelC => 6,
            Self::SevenChannel => 7,
            Self::EightChannelA | Self::EightChannelB => 8,
            Self::UserDefined(_) => return None,
        };
        Some(n)
    }
}

/// Audio sampling rate carried in the 4-bit `sfreq` field.
///
/// The DTS `sfreq` table interleaves valid rates with reserved codes;
/// reserved codes surface as [`Self::Invalid`] without losing the fact
/// that the field was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtsSampleRate {
    /// `0x1` — 8 kHz.
    Hz8000,
    /// `0x2` — 16 kHz.
    Hz16000,
    /// `0x3` — 32 kHz.
    Hz32000,
    /// `0x6` — 11025 Hz.
    Hz11025,
    /// `0x7` — 22050 Hz.
    Hz22050,
    /// `0x8` — 44100 Hz.
    Hz44100,
    /// `0xB` — 12 kHz.
    Hz12000,
    /// `0xC` — 24 kHz.
    Hz24000,
    /// `0xD` — 48 kHz (the DVD-Video rate).
    Hz48000,
    /// Any reserved / invalid code (`0`, `4`, `5`, `9`, `A`, `E`, `F`).
    Invalid,
}

impl DtsSampleRate {
    fn from_code(code: u8) -> Self {
        match code & 0b1111 {
            0x1 => Self::Hz8000,
            0x2 => Self::Hz16000,
            0x3 => Self::Hz32000,
            0x6 => Self::Hz11025,
            0x7 => Self::Hz22050,
            0x8 => Self::Hz44100,
            0xB => Self::Hz12000,
            0xC => Self::Hz24000,
            0xD => Self::Hz48000,
            _ => Self::Invalid,
        }
    }

    /// Sample rate in Hz for the nine defined codes; `None` for
    /// [`Self::Invalid`].
    pub fn hz(self) -> Option<u32> {
        match self {
            Self::Hz8000 => Some(8_000),
            Self::Hz16000 => Some(16_000),
            Self::Hz32000 => Some(32_000),
            Self::Hz11025 => Some(11_025),
            Self::Hz22050 => Some(22_050),
            Self::Hz44100 => Some(44_100),
            Self::Hz12000 => Some(12_000),
            Self::Hz24000 => Some(24_000),
            Self::Hz48000 => Some(48_000),
            Self::Invalid => None,
        }
    }
}

/// The 5-bit `rate` targeted-bit-rate code. On DVD-Video only two
/// values occur (`0x0F` → 768 kbps, `0x18` → 1536 kbps); the full
/// transmission-rate table is out of the DVD-Video scope, so other
/// codes surface as [`Self::Other`] carrying the raw value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtsBitRate {
    /// `0x0F` — targeted 768 kbps (actual 754.5 kbps).
    Kbps768,
    /// `0x18` — targeted 1536 kbps (actual 1509.75 kbps).
    Kbps1536,
    /// Any other `rate` code (raw 5-bit value preserved).
    Other(u8),
}

impl DtsBitRate {
    fn from_code(code: u8) -> Self {
        match code & 0b0001_1111 {
            0x0F => Self::Kbps768,
            0x18 => Self::Kbps1536,
            other => Self::Other(other),
        }
    }

    /// The raw 5-bit `rate` code.
    pub fn code(self) -> u8 {
        match self {
            Self::Kbps768 => 0x0F,
            Self::Kbps1536 => 0x18,
            Self::Other(code) => code,
        }
    }

    /// Targeted bit rate in kbps for the two DVD-Video codes; `None`
    /// for any other code.
    pub fn targeted_kbps(self) -> Option<u16> {
        match self {
            Self::Kbps768 => Some(768),
            Self::Kbps1536 => Some(1536),
            Self::Other(_) => None,
        }
    }
}

/// Decoded DTS core frame header (the first 10 bytes of a DTS core
/// sync frame).
///
/// Field layout from `stnsoft-dtshdr.html` (MSB-first, starting at the
/// byte after the 4-byte sync word):
///
/// ```text
/// syncword  32  0x7FFE8001
/// ftype      1  1 = normal, 0 = termination
/// short      5  samples a termination frame falls short of 32 (31 for normal)
/// cpf        1  CRC present flag (FALSE on DVD)
/// nblks      7  (sample blocks − 1); 15 on DVD → 16 × 32 = 512 samples
/// fsize     14  (frame size in bytes − 1)
/// amode      6  audio channel arrangement
/// sfreq      4  sampling rate
/// rate       5  targeted bit rate
/// mix        1  embedded down-mix enabled
/// dynf       1  embedded dynamic-range data
/// timef      1  embedded time-stamp
/// auxf       1  auxiliary byte count present
/// hdcd       1  HDCD format
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtsHeader {
    /// Decoded `ftype` frame type.
    pub frame_type: DtsFrameType,
    /// Raw 5-bit `short` field: the number of core samples by which a
    /// termination frame falls short of the normal 32. For a normal
    /// frame this is 31.
    pub short: u8,
    /// `cpf` — `true` when a CRC is present (always `false` on DVD).
    pub crc_present: bool,
    /// Raw 7-bit `nblks` field: the number of 32-sample core blocks
    /// minus 1. (Use [`Self::sample_block_count`] for the +1 form.)
    pub nblks: u8,
    /// Raw 14-bit `fsize` field: the frame size in bytes minus 1.
    /// (Use [`Self::frame_size_bytes`] for the +1 byte count.)
    pub fsize: u16,
    /// Decoded `amode` audio channel arrangement.
    pub audio_mode: DtsAudioMode,
    /// Decoded `sfreq` sampling rate.
    pub sample_rate: DtsSampleRate,
    /// Decoded `rate` targeted bit rate.
    pub bit_rate: DtsBitRate,
    /// `mix` — embedded down-mix coefficients present.
    pub mix: bool,
    /// `dynf` — embedded dynamic-range data present.
    pub dynamic_range: bool,
    /// `timef` — embedded time-stamp present.
    pub time_stamp: bool,
    /// `auxf` — auxiliary byte count present.
    pub aux_data: bool,
    /// `hdcd` — the source PCM was HDCD-format.
    pub hdcd: bool,
}

impl DtsHeader {
    /// Decode a DTS core frame header from the start of `frame` (the
    /// first byte of a DTS elementary stream routed from
    /// [`crate::vob::DvdSubstream::Dts`]).
    ///
    /// Returns [`Error::InvalidUdf`] when the buffer is shorter than
    /// the 10-byte header or when the leading four bytes are not the
    /// `0x7FFE8001` sync word.
    pub fn parse(frame: &[u8]) -> Result<Self> {
        if frame.len() < 10 {
            return Err(Error::InvalidUdf("DTS core frame truncated (< 10 bytes)"));
        }
        let syncword = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
        if syncword != DTS_SYNC_WORD {
            return Err(Error::InvalidUdf(
                "DTS core frame: sync word is not 0x7FFE8001",
            ));
        }

        // The header fields start at byte 4, MSB-first.
        let mut bits = BitReader::new(&frame[4..]);
        let frame_type = if bits.read(1) != 0 {
            DtsFrameType::Normal
        } else {
            DtsFrameType::Termination
        };
        let short = bits.read(5) as u8;
        let crc_present = bits.read(1) != 0;
        let nblks = bits.read(7) as u8;
        let fsize = bits.read(14) as u16;
        let audio_mode = DtsAudioMode::from_code(bits.read(6) as u8);
        let sample_rate = DtsSampleRate::from_code(bits.read(4) as u8);
        let bit_rate = DtsBitRate::from_code(bits.read(5) as u8);
        let mix = bits.read(1) != 0;
        let dynamic_range = bits.read(1) != 0;
        let time_stamp = bits.read(1) != 0;
        let aux_data = bits.read(1) != 0;
        let hdcd = bits.read(1) != 0;

        Ok(Self {
            frame_type,
            short,
            crc_present,
            nblks,
            fsize,
            audio_mode,
            sample_rate,
            bit_rate,
            mix,
            dynamic_range,
            time_stamp,
            aux_data,
            hdcd,
        })
    }

    /// Frame size in bytes (`fsize + 1`). For DVD this is 1006 for a
    /// 768 kbps stream or 2013 for a 1536 kbps stream.
    pub fn frame_size_bytes(self) -> u32 {
        self.fsize as u32 + 1
    }

    /// Number of 32-sample core blocks in the frame (`nblks + 1`). For
    /// DVD this is 16, so each frame carries 16 × 32 = 512 samples.
    pub fn sample_block_count(self) -> u16 {
        self.nblks as u16 + 1
    }

    /// Total PCM samples per channel carried by the frame
    /// (`sample_block_count × 32`).
    pub fn sample_count(self) -> u32 {
        self.sample_block_count() as u32 * 32
    }

    /// Sample rate in Hz for the nine defined `sfreq` codes; `None`
    /// when `sfreq` is reserved.
    pub fn sample_rate_hz(self) -> Option<u32> {
        self.sample_rate.hz()
    }

    /// Number of audio channels from the `amode` arrangement; `None`
    /// for a user-defined `amode` code.
    pub fn channel_count(self) -> Option<u8> {
        self.audio_mode.channel_count()
    }

    /// Targeted bit rate in kbps for the two DVD-Video `rate` codes;
    /// `None` for any other code.
    pub fn targeted_bitrate_kbps(self) -> Option<u16> {
        self.bit_rate.targeted_kbps()
    }
}

/// A minimal MSB-first bit reader over a byte slice. Reads past the
/// end of the buffer yield zero bits — the [`DtsHeader::parse`] caller
/// has already guaranteed at least 10 bytes, which covers every header
/// field, so the saturating behaviour never fabricates a meaningful
/// field here.
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Read `n` (≤ 32) bits MSB-first and return them right-aligned.
    fn read(&mut self, n: usize) -> u32 {
        let mut out = 0u32;
        for _ in 0..n {
            let byte_idx = self.bit_pos >> 3;
            let bit_idx = 7 - (self.bit_pos & 7);
            let bit = self
                .data
                .get(byte_idx)
                .map(|b| (b >> bit_idx) & 1)
                .unwrap_or(0);
            out = (out << 1) | u32::from(bit);
            self.bit_pos += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a 10-byte DTS core frame header from the field values,
    /// packing the bit fields MSB-first after the 4-byte sync word.
    #[allow(clippy::too_many_arguments)]
    fn build_frame(
        ftype: u8,
        short: u8,
        cpf: u8,
        nblks: u8,
        fsize: u16,
        amode: u8,
        sfreq: u8,
        rate: u8,
        flags: u8, // mix dynf timef auxf hdcd packed into low 5 bits
    ) -> Vec<u8> {
        let mut f = vec![0x7F, 0xFE, 0x80, 0x01];

        let mut bitbuf: Vec<bool> = Vec::new();
        let push = |bb: &mut Vec<bool>, val: u32, n: usize| {
            for i in (0..n).rev() {
                bb.push((val >> i) & 1 != 0);
            }
        };
        push(&mut bitbuf, ftype as u32, 1);
        push(&mut bitbuf, short as u32, 5);
        push(&mut bitbuf, cpf as u32, 1);
        push(&mut bitbuf, nblks as u32, 7);
        push(&mut bitbuf, fsize as u32, 14);
        push(&mut bitbuf, amode as u32, 6);
        push(&mut bitbuf, sfreq as u32, 4);
        push(&mut bitbuf, rate as u32, 5);
        push(&mut bitbuf, (flags >> 4) as u32 & 1, 1); // mix
        push(&mut bitbuf, (flags >> 3) as u32 & 1, 1); // dynf
        push(&mut bitbuf, (flags >> 2) as u32 & 1, 1); // timef
        push(&mut bitbuf, (flags >> 1) as u32 & 1, 1); // auxf
        push(&mut bitbuf, flags as u32 & 1, 1); // hdcd

        // 48 header bits → exactly 6 bytes.
        for chunk in bitbuf.chunks(8) {
            let mut b = 0u8;
            for (i, &bit) in chunk.iter().enumerate() {
                if bit {
                    b |= 1 << (7 - i);
                }
            }
            f.push(b);
        }
        while f.len() < 10 {
            f.push(0);
        }
        f
    }

    #[test]
    fn parse_dvd_768kbps_5_1() {
        // Normal frame, short=31, no CRC, nblks=15, fsize=1005 (1006 bytes),
        // amode=0x09 (5 ch), sfreq=0xD (48 kHz), rate=0x0F (768 kbps), no flags.
        let f = build_frame(1, 31, 0, 15, 1005, 0x09, 0xD, 0x0F, 0);
        let h = DtsHeader::parse(&f).unwrap();
        assert_eq!(h.frame_type, DtsFrameType::Normal);
        assert_eq!(h.short, 31);
        assert!(!h.crc_present);
        assert_eq!(h.nblks, 15);
        assert_eq!(h.sample_block_count(), 16);
        assert_eq!(h.sample_count(), 512);
        assert_eq!(h.fsize, 1005);
        assert_eq!(h.frame_size_bytes(), 1006);
        assert_eq!(h.audio_mode, DtsAudioMode::FiveChannel);
        assert_eq!(h.channel_count(), Some(5));
        assert_eq!(h.sample_rate, DtsSampleRate::Hz48000);
        assert_eq!(h.sample_rate_hz(), Some(48_000));
        assert_eq!(h.bit_rate, DtsBitRate::Kbps768);
        assert_eq!(h.targeted_bitrate_kbps(), Some(768));
        assert!(!h.mix && !h.dynamic_range && !h.time_stamp && !h.aux_data && !h.hdcd);
    }

    #[test]
    fn parse_dvd_1536kbps_with_flags() {
        // fsize=2012 (2013 bytes), rate=0x18 (1536 kbps), all five flags set.
        let f = build_frame(1, 31, 0, 15, 2012, 0x02, 0xD, 0x18, 0b1_1111);
        let h = DtsHeader::parse(&f).unwrap();
        assert_eq!(h.fsize, 2012);
        assert_eq!(h.frame_size_bytes(), 2013);
        assert_eq!(h.bit_rate, DtsBitRate::Kbps1536);
        assert_eq!(h.targeted_bitrate_kbps(), Some(1536));
        assert_eq!(h.audio_mode, DtsAudioMode::Stereo);
        assert_eq!(h.channel_count(), Some(2));
        assert!(h.mix && h.dynamic_range && h.time_stamp && h.aux_data && h.hdcd);
    }

    #[test]
    fn termination_frame() {
        // ftype=0, short=20 → falls 20 samples short of 32.
        let f = build_frame(0, 20, 0, 5, 600, 0x00, 0xD, 0x0F, 0);
        let h = DtsHeader::parse(&f).unwrap();
        assert_eq!(h.frame_type, DtsFrameType::Termination);
        assert_eq!(h.short, 20);
        assert_eq!(h.audio_mode, DtsAudioMode::Mono);
        assert_eq!(h.channel_count(), Some(1));
        assert_eq!(h.sample_block_count(), 6);
    }

    #[test]
    fn amode_channel_count_table() {
        let expected: [(u8, Option<u8>); 16] = [
            (0x00, Some(1)),
            (0x01, Some(2)),
            (0x02, Some(2)),
            (0x03, Some(2)),
            (0x04, Some(2)),
            (0x05, Some(3)),
            (0x06, Some(3)),
            (0x07, Some(4)),
            (0x08, Some(4)),
            (0x09, Some(5)),
            (0x0A, Some(6)),
            (0x0B, Some(6)),
            (0x0C, Some(6)),
            (0x0D, Some(7)),
            (0x0E, Some(8)),
            (0x0F, Some(8)),
        ];
        for (code, n) in expected {
            let m = DtsAudioMode::from_code(code);
            assert_eq!(m.code(), code);
            assert_eq!(m.channel_count(), n, "amode {code:#x}");
        }
    }

    #[test]
    fn amode_user_defined() {
        let m = DtsAudioMode::from_code(0x10);
        assert_eq!(m, DtsAudioMode::UserDefined(0x10));
        assert_eq!(m.channel_count(), None);
        assert_eq!(m.code(), 0x10);
        let m = DtsAudioMode::from_code(0x3F);
        assert_eq!(m, DtsAudioMode::UserDefined(0x3F));
        assert_eq!(m.channel_count(), None);
    }

    #[test]
    fn sfreq_table() {
        let valid: [(u8, u32); 9] = [
            (0x1, 8_000),
            (0x2, 16_000),
            (0x3, 32_000),
            (0x6, 11_025),
            (0x7, 22_050),
            (0x8, 44_100),
            (0xB, 12_000),
            (0xC, 24_000),
            (0xD, 48_000),
        ];
        for (code, hz) in valid {
            assert_eq!(
                DtsSampleRate::from_code(code).hz(),
                Some(hz),
                "sfreq {code:#x}"
            );
        }
        for code in [0x0u8, 0x4, 0x5, 0x9, 0xA, 0xE, 0xF] {
            assert_eq!(DtsSampleRate::from_code(code), DtsSampleRate::Invalid);
            assert_eq!(DtsSampleRate::from_code(code).hz(), None);
        }
    }

    #[test]
    fn rate_other_code() {
        let f = build_frame(1, 31, 0, 15, 1005, 0x02, 0xD, 0x05, 0);
        let h = DtsHeader::parse(&f).unwrap();
        assert_eq!(h.bit_rate, DtsBitRate::Other(0x05));
        assert_eq!(h.targeted_bitrate_kbps(), None);
        assert_eq!(h.bit_rate.code(), 0x05);
    }

    #[test]
    fn reserved_sfreq_in_frame() {
        let f = build_frame(1, 31, 0, 15, 1005, 0x02, 0x0, 0x0F, 0);
        let h = DtsHeader::parse(&f).unwrap();
        assert_eq!(h.sample_rate, DtsSampleRate::Invalid);
        assert_eq!(h.sample_rate_hz(), None);
    }

    #[test]
    fn rejects_bad_syncword() {
        let mut f = build_frame(1, 31, 0, 15, 1005, 0x02, 0xD, 0x0F, 0);
        f[0] = 0x00;
        let err = DtsHeader::parse(&f).unwrap_err();
        assert!(matches!(err, Error::InvalidUdf(_)));
    }

    #[test]
    fn rejects_short_buffer() {
        let err = DtsHeader::parse(&[0x7F, 0xFE, 0x80, 0x01, 0, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, Error::InvalidUdf(_)));
    }

    #[test]
    fn crc_present_flag() {
        let f = build_frame(1, 31, 1, 15, 1005, 0x02, 0xD, 0x0F, 0);
        let h = DtsHeader::parse(&f).unwrap();
        assert!(h.crc_present);
    }
}
