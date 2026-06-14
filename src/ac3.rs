//! DVD-Video AC-3 (Dolby Digital) sync-frame header decoder.
//!
//! On DVD-Video, AC-3 audio is carried inside MPEG-PS
//! `private_stream_1` (`stream_id = 0xBD`) PES packets under the
//! `0x80..=0x87` substream allocation that the VOB demuxer already
//! routes (see [`crate::vob::DvdSubstream::Ac3`]). The demuxer hands
//! the elementary stream to a downstream audio decoder as raw bytes;
//! the very first bytes of that stream are an AC-3 *sync frame*, whose
//! `syncinfo()` + the leading fixed-position fields of `bsi()` pin the
//! sample rate, the frame size, the nominal bit rate, and the channel
//! layout. This module decodes those header fields so a player can
//! size buffers, label the track, and seek to a frame boundary without
//! pulling in a full AC-3 audio decoder.
//!
//! ## Scope
//!
//! The decode covers the `syncinfo()` (sync word, `crc1`, `fscod`,
//! `frmsizecod`) and the deterministically-positioned prefix of
//! `bsi()` — `bsid`, `bsmod`, `acmod`, and the four conditional
//! mix-level / surround-mode fields whose presence is a pure function
//! of `acmod` (`cmixlev`, `surmixlev`, `dsurmod`), plus `lfeon`. After
//! `lfeon` the `bsi()` layout becomes variable-length (conditional
//! fields gated by their own flag bits), which a header-only reader
//! cannot traverse without a full bit-budget walk; those fields are
//! out of scope and the raw frame bytes stay available to the audio
//! decoder.
//!
//! The decode is read-only and allocation-free.
//!
//! ## Clean-room references
//!
//! - `docs/container/dvd/application/stnsoft-ac3hdr.html` — the
//!   `syncinfo()` field layout, the `fscod` sampling-rate table, the
//!   `frmsizecod` frame-size / nominal-bit-rate table (16-bit words
//!   per sync frame at each of the three sample rates), and the
//!   `bsi()` field order with the `acmod` audio-coding-mode table,
//!   the `cmixlev` / `surmixlev` / `dsurmod` conditional-presence
//!   rules, and the `bsmod` bitstream-mode table.
//! - `docs/container/dvd/application/mpucoder-dvdmpeg.html` — the
//!   `0x80..=0x87` substream allocation that locates the AC-3
//!   elementary stream inside the `private_stream_1` PES payload.
//!
//! Field layouts derive from the `stnsoft-ac3hdr.html` reference cited
//! above.

use crate::error::{Error, Result};

/// AC-3 sync word — the big-endian `0x0B77` at the start of every
/// sync frame.
pub const AC3_SYNC_WORD: u16 = 0x0B77;

/// Sampling rate carried in the 2-bit `fscod` field.
///
/// `00 = 48 kHz`, `01 = 44.1 kHz`, `10 = 32 kHz`, `11 = reserved`.
/// The `Reserved` variant marks a malformed / future code without
/// losing the fact that the field was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ac3SampleRate {
    /// 48 kHz (`fscod = 00`).
    Hz48000,
    /// 44.1 kHz (`fscod = 01`).
    Hz44100,
    /// 32 kHz (`fscod = 10`).
    Hz32000,
    /// Reserved (`fscod = 11`).
    Reserved,
}

impl Ac3SampleRate {
    fn from_code(code: u8) -> Self {
        match code & 0b11 {
            0 => Self::Hz48000,
            1 => Self::Hz44100,
            2 => Self::Hz32000,
            _ => Self::Reserved,
        }
    }

    /// Sample rate in Hz for the three defined codes; `None` for
    /// [`Self::Reserved`].
    pub fn hz(self) -> Option<u32> {
        match self {
            Self::Hz48000 => Some(48_000),
            Self::Hz44100 => Some(44_100),
            Self::Hz32000 => Some(32_000),
            Self::Reserved => None,
        }
    }
}

/// Audio coding mode carried in the 3-bit `acmod` field.
///
/// The enum names the speaker layout in the spec's `front/surround`
/// notation; [`Self::channel_count`] returns `nfchans` (the count of
/// full-bandwidth channels, *excluding* the optional LFE channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ac3AudioCodingMode {
    /// `000` — 1+1 (two independent mono channels, Ch1 + Ch2).
    DualMono,
    /// `001` — 1/0 (centre only).
    Mono,
    /// `010` — 2/0 (left + right stereo).
    Stereo,
    /// `011` — 3/0 (left, centre, right).
    ThreeZero,
    /// `100` — 2/1 (left, right, mono surround).
    TwoOne,
    /// `101` — 3/1 (left, centre, right, mono surround).
    ThreeOne,
    /// `110` — 2/2 (left, right, left-surround, right-surround).
    TwoTwo,
    /// `111` — 3/2 (left, centre, right, left-surround,
    /// right-surround).
    ThreeTwo,
}

impl Ac3AudioCodingMode {
    fn from_code(code: u8) -> Self {
        match code & 0b111 {
            0 => Self::DualMono,
            1 => Self::Mono,
            2 => Self::Stereo,
            3 => Self::ThreeZero,
            4 => Self::TwoOne,
            5 => Self::ThreeOne,
            6 => Self::TwoTwo,
            _ => Self::ThreeTwo,
        }
    }

    /// The raw 3-bit `acmod` code.
    pub fn code(self) -> u8 {
        match self {
            Self::DualMono => 0,
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::ThreeZero => 3,
            Self::TwoOne => 4,
            Self::ThreeOne => 5,
            Self::TwoTwo => 6,
            Self::ThreeTwo => 7,
        }
    }

    /// `nfchans` — the number of full-bandwidth channels, excluding
    /// any LFE. (Add 1 to this for the total channel count when
    /// [`Ac3Header::lfe_on`] is `true`.)
    pub fn channel_count(self) -> u8 {
        match self {
            Self::DualMono => 2,
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::ThreeZero => 3,
            Self::TwoOne => 3,
            Self::ThreeOne => 4,
            Self::TwoTwo => 4,
            Self::ThreeTwo => 5,
        }
    }

    /// `true` for the three modes (`3/0`, `3/1`, `3/2`) that carry a
    /// centre channel and therefore a `cmixlev` field — per the
    /// `(acmod & 0x1) && (acmod != 0x1)` rule in the BSI layout.
    pub fn has_center_mix_level(self) -> bool {
        let code = self.code();
        (code & 0x1) != 0 && code != 0x1
    }

    /// `true` for the four modes (`2/1`, `3/1`, `2/2`, `3/2`) that
    /// carry a surround channel and therefore a `surmixlev` field —
    /// per the `acmod & 0x4` rule in the BSI layout.
    pub fn has_surround_mix_level(self) -> bool {
        (self.code() & 0x4) != 0
    }

    /// `true` only for the `2/0` stereo mode, which carries the
    /// `dsurmod` Dolby Surround flag — per the `acmod == 0x2` rule.
    pub fn has_dolby_surround_mode(self) -> bool {
        self.code() == 0x2
    }
}

/// Bitstream mode (`bsmod`) — the type of audio service the frame
/// carries. The `VoiceOverOrKaraoke` variant covers the `111` code whose
/// meaning further depends on `acmod` (voice-over for `acmod = 001`,
/// otherwise a main / karaoke service); the raw code is preserved for
/// a caller that wants to disambiguate against `acmod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ac3BitstreamMode {
    /// `000` — main audio service: complete main (CM).
    CompleteMain,
    /// `001` — main audio service: music and effects (ME).
    MusicAndEffects,
    /// `010` — associated service: visually impaired (VI).
    VisuallyImpaired,
    /// `011` — associated service: hearing impaired (HI).
    HearingImpaired,
    /// `100` — associated service: dialogue (D).
    Dialogue,
    /// `101` — associated service: commentary (C).
    Commentary,
    /// `110` — associated service: emergency (E).
    Emergency,
    /// `111` — voice-over (when `acmod == 001`) or a main /
    /// karaoke service (otherwise). Disambiguate via `acmod`.
    VoiceOverOrKaraoke,
}

impl Ac3BitstreamMode {
    fn from_code(code: u8) -> Self {
        match code & 0b111 {
            0 => Self::CompleteMain,
            1 => Self::MusicAndEffects,
            2 => Self::VisuallyImpaired,
            3 => Self::HearingImpaired,
            4 => Self::Dialogue,
            5 => Self::Commentary,
            6 => Self::Emergency,
            _ => Self::VoiceOverOrKaraoke,
        }
    }
}

/// One row of the `frmsizecod` table: the nominal bit rate and the
/// frame size, in 16-bit words, at each of the three sample rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrmSizeRow {
    /// Nominal bit rate in kbps.
    bitrate_kbps: u16,
    /// 16-bit words per sync frame at 32 kHz.
    words_32k: u16,
    /// 16-bit words per sync frame at 44.1 kHz.
    words_44k: u16,
    /// 16-bit words per sync frame at 48 kHz.
    words_48k: u16,
}

/// The `frmsizecod` table from `stnsoft-ac3hdr.html`. The 6-bit code
/// indexes this 38-entry table directly (`0b000000..=0b100101`);
/// codes `0b100110..=0b111111` are reserved and absent from the
/// table.
const FRM_SIZE_TABLE: [FrmSizeRow; 38] = [
    FrmSizeRow {
        bitrate_kbps: 32,
        words_32k: 96,
        words_44k: 69,
        words_48k: 64,
    },
    FrmSizeRow {
        bitrate_kbps: 32,
        words_32k: 96,
        words_44k: 70,
        words_48k: 64,
    },
    FrmSizeRow {
        bitrate_kbps: 40,
        words_32k: 120,
        words_44k: 87,
        words_48k: 80,
    },
    FrmSizeRow {
        bitrate_kbps: 40,
        words_32k: 120,
        words_44k: 88,
        words_48k: 80,
    },
    FrmSizeRow {
        bitrate_kbps: 48,
        words_32k: 144,
        words_44k: 104,
        words_48k: 96,
    },
    FrmSizeRow {
        bitrate_kbps: 48,
        words_32k: 144,
        words_44k: 105,
        words_48k: 96,
    },
    FrmSizeRow {
        bitrate_kbps: 56,
        words_32k: 168,
        words_44k: 121,
        words_48k: 112,
    },
    FrmSizeRow {
        bitrate_kbps: 56,
        words_32k: 168,
        words_44k: 122,
        words_48k: 112,
    },
    FrmSizeRow {
        bitrate_kbps: 64,
        words_32k: 192,
        words_44k: 139,
        words_48k: 128,
    },
    FrmSizeRow {
        bitrate_kbps: 64,
        words_32k: 192,
        words_44k: 140,
        words_48k: 128,
    },
    FrmSizeRow {
        bitrate_kbps: 80,
        words_32k: 240,
        words_44k: 174,
        words_48k: 160,
    },
    FrmSizeRow {
        bitrate_kbps: 80,
        words_32k: 240,
        words_44k: 175,
        words_48k: 160,
    },
    FrmSizeRow {
        bitrate_kbps: 96,
        words_32k: 288,
        words_44k: 208,
        words_48k: 192,
    },
    FrmSizeRow {
        bitrate_kbps: 96,
        words_32k: 288,
        words_44k: 209,
        words_48k: 192,
    },
    FrmSizeRow {
        bitrate_kbps: 112,
        words_32k: 336,
        words_44k: 243,
        words_48k: 224,
    },
    FrmSizeRow {
        bitrate_kbps: 112,
        words_32k: 336,
        words_44k: 244,
        words_48k: 224,
    },
    FrmSizeRow {
        bitrate_kbps: 128,
        words_32k: 384,
        words_44k: 278,
        words_48k: 256,
    },
    FrmSizeRow {
        bitrate_kbps: 128,
        words_32k: 384,
        words_44k: 279,
        words_48k: 256,
    },
    FrmSizeRow {
        bitrate_kbps: 160,
        words_32k: 480,
        words_44k: 348,
        words_48k: 320,
    },
    FrmSizeRow {
        bitrate_kbps: 160,
        words_32k: 480,
        words_44k: 349,
        words_48k: 320,
    },
    FrmSizeRow {
        bitrate_kbps: 192,
        words_32k: 576,
        words_44k: 417,
        words_48k: 384,
    },
    FrmSizeRow {
        bitrate_kbps: 192,
        words_32k: 576,
        words_44k: 418,
        words_48k: 384,
    },
    FrmSizeRow {
        bitrate_kbps: 224,
        words_32k: 672,
        words_44k: 487,
        words_48k: 448,
    },
    FrmSizeRow {
        bitrate_kbps: 224,
        words_32k: 672,
        words_44k: 488,
        words_48k: 448,
    },
    FrmSizeRow {
        bitrate_kbps: 256,
        words_32k: 768,
        words_44k: 557,
        words_48k: 512,
    },
    FrmSizeRow {
        bitrate_kbps: 256,
        words_32k: 768,
        words_44k: 558,
        words_48k: 512,
    },
    FrmSizeRow {
        bitrate_kbps: 320,
        words_32k: 960,
        words_44k: 696,
        words_48k: 640,
    },
    FrmSizeRow {
        bitrate_kbps: 320,
        words_32k: 960,
        words_44k: 697,
        words_48k: 640,
    },
    FrmSizeRow {
        bitrate_kbps: 384,
        words_32k: 1152,
        words_44k: 835,
        words_48k: 768,
    },
    FrmSizeRow {
        bitrate_kbps: 384,
        words_32k: 1152,
        words_44k: 836,
        words_48k: 768,
    },
    FrmSizeRow {
        bitrate_kbps: 448,
        words_32k: 1344,
        words_44k: 975,
        words_48k: 896,
    },
    FrmSizeRow {
        bitrate_kbps: 448,
        words_32k: 1344,
        words_44k: 976,
        words_48k: 896,
    },
    FrmSizeRow {
        bitrate_kbps: 512,
        words_32k: 1536,
        words_44k: 1114,
        words_48k: 1024,
    },
    FrmSizeRow {
        bitrate_kbps: 512,
        words_32k: 1536,
        words_44k: 1115,
        words_48k: 1024,
    },
    FrmSizeRow {
        bitrate_kbps: 576,
        words_32k: 1728,
        words_44k: 1253,
        words_48k: 1152,
    },
    FrmSizeRow {
        bitrate_kbps: 576,
        words_32k: 1728,
        words_44k: 1254,
        words_48k: 1152,
    },
    FrmSizeRow {
        bitrate_kbps: 640,
        words_32k: 1920,
        words_44k: 1393,
        words_48k: 1280,
    },
    FrmSizeRow {
        bitrate_kbps: 640,
        words_32k: 1920,
        words_44k: 1394,
        words_48k: 1280,
    },
];

/// Decoded AC-3 sync-frame header (`syncinfo()` + the fixed-position
/// prefix of `bsi()`).
///
/// Field layout from `stnsoft-ac3hdr.html`:
///
/// ```text
/// syncinfo()
///   syncword   16  0x0B77
///   crc1       16  CRC of the first 5/8 of the frame
///   fscod       2  sampling-rate code
///   frmsizecod  6  frame-size code
/// bsi()  (fixed-position prefix)
///   bsid        5  bitstream identification (8 in this version)
///   bsmod       3  bitstream mode
///   acmod       3  audio coding mode
///   [cmixlev    2] if acmod has a centre channel
///   [surmixlev  2] if acmod has a surround channel
///   [dsurmod    2] if acmod == 2/0
///   lfeon       1  LFE channel present
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ac3Header {
    /// 16-bit CRC over the first 5/8 of the sync frame (carried, not
    /// verified — the decoder is header-only).
    pub crc1: u16,
    /// Decoded sampling rate (`fscod`).
    pub sample_rate: Ac3SampleRate,
    /// Raw 6-bit `frmsizecod` frame-size code.
    pub frame_size_code: u8,
    /// 5-bit `bsid` bitstream identification (8 for the standard
    /// A/52 stream, 6 for the alternate A/52a layout).
    pub bsid: u8,
    /// Decoded bitstream mode (`bsmod`).
    pub bitstream_mode: Ac3BitstreamMode,
    /// Decoded audio coding mode (`acmod`).
    pub audio_coding_mode: Ac3AudioCodingMode,
    /// 2-bit `cmixlev` centre-mix level, present only when
    /// [`Ac3AudioCodingMode::has_center_mix_level`].
    pub center_mix_level: Option<u8>,
    /// 2-bit `surmixlev` surround-mix level, present only when
    /// [`Ac3AudioCodingMode::has_surround_mix_level`].
    pub surround_mix_level: Option<u8>,
    /// 2-bit `dsurmod` Dolby Surround mode, present only for the
    /// `2/0` mode (`acmod == 2`).
    pub dolby_surround_mode: Option<u8>,
    /// `lfeon` — `true` when a low-frequency-effects channel is
    /// present.
    pub lfe_on: bool,
}

impl Ac3Header {
    /// Decode an AC-3 sync-frame header from the start of `frame`
    /// (the first byte of an AC-3 elementary stream routed from
    /// [`crate::vob::DvdSubstream::Ac3`]).
    ///
    /// Returns [`Error::InvalidUdf`] when the buffer is too short to
    /// reach `lfeon` or when the leading two bytes are not the
    /// `0x0B77` sync word.
    pub fn parse(frame: &[u8]) -> Result<Self> {
        // syncinfo() is 5 bytes; bsi() up to lfeon adds at most 5
        // more bits past byte 5's bit boundary, so 7 bytes always
        // covers the deterministic prefix regardless of acmod.
        if frame.len() < 7 {
            return Err(Error::InvalidUdf("AC-3 sync frame truncated (< 7 bytes)"));
        }
        let syncword = u16::from_be_bytes([frame[0], frame[1]]);
        if syncword != AC3_SYNC_WORD {
            return Err(Error::InvalidUdf(
                "AC-3 sync frame: sync word is not 0x0B77",
            ));
        }
        let crc1 = u16::from_be_bytes([frame[2], frame[3]]);

        let byte4 = frame[4];
        let sample_rate = Ac3SampleRate::from_code(byte4 >> 6);
        let frame_size_code = byte4 & 0b0011_1111;

        // bsi() begins at byte 5, MSB-first. A small bit cursor walks
        // the deterministic prefix (bsid .. lfeon).
        let mut bits = BitReader::new(&frame[5..]);
        let bsid = bits.read(5);
        let bitstream_mode = Ac3BitstreamMode::from_code(bits.read(3));
        let audio_coding_mode = Ac3AudioCodingMode::from_code(bits.read(3));

        let center_mix_level = if audio_coding_mode.has_center_mix_level() {
            Some(bits.read(2))
        } else {
            None
        };
        let surround_mix_level = if audio_coding_mode.has_surround_mix_level() {
            Some(bits.read(2))
        } else {
            None
        };
        let dolby_surround_mode = if audio_coding_mode.has_dolby_surround_mode() {
            Some(bits.read(2))
        } else {
            None
        };
        let lfe_on = bits.read(1) != 0;

        Ok(Self {
            crc1,
            sample_rate,
            frame_size_code,
            bsid,
            bitstream_mode,
            audio_coding_mode,
            center_mix_level,
            surround_mix_level,
            dolby_surround_mode,
            lfe_on,
        })
    }

    /// Sample rate in Hz for the three defined `fscod` codes; `None`
    /// when `fscod == 11` (reserved).
    pub fn sample_rate_hz(self) -> Option<u32> {
        self.sample_rate.hz()
    }

    /// Nominal bit rate in kbps from the `frmsizecod` table; `None`
    /// for the reserved codes (`>= 0b100110`).
    pub fn nominal_bitrate_kbps(self) -> Option<u16> {
        FRM_SIZE_TABLE
            .get(self.frame_size_code as usize)
            .map(|r| r.bitrate_kbps)
    }

    /// Sync-frame size in 16-bit words from the `frmsizecod` table,
    /// selected by the decoded sample rate. `None` for a reserved
    /// `frmsizecod` or a reserved `fscod`.
    pub fn frame_size_words(self) -> Option<u16> {
        let row = FRM_SIZE_TABLE.get(self.frame_size_code as usize)?;
        match self.sample_rate {
            Ac3SampleRate::Hz32000 => Some(row.words_32k),
            Ac3SampleRate::Hz44100 => Some(row.words_44k),
            Ac3SampleRate::Hz48000 => Some(row.words_48k),
            Ac3SampleRate::Reserved => None,
        }
    }

    /// Sync-frame size in bytes (`frame_size_words × 2`); `None` when
    /// [`Self::frame_size_words`] is `None`.
    pub fn frame_size_bytes(self) -> Option<u32> {
        self.frame_size_words().map(|w| w as u32 * 2)
    }

    /// Total channel count including the LFE channel when present
    /// (`nfchans + lfeon`).
    pub fn total_channel_count(self) -> u8 {
        self.audio_coding_mode.channel_count() + u8::from(self.lfe_on)
    }
}

/// A minimal MSB-first bit reader over a byte slice. Reads past the
/// end of the buffer yield zero bits — the [`Ac3Header::parse`] caller
/// has already guaranteed at least 7 bytes, which covers every
/// deterministic-prefix path, so the saturating behaviour never
/// fabricates a meaningful field here.
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Read `n` (≤ 8) bits MSB-first and return them right-aligned.
    fn read(&mut self, n: usize) -> u8 {
        let mut out = 0u8;
        for _ in 0..n {
            let byte_idx = self.bit_pos >> 3;
            let bit_idx = 7 - (self.bit_pos & 7);
            let bit = self
                .data
                .get(byte_idx)
                .map(|b| (b >> bit_idx) & 1)
                .unwrap_or(0);
            out = (out << 1) | bit;
            self.bit_pos += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a sync frame whose `syncinfo()` declares 48 kHz, the
    /// given `frmsizecod`, and a `bsi()` prefix of `bsid=8`,
    /// `bsmod=0`, the given `acmod`, no conditional mix fields beyond
    /// what `acmod` forces, and `lfeon` from `lfe`.
    fn build_frame(
        fscod: u8,
        frmsizecod: u8,
        bsid: u8,
        bsmod: u8,
        acmod: u8,
        lfe: bool,
    ) -> Vec<u8> {
        let mut f = vec![0x0B, 0x77, 0x12, 0x34];
        f.push((fscod << 6) | (frmsizecod & 0b0011_1111));

        // Assemble bsi() prefix MSB-first into a bit buffer.
        let mut bitbuf: Vec<bool> = Vec::new();
        let push = |bb: &mut Vec<bool>, val: u8, n: usize| {
            for i in (0..n).rev() {
                bb.push((val >> i) & 1 != 0);
            }
        };
        push(&mut bitbuf, bsid, 5);
        push(&mut bitbuf, bsmod, 3);
        push(&mut bitbuf, acmod, 3);
        let acm = Ac3AudioCodingMode::from_code(acmod);
        if acm.has_center_mix_level() {
            push(&mut bitbuf, 0b01, 2);
        }
        if acm.has_surround_mix_level() {
            push(&mut bitbuf, 0b10, 2);
        }
        if acm.has_dolby_surround_mode() {
            push(&mut bitbuf, 0b10, 2);
        }
        bitbuf.push(lfe);
        // Pack the bit buffer into bytes (MSB-first), padding the tail.
        while bitbuf.len() % 8 != 0 {
            bitbuf.push(false);
        }
        for chunk in bitbuf.chunks(8) {
            let mut b = 0u8;
            for (i, &bit) in chunk.iter().enumerate() {
                if bit {
                    b |= 1 << (7 - i);
                }
            }
            f.push(b);
        }
        // Ensure at least 7 bytes total.
        while f.len() < 7 {
            f.push(0);
        }
        f
    }

    #[test]
    fn parse_stereo_48k() {
        // 48 kHz, frmsizecod=0 (32 kbps), bsid=8, bsmod=0, acmod=2 (2/0), no LFE.
        let f = build_frame(0, 0, 8, 0, 2, false);
        let h = Ac3Header::parse(&f).unwrap();
        assert_eq!(h.crc1, 0x1234);
        assert_eq!(h.sample_rate, Ac3SampleRate::Hz48000);
        assert_eq!(h.sample_rate_hz(), Some(48_000));
        assert_eq!(h.frame_size_code, 0);
        assert_eq!(h.bsid, 8);
        assert_eq!(h.bitstream_mode, Ac3BitstreamMode::CompleteMain);
        assert_eq!(h.audio_coding_mode, Ac3AudioCodingMode::Stereo);
        // 2/0 has dsurmod but no cmixlev / surmixlev.
        assert_eq!(h.center_mix_level, None);
        assert_eq!(h.surround_mix_level, None);
        assert_eq!(h.dolby_surround_mode, Some(0b10));
        assert!(!h.lfe_on);
        assert_eq!(h.total_channel_count(), 2);
        assert_eq!(h.nominal_bitrate_kbps(), Some(32));
        assert_eq!(h.frame_size_words(), Some(64));
        assert_eq!(h.frame_size_bytes(), Some(128));
    }

    #[test]
    fn parse_five_one_48k() {
        // acmod=7 (3/2) + LFE → 5.1. frmsizecod=24 → 256 kbps.
        let f = build_frame(0, 24, 8, 0, 7, true);
        let h = Ac3Header::parse(&f).unwrap();
        assert_eq!(h.audio_coding_mode, Ac3AudioCodingMode::ThreeTwo);
        // 3/2 carries both cmixlev and surmixlev, no dsurmod.
        assert_eq!(h.center_mix_level, Some(0b01));
        assert_eq!(h.surround_mix_level, Some(0b10));
        assert_eq!(h.dolby_surround_mode, None);
        assert!(h.lfe_on);
        assert_eq!(h.audio_coding_mode.channel_count(), 5);
        assert_eq!(h.total_channel_count(), 6);
        assert_eq!(h.nominal_bitrate_kbps(), Some(256));
        assert_eq!(h.frame_size_words(), Some(512));
    }

    #[test]
    fn frmsizecod_table_sample_rate_columns() {
        // frmsizecod=26 (320 kbps) at each sample rate.
        let at = |fscod: u8| Ac3Header::parse(&build_frame(fscod, 26, 8, 0, 2, false)).unwrap();
        let h48 = at(0);
        assert_eq!(h48.sample_rate_hz(), Some(48_000));
        assert_eq!(h48.frame_size_words(), Some(640));
        let h44 = at(1);
        assert_eq!(h44.sample_rate_hz(), Some(44_100));
        assert_eq!(h44.frame_size_words(), Some(696));
        let h32 = at(2);
        assert_eq!(h32.sample_rate_hz(), Some(32_000));
        assert_eq!(h32.frame_size_words(), Some(960));
    }

    #[test]
    fn reserved_fscod_yields_none() {
        let h = Ac3Header::parse(&build_frame(3, 0, 8, 0, 2, false)).unwrap();
        assert_eq!(h.sample_rate, Ac3SampleRate::Reserved);
        assert_eq!(h.sample_rate_hz(), None);
        // frame_size_words needs a defined sample rate.
        assert_eq!(h.frame_size_words(), None);
        // but the nominal bitrate is sample-rate-independent.
        assert_eq!(h.nominal_bitrate_kbps(), Some(32));
    }

    #[test]
    fn reserved_frmsizecod_yields_none() {
        // frmsizecod=0b100110 (38) is the first reserved code.
        let h = Ac3Header::parse(&build_frame(0, 38, 8, 0, 2, false)).unwrap();
        assert_eq!(h.frame_size_code, 38);
        assert_eq!(h.nominal_bitrate_kbps(), None);
        assert_eq!(h.frame_size_words(), None);
        assert_eq!(h.frame_size_bytes(), None);
    }

    #[test]
    fn all_acmod_channel_counts() {
        let expected = [2u8, 1, 2, 3, 3, 4, 4, 5];
        for (code, &n) in expected.iter().enumerate() {
            assert_eq!(Ac3AudioCodingMode::from_code(code as u8).channel_count(), n);
        }
    }

    #[test]
    fn conditional_field_presence_by_acmod() {
        // Centre present for 3/0(3), 3/1(5), 3/2(7).
        for code in [3u8, 5, 7] {
            assert!(Ac3AudioCodingMode::from_code(code).has_center_mix_level());
        }
        for code in [0u8, 1, 2, 4, 6] {
            assert!(!Ac3AudioCodingMode::from_code(code).has_center_mix_level());
        }
        // Surround present for 2/1(4), 3/1(5), 2/2(6), 3/2(7).
        for code in [4u8, 5, 6, 7] {
            assert!(Ac3AudioCodingMode::from_code(code).has_surround_mix_level());
        }
        for code in [0u8, 1, 2, 3] {
            assert!(!Ac3AudioCodingMode::from_code(code).has_surround_mix_level());
        }
        // dsurmod only for 2/0(2).
        assert!(Ac3AudioCodingMode::from_code(2).has_dolby_surround_mode());
        for code in [0u8, 1, 3, 4, 5, 6, 7] {
            assert!(!Ac3AudioCodingMode::from_code(code).has_dolby_surround_mode());
        }
    }

    #[test]
    fn bitstream_mode_table() {
        let modes = [
            Ac3BitstreamMode::CompleteMain,
            Ac3BitstreamMode::MusicAndEffects,
            Ac3BitstreamMode::VisuallyImpaired,
            Ac3BitstreamMode::HearingImpaired,
            Ac3BitstreamMode::Dialogue,
            Ac3BitstreamMode::Commentary,
            Ac3BitstreamMode::Emergency,
            Ac3BitstreamMode::VoiceOverOrKaraoke,
        ];
        for (code, &m) in modes.iter().enumerate() {
            let h = Ac3Header::parse(&build_frame(0, 0, 8, code as u8, 2, false)).unwrap();
            assert_eq!(h.bitstream_mode, m);
        }
    }

    #[test]
    fn rejects_bad_syncword() {
        let mut f = build_frame(0, 0, 8, 0, 2, false);
        f[0] = 0x00;
        let err = Ac3Header::parse(&f).unwrap_err();
        assert!(matches!(err, Error::InvalidUdf(_)));
    }

    #[test]
    fn rejects_short_buffer() {
        let err = Ac3Header::parse(&[0x0B, 0x77, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, Error::InvalidUdf(_)));
    }

    #[test]
    fn frmsizecod_table_is_complete() {
        // 38 defined codes, indices 0..=37.
        assert_eq!(FRM_SIZE_TABLE.len(), 38);
        // First and last bit rates per the table.
        assert_eq!(FRM_SIZE_TABLE[0].bitrate_kbps, 32);
        assert_eq!(FRM_SIZE_TABLE[37].bitrate_kbps, 640);
    }
}
