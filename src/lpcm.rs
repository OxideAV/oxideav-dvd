//! DVD-Video LPCM private_stream_1 audio-pack 7-byte header decoder.
//!
//! On DVD-Video, linear PCM audio is carried inside MPEG-PS
//! `private_stream_1` (`stream_id = 0xBD`) PES packets, the same
//! private-stream the VOB demuxer already routes for AC-3 / DTS /
//! subpicture (see [`crate::vob::DvdSubstream`]). The first byte of
//! every private-stream-1 payload is the substream selector
//! (`0xA0..=0xA7` for LPCM); the **next seven bytes** carry the
//! LPCM-specific audio-pack header that pins the sample format
//! (quantisation word length, sample rate, channel count), the
//! per-pack frame-counter book-keeping the seamless-playback path
//! needs, and the X/Y dynamic-range coefficients a decoder applies
//! to the raw PCM samples.
//!
//! ## Scope
//!
//! The header decode is bit-pure per the layout on
//! `docs/container/dvd/application/mpucoder-lpcm.html` and the
//! bitrate-feasibility table on
//! `docs/container/dvd/application/stnsoft-LimPcmAud.html`.
//!
//! For the **16-bit** quantisation case the raw PCM tail can be
//! unpacked into channel-interleaved signed samples via
//! [`LpcmHeader::unpack_samples_16bit`]: `mpucoder-lpcm.html` pins
//! the storage as most-significant-byte-first
//! ("first channel 0 (left) sample" leads the payload), interleaved
//! by channel in ascending order, so each pair reads as a big-endian
//! `i16`. The **20-bit and 24-bit** sub-byte grouping layout is *not*
//! specified by the staged reference pages — the LimPcmAud table
//! confirms those widths exist but neither page documents how the
//! extra nibble / byte of grouped samples is arranged — so the
//! unpacker is deliberately limited to 16-bit and returns `None`
//! otherwise (see the "Docs gap" note below). The dynamic-range
//! gain ([`LpcmHeader::linear_gain`] / [`LpcmHeader::gain_db`]) is
//! exposed as a coefficient the caller applies during mix-down.
//!
//! ## Docs gap
//!
//! Neither `mpucoder-lpcm.html` nor `stnsoft-LimPcmAud.html`
//! specifies the on-wire packing of 20-bit or 24-bit LPCM samples
//! (the order in which the grouped low-order bits are stored across
//! bytes). 16-bit packing is unambiguous (big-endian `i16`,
//! channel-interleaved) and fully covered here; 20/24-bit sample
//! reconstruction is blocked on a reference that pins the grouping.
//!
//! ## Clean-room references
//!
//! - `docs/container/dvd/application/mpucoder-lpcm.html` — 7-byte
//!   audio-pack header field layout, the `linear gain = 2^(4-(X+(Y/30)))`
//!   dynamic-range formula, and the dB-gain formula
//!   `24.082 - 6.0206 X - 0.2007 Y`.
//! - `docs/container/dvd/application/stnsoft-LimPcmAud.html` — the
//!   `48 kHz | 96 kHz × {16, 20, 24} bits × 1..=8 channels`
//!   bitrate table and the 6144 kbps DVD-Video ceiling that rejects
//!   the red-highlighted combinations.
//! - `docs/container/dvd/application/mpucoder-dvdmpeg.html` — the
//!   `0xA0..=0xA7` substream allocation that locates this header
//!   inside the PES payload (one byte after the substream selector).
//!
//! Field layouts derive from the two `mpucoder-*.html` references
//! cited above.

use crate::error::{Error, Result};

/// Sample-quantisation word length carried in bits 7..=6 of byte 5.
///
/// `0 = 16 bits`, `1 = 20 bits`, `2 = 24 bits`, `3 = reserved`. The
/// `Reserved` variant preserves the raw 2-bit code so a debugger can
/// surface a malformed disc without losing information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LpcmQuantisation {
    /// 16 bits per sample.
    Bits16,
    /// 20 bits per sample.
    Bits20,
    /// 24 bits per sample.
    Bits24,
    /// Reserved — the spec leaves code `3` undefined.
    Reserved,
}

impl LpcmQuantisation {
    fn from_code(code: u8) -> Self {
        match code & 0b11 {
            0 => Self::Bits16,
            1 => Self::Bits20,
            2 => Self::Bits24,
            _ => Self::Reserved,
        }
    }

    /// Bits per sample for the well-defined codes; `None` for
    /// [`Self::Reserved`].
    pub fn bits_per_sample(self) -> Option<u8> {
        match self {
            Self::Bits16 => Some(16),
            Self::Bits20 => Some(20),
            Self::Bits24 => Some(24),
            Self::Reserved => None,
        }
    }
}

/// Sample frequency carried in bits 5..=4 of byte 5.
///
/// `0 = 48 kHz`, `1 = 96 kHz`, `2/3 = reserved`. The `Reserved`
/// variant preserves the raw 2-bit code per the same rationale as
/// [`LpcmQuantisation::Reserved`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LpcmSampleFrequency {
    /// 48 kHz.
    Hz48000,
    /// 96 kHz.
    Hz96000,
    /// Reserved (codes 2 + 3).
    Reserved,
}

impl LpcmSampleFrequency {
    fn from_code(code: u8) -> Self {
        match code & 0b11 {
            0 => Self::Hz48000,
            1 => Self::Hz96000,
            _ => Self::Reserved,
        }
    }

    /// Sample frequency in Hz for the well-defined codes; `None`
    /// for [`Self::Reserved`].
    pub fn hz(self) -> Option<u32> {
        match self {
            Self::Hz48000 => Some(48_000),
            Self::Hz96000 => Some(96_000),
            Self::Reserved => None,
        }
    }
}

/// Decoded 7-byte LPCM audio-pack header.
///
/// Field layout from `mpucoder-lpcm.html`:
///
/// | Off | Field                       | Bits | Notes                               |
/// |-----|-----------------------------|------|-------------------------------------|
/// | 0   | `sub_stream_id`             | 8    | `1010 0xxx` — LPCM track `0..=7`.   |
/// | 1   | `number_of_frame_headers`   | 8    | Audio frames starting in this pack. |
/// | 2-3 | `first_access_unit_pointer` | 16   | First-frame byte offset for PES PTS.|
/// | 4   | `audio_emphasis_flag`       | 1    | Off / on.                           |
/// |     | `audio_mute_flag`           | 1    | Off / on.                           |
/// |     | reserved                    | 1    |                                     |
/// |     | `audio_frame_number`        | 5    | Frame index within group.           |
/// | 5   | `quantisation_word_length`  | 2    | `0/1/2/3 → 16/20/24/reserved`.      |
/// |     | `audio_sample_frequency`    | 2    | `0/1 → 48/96 kHz`.                  |
/// |     | reserved                    | 1    |                                     |
/// |     | `number_of_audio_channels`  | 3    | `code + 1` → 1..=8 channels.        |
/// | 6   | `dynamic_range X`           | 3    | High nibble of the gain coefficient.|
/// |     | `dynamic_range Y`           | 5    | Low bits of the gain coefficient.   |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LpcmHeader {
    /// `0xA0..=0xA7` LPCM substream selector (mirrored from byte 0
    /// of the header for round-trip purposes; the demuxer has
    /// already classified the substream by this point).
    pub sub_stream_id: u8,
    /// Number of audio frame headers whose first byte falls inside
    /// the enclosing PES packet's payload.
    pub number_of_frame_headers: u8,
    /// Byte offset of the first audio frame the PES PTS applies to.
    pub first_access_unit_pointer: u16,
    /// `true` ⇒ pre-emphasis was applied at encode time and the
    /// decoder should apply the matching de-emphasis curve.
    pub audio_emphasis_flag: bool,
    /// `true` ⇒ samples in this pack should be muted at playback.
    pub audio_mute_flag: bool,
    /// 5-bit frame counter within the current Group of Audio frames.
    pub audio_frame_number: u8,
    /// Decoded sample-quantisation word length.
    pub quantisation: LpcmQuantisation,
    /// Decoded sample frequency.
    pub sample_frequency: LpcmSampleFrequency,
    /// Channel count `1..=8` (the on-wire code carries `count - 1`).
    pub channel_count: u8,
    /// 3-bit `X` coefficient of the dynamic-range gain.
    pub dynamic_range_x: u8,
    /// 5-bit `Y` coefficient of the dynamic-range gain.
    pub dynamic_range_y: u8,
}

/// Length of an LPCM audio-pack header in bytes.
pub const LPCM_HEADER_LEN: usize = 7;

/// DVD-Video PCM bitrate ceiling (kbps) per `stnsoft-LimPcmAud.html`.
/// Combinations whose `channels × sample_rate × bits_per_sample`
/// exceeds this are physically unrepresentable on a DVD and the
/// table marks them in red.
pub const DVD_LPCM_MAX_BITRATE_KBPS: u32 = 6144;

impl LpcmHeader {
    /// Decode a 7-byte LPCM audio-pack header from the start of
    /// `payload` (which starts at the substream-ID byte — i.e. the
    /// first byte of a `private_stream_1` PES payload routed to an
    /// LPCM track per [`crate::vob::DvdSubstream::Lpcm`]).
    ///
    /// Rejects a buffer shorter than [`LPCM_HEADER_LEN`] bytes and a
    /// `sub_stream_id` outside the `0xA0..=0xA7` LPCM range with
    /// [`Error::InvalidUdf`] — both indicate the demuxer routed a
    /// non-LPCM PES payload to the LPCM header decoder by mistake.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() < LPCM_HEADER_LEN {
            return Err(Error::InvalidUdf(
                "LPCM audio-pack header truncated (< 7 bytes)",
            ));
        }
        let sub_stream_id = payload[0];
        if !(0xA0..=0xA7).contains(&sub_stream_id) {
            return Err(Error::InvalidUdf(
                "LPCM audio-pack header: sub_stream_id not in 0xA0..=0xA7",
            ));
        }
        let number_of_frame_headers = payload[1];
        let first_access_unit_pointer = u16::from_be_bytes([payload[2], payload[3]]);

        let byte4 = payload[4];
        let audio_emphasis_flag = (byte4 & 0b1000_0000) != 0;
        let audio_mute_flag = (byte4 & 0b0100_0000) != 0;
        // bit 5 reserved
        let audio_frame_number = byte4 & 0b0001_1111;

        let byte5 = payload[5];
        let quantisation = LpcmQuantisation::from_code(byte5 >> 6);
        let sample_frequency = LpcmSampleFrequency::from_code((byte5 >> 4) & 0b11);
        // bit 3 reserved
        let channel_count = (byte5 & 0b0000_0111) + 1;

        let byte6 = payload[6];
        let dynamic_range_x = byte6 >> 5;
        let dynamic_range_y = byte6 & 0b0001_1111;

        Ok(Self {
            sub_stream_id,
            number_of_frame_headers,
            first_access_unit_pointer,
            audio_emphasis_flag,
            audio_mute_flag,
            audio_frame_number,
            quantisation,
            sample_frequency,
            channel_count,
            dynamic_range_x,
            dynamic_range_y,
        })
    }

    /// LPCM track ID `0..=7` (substream ID minus the `0xA0` base).
    pub fn track(self) -> u8 {
        self.sub_stream_id - 0xA0
    }

    /// Bits per sample for the well-defined quantisation codes;
    /// `None` for [`LpcmQuantisation::Reserved`].
    pub fn bits_per_sample(self) -> Option<u8> {
        self.quantisation.bits_per_sample()
    }

    /// Sample rate in Hz for the well-defined frequency codes;
    /// `None` for [`LpcmSampleFrequency::Reserved`].
    pub fn sample_rate_hz(self) -> Option<u32> {
        self.sample_frequency.hz()
    }

    /// Uncompressed bitrate (kbps) for the well-defined combinations
    /// of channel count, sample rate, and quantisation. Returns
    /// `None` when either of the two reserved codes is present.
    ///
    /// For 48 kHz × 16-bit × 2 ch this returns `1536`; for the
    /// `stnsoft-LimPcmAud.html` ceiling cell (48 kHz × 16-bit × 8 ch)
    /// it returns `6144`.
    pub fn bitrate_kbps(self) -> Option<u32> {
        let bits = self.bits_per_sample()? as u32;
        let rate = self.sample_rate_hz()?;
        Some((bits * rate * self.channel_count as u32) / 1_000)
    }

    /// `true` ⇒ the declared format fits inside the DVD-Video
    /// 6144 kbps LPCM ceiling per `stnsoft-LimPcmAud.html`. Returns
    /// `false` for the red-marked combinations and for any header
    /// whose quantisation / sample-frequency code is reserved.
    pub fn is_within_dvd_video_limit(self) -> bool {
        self.bitrate_kbps()
            .is_some_and(|kbps| kbps <= DVD_LPCM_MAX_BITRATE_KBPS)
    }

    /// Linear dynamic-range gain `2^(4 - (X + Y / 30))` per the
    /// `mpucoder-lpcm.html` formula. `X = 0, Y = 0` gives the
    /// unity-gain identity `16.0` (i.e. `2^4`), matching the
    /// no-attenuation default; larger `X` / `Y` attenuate.
    pub fn linear_gain(self) -> f32 {
        let exponent = 4.0 - (self.dynamic_range_x as f32 + self.dynamic_range_y as f32 / 30.0);
        2.0_f32.powf(exponent)
    }

    /// Gain in dB per `24.082 - 6.0206 X - 0.2007 Y`. `X = 0, Y = 0`
    /// returns the unity-gain reference `24.082` dB. (The `linear
    /// gain` and `gain_db` formulas are two parameterisations of the
    /// same coefficient table on `mpucoder-lpcm.html`.)
    pub fn gain_db(self) -> f32 {
        24.082 - 6.0206 * self.dynamic_range_x as f32 - 0.2007 * self.dynamic_range_y as f32
    }

    /// Number of bytes one fully-interleaved sample frame (one sample
    /// per channel) occupies, for the well-defined quantisation codes.
    ///
    /// A 16-bit stereo stream packs `2 channels × 2 bytes = 4` bytes
    /// per frame; 24-bit 5.1 packs `6 × 3 = 18`. Returns `None` when
    /// the quantisation code is [`LpcmQuantisation::Reserved`] (no
    /// defined sample width). Per `mpucoder-lpcm.html`'s worked example
    /// (48 kHz 16-bit stereo = 320 bytes / frame across 80 sample
    /// frames).
    pub fn frame_stride_bytes(self) -> Option<usize> {
        let bits = self.quantisation.bits_per_sample()? as usize;
        Some((bits / 8) * self.channel_count as usize)
    }

    /// Unpack the raw 16-bit PCM tail into channel-interleaved signed
    /// samples.
    ///
    /// `pcm` is the byte slice that follows the 7-byte audio-pack
    /// header (i.e. the second half of [`peel_lpcm_payload`]'s return).
    /// Per `mpucoder-lpcm.html` the samples are stored most-significant-
    /// byte-first ("first channel 0 (left) sample" begins the payload),
    /// interleaved by channel in ascending channel order; this decoder
    /// reads each pair as a big-endian `i16` widened to `i32`, so a
    /// caller can mix channel counts uniformly.
    ///
    /// Returns `None` for any non-16-bit quantisation
    /// ([`LpcmQuantisation::Bits20`] / [`LpcmQuantisation::Bits24`] /
    /// [`LpcmQuantisation::Reserved`]) — the 20-bit and 24-bit sub-byte
    /// grouping layout is **not specified** by the staged reference
    /// pages (see the module-level docs-gap note), so this decoder
    /// covers only the bit-pure 16-bit case.
    ///
    /// Any trailing bytes that do not complete a 16-bit sample (an
    /// odd-length tail) are ignored — DVD LPCM packs whole samples, so
    /// a remainder indicates a truncated payload the caller can detect
    /// by comparing `pcm.len()` against `2 × returned.len()`.
    pub fn unpack_samples_16bit(self, pcm: &[u8]) -> Option<Vec<i32>> {
        if self.quantisation != LpcmQuantisation::Bits16 {
            return None;
        }
        let mut out = Vec::with_capacity(pcm.len() / 2);
        for chunk in pcm.chunks_exact(2) {
            out.push(i16::from_be_bytes([chunk[0], chunk[1]]) as i32);
        }
        Some(out)
    }

    /// Number of complete sample frames (one sample per channel) the
    /// 16-bit PCM tail `pcm` carries.
    ///
    /// Returns `None` for non-16-bit quantisation (same rationale as
    /// [`Self::unpack_samples_16bit`]) and for a zero-channel header
    /// (which the on-wire `channels - 1` encoding cannot produce, but
    /// guards against a corrupt count). For 48 kHz 16-bit stereo with a
    /// 320-byte tail this returns `80`, matching the
    /// `mpucoder-lpcm.html` worked example.
    pub fn sample_frame_count_16bit(self, pcm: &[u8]) -> Option<usize> {
        if self.quantisation != LpcmQuantisation::Bits16 {
            return None;
        }
        let stride = self.frame_stride_bytes()?;
        if stride == 0 {
            return None;
        }
        Some(pcm.len() / stride)
    }
}

/// Split a private-stream-1 LPCM PES payload into its 7-byte header
/// and the raw PCM sample tail. The input is expected to be the PES
/// payload *with the substream selector still present* (i.e. byte 0
/// is the `0xA0..=0xA7` substream ID — the same shape the VOB
/// demuxer routes to `VobStreams::lpcm` after stripping is undone).
///
/// The returned slice borrows from `payload` directly so callers can
/// forward the PCM bytes to a sample-unpacker without copying.
pub fn peel_lpcm_payload(payload: &[u8]) -> Result<(LpcmHeader, &[u8])> {
    let header = LpcmHeader::parse(payload)?;
    Ok((header, &payload[LPCM_HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the canonical 7-byte LPCM header: track 0, 16-bit /
    /// 48 kHz / 2 ch, no emphasis / mute, all counters zero, unity
    /// dynamic-range coefficients.
    fn baseline_header() -> [u8; 7] {
        [
            0xA0, // sub_stream_id, LPCM track 0
            0x00, // number_of_frame_headers
            0x00, 0x00, // first_access_unit_pointer
            0x00, // emphasis=0 mute=0 frame=0
            0x01, // q=0 (16-bit) sr=0 (48k) ch=0+1=1 — fixed below
            0x00, // X=0 Y=0
        ]
    }

    #[test]
    fn parse_baseline_header() {
        let mut bytes = baseline_header();
        // 2-channel form: ch_code = 1 → channel_count = 2
        bytes[5] = 0b0000_0001;
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h.sub_stream_id, 0xA0);
        assert_eq!(h.track(), 0);
        assert_eq!(h.number_of_frame_headers, 0);
        assert_eq!(h.first_access_unit_pointer, 0);
        assert!(!h.audio_emphasis_flag);
        assert!(!h.audio_mute_flag);
        assert_eq!(h.audio_frame_number, 0);
        assert_eq!(h.quantisation, LpcmQuantisation::Bits16);
        assert_eq!(h.sample_frequency, LpcmSampleFrequency::Hz48000);
        assert_eq!(h.channel_count, 2);
        assert_eq!(h.bits_per_sample(), Some(16));
        assert_eq!(h.sample_rate_hz(), Some(48_000));
        assert_eq!(h.bitrate_kbps(), Some(1_536));
        assert!(h.is_within_dvd_video_limit());
    }

    #[test]
    fn parse_rejects_short_buffer() {
        let short = [0xA0, 0, 0, 0, 0, 0];
        let err = LpcmHeader::parse(&short).unwrap_err();
        matches!(err, Error::InvalidUdf(_));
    }

    #[test]
    fn parse_rejects_non_lpcm_substream() {
        // 0x80 is AC-3 territory, not LPCM.
        let bytes = [0x80, 0, 0, 0, 0, 0, 0];
        let err = LpcmHeader::parse(&bytes).unwrap_err();
        matches!(err, Error::InvalidUdf(_));
    }

    #[test]
    fn parse_decodes_each_track_id() {
        for track in 0..=7u8 {
            let mut bytes = baseline_header();
            bytes[0] = 0xA0 + track;
            let h = LpcmHeader::parse(&bytes).unwrap();
            assert_eq!(h.track(), track);
            assert_eq!(h.sub_stream_id, 0xA0 + track);
        }
    }

    #[test]
    fn parse_decodes_quantisation_codes() {
        for (code, expected, bps) in [
            (0u8, LpcmQuantisation::Bits16, Some(16)),
            (1, LpcmQuantisation::Bits20, Some(20)),
            (2, LpcmQuantisation::Bits24, Some(24)),
            (3, LpcmQuantisation::Reserved, None),
        ] {
            let mut bytes = baseline_header();
            // Preserve channel-count = 2 (ch_code = 1).
            bytes[5] = (code << 6) | 0b0000_0001;
            let h = LpcmHeader::parse(&bytes).unwrap();
            assert_eq!(h.quantisation, expected);
            assert_eq!(h.bits_per_sample(), bps);
        }
    }

    #[test]
    fn parse_decodes_sample_frequency_codes() {
        for (code, expected, hz) in [
            (0u8, LpcmSampleFrequency::Hz48000, Some(48_000)),
            (1, LpcmSampleFrequency::Hz96000, Some(96_000)),
            (2, LpcmSampleFrequency::Reserved, None),
            (3, LpcmSampleFrequency::Reserved, None),
        ] {
            let mut bytes = baseline_header();
            bytes[5] = (code << 4) | 0b0000_0001;
            let h = LpcmHeader::parse(&bytes).unwrap();
            assert_eq!(h.sample_frequency, expected);
            assert_eq!(h.sample_rate_hz(), hz);
        }
    }

    #[test]
    fn parse_decodes_channel_count_offset_by_one() {
        for code in 0u8..=7 {
            let mut bytes = baseline_header();
            bytes[5] = code & 0b0000_0111;
            let h = LpcmHeader::parse(&bytes).unwrap();
            assert_eq!(h.channel_count, code + 1);
        }
    }

    #[test]
    fn parse_decodes_emphasis_mute_frame_number() {
        let mut bytes = baseline_header();
        bytes[4] = 0b1000_0000 | 0b0100_0000 | 0b0001_0101;
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert!(h.audio_emphasis_flag);
        assert!(h.audio_mute_flag);
        assert_eq!(h.audio_frame_number, 0b1_0101);
    }

    #[test]
    fn parse_decodes_first_access_unit_pointer() {
        let mut bytes = baseline_header();
        bytes[2] = 0x12;
        bytes[3] = 0x34;
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h.first_access_unit_pointer, 0x1234);
    }

    #[test]
    fn parse_decodes_dynamic_range_xy_split() {
        let mut bytes = baseline_header();
        bytes[6] = 0b1010_0000 | 0b0000_1011; // X = 0b101, Y = 0b01011
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h.dynamic_range_x, 0b101);
        assert_eq!(h.dynamic_range_y, 0b01011);
    }

    #[test]
    fn dynamic_range_unity_gain_at_zero_zero() {
        let bytes = baseline_header();
        let h = LpcmHeader::parse(&bytes).unwrap();
        // X = 0, Y = 0 → exponent = 4 → 2^4 = 16.
        assert!((h.linear_gain() - 16.0).abs() < 1e-4);
        // dB form: 24.082 - 0 - 0 = 24.082.
        assert!((h.gain_db() - 24.082).abs() < 1e-3);
    }

    #[test]
    fn dynamic_range_negative_attenuation_when_x_y_grow() {
        let mut bytes = baseline_header();
        bytes[6] = 0b1110_0000 | 0b0001_1110; // X = 7, Y = 30
        let h = LpcmHeader::parse(&bytes).unwrap();
        // exponent = 4 - (7 + 30/30) = 4 - 8 = -4 → linear_gain = 1/16.
        assert!((h.linear_gain() - (1.0 / 16.0)).abs() < 1e-5);
        // dB form: 24.082 - 42.1442 - 6.021 = -24.0832 (matches the
        // -24 dB attenuation pole on mpucoder-lpcm.html).
        assert!(h.gain_db() < -24.0 && h.gain_db() > -24.5);
    }

    /// Every well-defined cell on the green half of the
    /// `stnsoft-LimPcmAud.html` table must report
    /// `is_within_dvd_video_limit() == true`; every red cell must
    /// report `false`. The table is reproduced bit-for-bit here so a
    /// future spec patch surfaces as a test diff.
    #[test]
    fn bitrate_table_matches_limpcmaud_doc() {
        // (sample_rate_code, quantisation_code, channels, kbps, is_red)
        let table: &[(u8, u8, u8, u32, bool)] = &[
            // 48 kHz / 16 bits
            (0, 0, 1, 768, false),
            (0, 0, 2, 1536, false),
            (0, 0, 3, 2304, false),
            (0, 0, 4, 3072, false),
            (0, 0, 5, 3840, false),
            (0, 0, 6, 4608, false),
            (0, 0, 7, 5376, false),
            (0, 0, 8, 6144, false),
            // 48 kHz / 20 bits
            (0, 1, 1, 960, false),
            (0, 1, 2, 1920, false),
            (0, 1, 3, 2880, false),
            (0, 1, 4, 3840, false),
            (0, 1, 5, 4800, false),
            (0, 1, 6, 5760, false),
            (0, 1, 7, 6720, true),
            (0, 1, 8, 7680, true),
            // 48 kHz / 24 bits
            (0, 2, 1, 1152, false),
            (0, 2, 2, 2304, false),
            (0, 2, 3, 3456, false),
            (0, 2, 4, 4608, false),
            (0, 2, 5, 5760, false),
            (0, 2, 6, 6912, true),
            (0, 2, 7, 8064, true),
            (0, 2, 8, 9216, true),
            // 96 kHz / 16 bits
            (1, 0, 1, 1536, false),
            (1, 0, 2, 3072, false),
            (1, 0, 3, 4608, false),
            (1, 0, 4, 6144, false),
            (1, 0, 5, 7680, true),
            (1, 0, 6, 9216, true),
            (1, 0, 7, 10752, true),
            (1, 0, 8, 12288, true),
            // 96 kHz / 20 bits
            (1, 1, 1, 1920, false),
            (1, 1, 2, 3840, false),
            (1, 1, 3, 5760, false),
            (1, 1, 4, 7680, true),
            (1, 1, 5, 9600, true),
            (1, 1, 6, 11520, true),
            (1, 1, 7, 13440, true),
            (1, 1, 8, 15360, true),
            // 96 kHz / 24 bits
            (1, 2, 1, 2304, false),
            (1, 2, 2, 4608, false),
            (1, 2, 3, 6912, true),
            (1, 2, 4, 9216, true),
            (1, 2, 5, 11520, true),
            (1, 2, 6, 13824, true),
            (1, 2, 7, 16128, true),
            (1, 2, 8, 18432, true),
        ];
        for &(sr, q, ch, expected_kbps, is_red) in table {
            let mut bytes = baseline_header();
            bytes[5] = (q << 6) | (sr << 4) | ((ch - 1) & 0b111);
            let h = LpcmHeader::parse(&bytes).unwrap();
            assert_eq!(
                h.bitrate_kbps(),
                Some(expected_kbps),
                "sr={sr} q={q} ch={ch} mismatched bitrate",
            );
            assert_eq!(
                h.is_within_dvd_video_limit(),
                !is_red,
                "sr={sr} q={q} ch={ch} mismatched DVD-limit verdict",
            );
        }
    }

    #[test]
    fn bitrate_returns_none_for_reserved_codes() {
        // Reserved quantisation code 3.
        let mut bytes = baseline_header();
        bytes[5] = (3 << 6) | 0b0000_0001;
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h.bitrate_kbps(), None);
        assert!(!h.is_within_dvd_video_limit());

        // Reserved sample-frequency code 2.
        let mut bytes = baseline_header();
        bytes[5] = (2 << 4) | 0b0000_0001;
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h.bitrate_kbps(), None);
        assert!(!h.is_within_dvd_video_limit());
    }

    #[test]
    fn peel_lpcm_payload_returns_header_and_tail() {
        let mut bytes = vec![0xA0, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00];
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let (h, tail) = peel_lpcm_payload(&bytes).unwrap();
        assert_eq!(h.track(), 0);
        assert_eq!(tail, &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn peel_lpcm_payload_rejects_short_buffer() {
        let short = [0xA0, 0, 0, 0];
        let err = peel_lpcm_payload(&short).unwrap_err();
        matches!(err, Error::InvalidUdf(_));
    }

    /// Build a 16-bit stereo (or N-channel) header for the unpacker
    /// tests: track 0, 48 kHz, unity dynamic range.
    fn header_16bit(channels: u8) -> LpcmHeader {
        let mut bytes = baseline_header();
        // q=0 (16-bit), sr=0 (48k), channels = channels-1 in low 3 bits.
        bytes[5] = (channels - 1) & 0b111;
        LpcmHeader::parse(&bytes).unwrap()
    }

    #[test]
    fn unpack_16bit_reads_big_endian_interleaved() {
        let h = header_16bit(2);
        // Two stereo frames: L0=0x0102, R0=0xFFFE (=-2), L1=0x7FFF, R1=0x8000.
        let pcm = [0x01, 0x02, 0xFF, 0xFE, 0x7F, 0xFF, 0x80, 0x00];
        let samples = h.unpack_samples_16bit(&pcm).unwrap();
        assert_eq!(samples, vec![0x0102, -2, 0x7FFF, -0x8000]);
    }

    #[test]
    fn unpack_16bit_ignores_incomplete_trailing_byte() {
        let h = header_16bit(1);
        // 5 bytes → 2 whole i16 samples + 1 leftover byte (ignored).
        let pcm = [0x00, 0x10, 0x00, 0x20, 0xAA];
        let samples = h.unpack_samples_16bit(&pcm).unwrap();
        assert_eq!(samples, vec![0x0010, 0x0020]);
        // The caller can detect truncation: 2 samples × 2 bytes = 4 < 5.
        assert_ne!(pcm.len(), samples.len() * 2);
    }

    #[test]
    fn unpack_16bit_rejects_non_16bit_quantisation() {
        let mut bytes = baseline_header();
        // q=1 (20-bit), stereo.
        bytes[5] = 0b0100_0001;
        let h20 = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h20.quantisation, LpcmQuantisation::Bits20);
        assert_eq!(h20.unpack_samples_16bit(&[0u8; 8]), None);
        assert_eq!(h20.sample_frame_count_16bit(&[0u8; 8]), None);

        // q=2 (24-bit).
        bytes[5] = 0b1000_0001;
        let h24 = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h24.quantisation, LpcmQuantisation::Bits24);
        assert_eq!(h24.unpack_samples_16bit(&[0u8; 9]), None);
    }

    #[test]
    fn frame_stride_and_count_match_mpucoder_example() {
        // mpucoder-lpcm.html worked example: 48 kHz 16-bit stereo, one
        // 1.67 ms frame = 80 sample frames = 320 bytes.
        let h = header_16bit(2);
        assert_eq!(h.frame_stride_bytes(), Some(4)); // 2 ch × 2 bytes
        let pcm = vec![0u8; 320];
        assert_eq!(h.sample_frame_count_16bit(&pcm), Some(80));
        // The flat sample vector holds channel_count × frame_count
        // interleaved samples.
        let samples = h.unpack_samples_16bit(&pcm).unwrap();
        assert_eq!(samples.len(), 80 * 2);
    }

    #[test]
    fn frame_stride_scales_with_channels_and_width() {
        // 16-bit 5.1 (6 channels) → 12-byte stride.
        let h6 = header_16bit(6);
        assert_eq!(h6.frame_stride_bytes(), Some(12));

        // 24-bit 6-channel stride is defined even though the sample
        // unpacker isn't: 6 × 3 = 18 bytes per frame.
        let mut bytes = baseline_header();
        bytes[5] = 0b1000_0101; // q=2 (24-bit), channels = 5+1 = 6
        let h24 = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h24.channel_count, 6);
        assert_eq!(h24.frame_stride_bytes(), Some(18));
    }

    #[test]
    fn frame_stride_none_for_reserved_quantisation() {
        let mut bytes = baseline_header();
        bytes[5] = 0b1100_0001; // q=3 (reserved)
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h.quantisation, LpcmQuantisation::Reserved);
        assert_eq!(h.frame_stride_bytes(), None);
    }
}
