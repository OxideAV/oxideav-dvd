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
//! Above the individual sample sits the **LPCM audio frame** — the
//! unit `number_of_frame_headers` / `audio_frame_number` /
//! `first_access_unit_pointer` all count in. Its geometry *is*
//! documented (`stnsoft-ass-hdr.html` "Additional info"): every frame
//! spans 150 ticks of the 90 kHz clock (1.67 ms, 600 frames/s), its
//! byte size follows `sample_rate × quantization × channels / 4800`,
//! and the `FrmNum` counter runs modulo 20 (so 20 frames form one
//! Group of Audio frames). [`LpcmHeader::audio_frame_size_bytes`] /
//! [`LpcmHeader::samples_per_frame`] / [`LpcmHeader::split_frames`]
//! implement that bytes → PCM-frames packing for **all three**
//! quantisation widths — a 20-bit sample has no whole-byte stride,
//! but a 20-bit *frame* always does (e.g. 48 kHz 20-bit stereo =
//! 400 bytes/frame).
//!
//! ## Docs gap
//!
//! Neither `mpucoder-lpcm.html` nor `stnsoft-LimPcmAud.html`
//! specifies the on-wire packing of 20-bit or 24-bit LPCM samples
//! (the order in which the grouped low-order bits are stored across
//! bytes). 16-bit packing is unambiguous (big-endian `i16`,
//! channel-interleaved) and fully covered here; 20/24-bit sample
//! reconstruction is blocked on a reference that pins the grouping.
//! Frame-granular splitting ([`LpcmHeader::split_frames`]) is *not*
//! affected — the frame byte-size formula is documented for every
//! width — only the intra-frame sample decode remains gated.
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

    /// Bytes occupied by one sample, as a reduced `(numerator,
    /// denominator)` ratio. `None` for [`Self::Reserved`].
    ///
    /// 16-bit packs two whole bytes per sample (`2/1`), 24-bit three
    /// (`3/1`), but 20-bit packs *two-and-a-half* bytes (`5/2`) — the
    /// fractional byte is why a 20-bit sample has no integer per-sample
    /// width and must be expressed as a ratio. This is a width fact the
    /// `mpucoder-lpcm.html` / `stnsoft-LimPcmAud.html` tables state
    /// directly (16 / 20 / 24 bits per sample); the *byte order* in
    /// which the grouped low-order bits of a 20/24-bit sample are stored
    /// across bytes is a separate, undocumented question (see the
    /// module-level docs-gap note) and is **not** implied by this ratio.
    pub fn bytes_per_sample(self) -> Option<(u32, u32)> {
        // bits / 8, reduced. 16→2/1, 20→5/2, 24→3/1.
        let bits = self.bits_per_sample()? as u32;
        let g = gcd(bits, 8);
        Some((bits / g, 8 / g))
    }
}

/// Greatest common divisor (binary-free Euclid) for the byte-ratio
/// reduction in [`LpcmQuantisation::bytes_per_sample`].
const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
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

/// Duration of one LPCM audio frame in 90 kHz clock ticks — the
/// `stnsoft-ass-hdr.html` "Additional info" constant ("LPCM frames
/// are 150 ticks of the 90KHz clock long (1.67ms)").
pub const LPCM_FRAME_DURATION_90KHZ: u32 = 150;

/// LPCM audio frames per second — `90_000 / 150 = 600` ("giving a
/// frame rate of 600 fps" per the same page). Rate-independent: at
/// 96 kHz each frame simply carries twice the samples.
pub const LPCM_FRAMES_PER_SECOND: u32 = 600;

/// Audio frames per Group of Audio frames. The header's `FrmNum`
/// field is documented as the "modulo 20 frame number of first
/// frame" (`stnsoft-ass-hdr.html`), so 20 consecutive frames form
/// one group.
pub const LPCM_FRAMES_PER_GROUP: u8 = 20;

/// Duration of one full Group of Audio frames in 90 kHz ticks —
/// `20 × 150 = 3000` (33.3 ms).
pub const LPCM_GROUP_DURATION_90KHZ: u32 = LPCM_FRAME_DURATION_90KHZ * LPCM_FRAMES_PER_GROUP as u32;

/// Borrowed iterator over whole LPCM audio frames — see
/// [`LpcmHeader::split_frames`]. Yields fixed-size byte slices, one
/// per complete audio frame; any incomplete trailing bytes are left
/// in [`LpcmFrames::partial_tail`].
#[derive(Debug, Clone)]
pub struct LpcmFrames<'a> {
    inner: std::slice::ChunksExact<'a, u8>,
}

impl<'a> LpcmFrames<'a> {
    /// The bytes after the last complete frame (empty when the
    /// payload divides evenly). A non-empty tail means the PES
    /// packet was truncated mid-frame or the caller sliced the
    /// stream off a frame boundary.
    pub fn partial_tail(&self) -> &'a [u8] {
        self.inner.remainder()
    }
}

impl<'a> Iterator for LpcmFrames<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        self.inner.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for LpcmFrames<'_> {}

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

    /// Bytes occupied by one sample, as a reduced `(numerator,
    /// denominator)` ratio — forwards
    /// [`LpcmQuantisation::bytes_per_sample`]. 16-bit is `2/1`, 24-bit
    /// `3/1`, 20-bit `5/2` (two-and-a-half bytes). `None` for the
    /// reserved quantisation code.
    pub fn bytes_per_sample(self) -> Option<(u32, u32)> {
        self.quantisation.bytes_per_sample()
    }

    /// Number of bytes one fully-interleaved sample frame (one sample
    /// per channel) occupies — `Some(n)` only when that count is a whole
    /// number of bytes.
    ///
    /// A 16-bit stereo frame packs `2 channels × 2 bytes = 4` bytes;
    /// 24-bit 5.1 packs `6 × 3 = 18`. The **20-bit** width is `2.5`
    /// bytes per sample, so a single sample never lands on a byte
    /// boundary; there is no integer per-sample byte stride and this
    /// accessor returns `None` for every 20-bit header regardless of
    /// channel count (use [`Self::bytes_per_sample`] for the exact
    /// `5/2` ratio). `None` is likewise returned for the reserved
    /// quantisation code. Per `mpucoder-lpcm.html`'s worked example,
    /// 48 kHz 16-bit stereo = 4 bytes / sample frame (320 bytes across
    /// 80 frames).
    ///
    /// The 20/24-bit *intra-group bit-packing* — the order in which a
    /// 20- or 24-bit sample's grouped low-order bits are laid across the
    /// payload bytes — is **not** documented by the staged reference
    /// pages (see the module-level docs-gap note); this accessor reports
    /// only the byte-aligned width fact those pages do state.
    pub fn frame_stride_bytes(self) -> Option<usize> {
        let (num, den) = self.bytes_per_sample()?;
        // A whole-byte per-sample width requires den == 1 (16- and
        // 24-bit). 20-bit (den == 2) has no integer per-sample stride.
        if den != 1 {
            return None;
        }
        Some(num as usize * self.channel_count as usize)
    }

    /// Samples one audio frame carries **per channel** — `sample_rate
    /// / 600`. 48 kHz packs 80 samples per frame ("At 48K sample rate
    /// there are 48000/600 = 80 samples per frame" per
    /// `stnsoft-ass-hdr.html`), 96 kHz packs 160. `None` for the
    /// reserved sample-frequency code.
    pub fn samples_per_frame(self) -> Option<u32> {
        Some(self.sample_rate_hz()? / LPCM_FRAMES_PER_SECOND)
    }

    /// Byte size of one LPCM audio frame — the documented
    /// `(sample rate) × (quantization) × (number of channels) / 4800`
    /// formula (`stnsoft-ass-hdr.html` "Additional info").
    ///
    /// Unlike the per-sample stride, this is a whole number of bytes
    /// for **every** quantisation width: the worked example's 48 kHz
    /// 16-bit stereo gives 320 bytes; 48 kHz **20-bit** stereo gives
    /// 400 (the fractional 2.5-byte samples always group evenly
    /// across a frame's 80 × channels samples). `None` when either
    /// the quantisation or sample-frequency code is reserved.
    pub fn audio_frame_size_bytes(self) -> Option<usize> {
        let rate = self.sample_rate_hz()?;
        let bits = self.bits_per_sample()? as u32;
        Some((rate * bits * self.channel_count as u32 / 4800) as usize)
    }

    /// Split the raw PCM tail (the bytes after the 7-byte audio-pack
    /// header — [`peel_lpcm_payload`]'s second return) into whole
    /// LPCM audio frames of [`Self::audio_frame_size_bytes`] bytes
    /// each.
    ///
    /// This is the bytes → PCM-frames packing boundary the header's
    /// counters speak in: `number_of_frame_headers` counts frames
    /// whose first byte lands in this pack, `audio_frame_number` is
    /// the modulo-[`LPCM_FRAMES_PER_GROUP`] index of the first one,
    /// and each yielded frame advances the clock by
    /// [`LPCM_FRAME_DURATION_90KHZ`] ticks. Works for 16-, 20- **and**
    /// 24-bit quantisation (the frame byte size is documented for all
    /// three; only intra-frame 20/24-bit sample decode is gated on the
    /// module-level docs gap). `None` when a reserved quantisation /
    /// sample-frequency code leaves the frame size undefined.
    pub fn split_frames(self, pcm: &[u8]) -> Option<LpcmFrames<'_>> {
        let size = self.audio_frame_size_bytes()?;
        if size == 0 {
            return None;
        }
        Some(LpcmFrames {
            inner: pcm.chunks_exact(size),
        })
    }

    /// Number of complete audio frames the PCM tail carries —
    /// `pcm.len() / audio_frame_size_bytes`. `None` under the same
    /// reserved-code conditions as [`Self::split_frames`].
    pub fn audio_frame_count(self, pcm: &[u8]) -> Option<usize> {
        let size = self.audio_frame_size_bytes()?;
        if size == 0 {
            return None;
        }
        Some(pcm.len() / size)
    }

    /// `true` when `first_access_unit_pointer == 0` — the reference
    /// page's "the value 0000 indicates there is no first access
    /// unit": no audio frame *starts* in this pack, so the PES PTS
    /// (if any) belongs to a frame carried over from an earlier pack.
    pub fn has_no_first_access_unit(self) -> bool {
        self.first_access_unit_pointer == 0
    }

    /// Payload-relative byte offset of the audio frame the enclosing
    /// PES PTS applies to, using the reference page's arithmetic:
    /// "offset 0 is the last byte of FirstAccUnit, ie add the offset
    /// of byte 2 to get the AU's offset" — the pointer's last byte
    /// sits at payload offset 3, so the frame starts at
    /// `3 + first_access_unit_pointer`.
    ///
    /// For the `stnsoft-ass-hdr.html` LPCM worked example
    /// (`FirstAccUnit = 0x0004`) this yields 7 — exactly the first
    /// byte after the 7-byte audio-pack header, where "first channel
    /// 0 (left) sample" begins. Returns `None` when
    /// [`Self::has_no_first_access_unit`] holds.
    pub fn access_unit_offset(self) -> Option<usize> {
        if self.has_no_first_access_unit() {
            None
        } else {
            Some(3 + self.first_access_unit_pointer as usize)
        }
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

    #[test]
    fn bytes_per_sample_ratio_per_quantisation() {
        // 16-bit → 2/1, 20-bit → 5/2 (two-and-a-half bytes), 24-bit → 3/1.
        assert_eq!(LpcmQuantisation::Bits16.bytes_per_sample(), Some((2, 1)));
        assert_eq!(LpcmQuantisation::Bits20.bytes_per_sample(), Some((5, 2)));
        assert_eq!(LpcmQuantisation::Bits24.bytes_per_sample(), Some((3, 1)));
        assert_eq!(LpcmQuantisation::Reserved.bytes_per_sample(), None);

        // The header accessor forwards the same ratio.
        let mut bytes = baseline_header();
        bytes[5] = 0b0100_0001; // q=1 (20-bit), stereo
        let h20 = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h20.bytes_per_sample(), Some((5, 2)));
    }

    #[test]
    fn frame_stride_none_for_20bit_no_integer_per_sample_width() {
        // 20-bit packs 2.5 bytes/sample — no integer per-sample stride —
        // so frame_stride_bytes() is None for every channel count, while
        // bytes_per_sample() still reports the exact 5/2 ratio.
        for ch in 1u8..=8 {
            let mut bytes = baseline_header();
            bytes[5] = 0b0100_0000 | ((ch - 1) & 0b111); // q=1 (20-bit)
            let h = LpcmHeader::parse(&bytes).unwrap();
            assert_eq!(h.quantisation, LpcmQuantisation::Bits20);
            assert_eq!(h.channel_count, ch);
            assert_eq!(
                h.frame_stride_bytes(),
                None,
                "20-bit {ch}ch must have no integer per-frame stride",
            );
            assert_eq!(h.bytes_per_sample(), Some((5, 2)));
        }
    }

    /// Build a header with explicit quantisation / rate / channel
    /// codes for the frame-geometry tests.
    fn header_qrc(q: u8, sr: u8, channels: u8) -> LpcmHeader {
        let mut bytes = baseline_header();
        bytes[5] = (q << 6) | (sr << 4) | ((channels - 1) & 0b111);
        LpcmHeader::parse(&bytes).unwrap()
    }

    #[test]
    fn frame_timing_constants_match_ass_hdr_page() {
        // 150 ticks of the 90 kHz clock = 1.67 ms → 600 fps.
        assert_eq!(LPCM_FRAME_DURATION_90KHZ, 150);
        assert_eq!(LPCM_FRAMES_PER_SECOND, 600);
        assert_eq!(90_000 / LPCM_FRAME_DURATION_90KHZ, LPCM_FRAMES_PER_SECOND);
        // FrmNum is modulo 20 → one group = 20 × 150 = 3000 ticks.
        assert_eq!(LPCM_FRAMES_PER_GROUP, 20);
        assert_eq!(LPCM_GROUP_DURATION_90KHZ, 3000);
        // The 5-bit audio_frame_number field holds every modulo-20
        // value the page allows: the highest one still parses intact.
        let mut bytes = baseline_header();
        bytes[4] = LPCM_FRAMES_PER_GROUP - 1; // frame 19 in low 5 bits
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert_eq!(h.audio_frame_number, 19);
    }

    #[test]
    fn samples_per_frame_follows_rate() {
        // "At 48K sample rate there are 48000/600 = 80 samples per frame."
        assert_eq!(header_qrc(0, 0, 2).samples_per_frame(), Some(80));
        assert_eq!(header_qrc(0, 1, 2).samples_per_frame(), Some(160));
        // Reserved rate code → undefined.
        assert_eq!(header_qrc(0, 2, 2).samples_per_frame(), None);
    }

    #[test]
    fn audio_frame_size_matches_formula_across_matrix() {
        // Frame size = rate × quantization × channels / 4800
        // (stnsoft-ass-hdr.html). Spot-check the worked example, then
        // sweep the full defined matrix against the formula.
        assert_eq!(header_qrc(0, 0, 2).audio_frame_size_bytes(), Some(320));
        for (q, bits) in [(0u8, 16u32), (1, 20), (2, 24)] {
            for (sr, rate) in [(0u8, 48_000u32), (1, 96_000)] {
                for ch in 1u8..=8 {
                    let h = header_qrc(q, sr, ch);
                    let expected = (rate * bits * ch as u32 / 4800) as usize;
                    assert_eq!(
                        h.audio_frame_size_bytes(),
                        Some(expected),
                        "q={q} sr={sr} ch={ch}",
                    );
                    // The formula must agree with samples-per-frame ×
                    // bits: frame bytes × 8 = samples × bits × channels.
                    let spf = h.samples_per_frame().unwrap();
                    assert_eq!(expected as u32 * 8, spf * bits * ch as u32);
                }
            }
        }
        // Reserved quantisation / rate codes leave the size undefined.
        assert_eq!(header_qrc(3, 0, 2).audio_frame_size_bytes(), None);
        assert_eq!(header_qrc(0, 2, 2).audio_frame_size_bytes(), None);
    }

    #[test]
    fn twenty_bit_frames_are_whole_bytes_despite_fractional_samples() {
        // 20-bit has no integer per-sample stride (5/2 bytes)…
        let h = header_qrc(1, 0, 2);
        assert_eq!(h.frame_stride_bytes(), None);
        // …but the frame is a whole 400 bytes (48000 × 20 × 2 / 4800),
        // so bytes → PCM frames still splits exactly.
        assert_eq!(h.audio_frame_size_bytes(), Some(400));
        let pcm = vec![0u8; 400 * 3];
        let frames = h.split_frames(&pcm).unwrap();
        assert_eq!(frames.len(), 3);
        for f in frames.clone() {
            assert_eq!(f.len(), 400);
        }
        assert!(frames.clone().partial_tail().is_empty());
        assert_eq!(h.audio_frame_count(&pcm), Some(3));
    }

    #[test]
    fn split_frames_yields_whole_frames_and_partial_tail() {
        // 16-bit mono 48 kHz → 160-byte frames.
        let h = header_qrc(0, 0, 1);
        assert_eq!(h.audio_frame_size_bytes(), Some(160));
        // Two whole frames + 5 stray bytes.
        let mut pcm = Vec::new();
        for i in 0..(160 * 2 + 5) {
            pcm.push(i as u8);
        }
        let mut frames = h.split_frames(&pcm).unwrap();
        let f0 = frames.next().unwrap();
        let f1 = frames.next().unwrap();
        assert!(frames.next().is_none());
        assert_eq!(f0, &pcm[..160]);
        assert_eq!(f1, &pcm[160..320]);
        assert_eq!(frames.partial_tail(), &pcm[320..]);
        assert_eq!(h.audio_frame_count(&pcm), Some(2));
    }

    #[test]
    fn split_frames_none_for_reserved_codes() {
        assert!(header_qrc(3, 0, 2).split_frames(&[0u8; 64]).is_none());
        assert!(header_qrc(0, 3, 2).split_frames(&[0u8; 64]).is_none());
        assert_eq!(header_qrc(3, 0, 2).audio_frame_count(&[0u8; 64]), None);
    }

    #[test]
    fn access_unit_offset_matches_ass_hdr_example() {
        // stnsoft-ass-hdr.html LPCM example: FirstAccUnit = 0x0004 →
        // the PTS frame begins at packet offset 026, i.e. 7 bytes
        // after the substream selector — the first byte after the
        // 7-byte audio-pack header.
        let mut bytes = baseline_header();
        bytes[1] = 0x07; // 7 frames begin in this packet
        bytes[2] = 0x00;
        bytes[3] = 0x04;
        let h = LpcmHeader::parse(&bytes).unwrap();
        assert!(!h.has_no_first_access_unit());
        assert_eq!(h.access_unit_offset(), Some(7));
        assert_eq!(h.access_unit_offset(), Some(LPCM_HEADER_LEN));
    }

    #[test]
    fn access_unit_offset_none_when_pointer_zero() {
        // "The value 0000 indicates there is no first access unit."
        let h = LpcmHeader::parse(&baseline_header()).unwrap();
        assert!(h.has_no_first_access_unit());
        assert_eq!(h.access_unit_offset(), None);
    }

    #[test]
    fn frame_stride_integer_for_16_and_24bit() {
        // 16-bit and 24-bit both land on whole-byte sample widths, so the
        // per-frame stride stays defined for all channel counts.
        for ch in 1u8..=8 {
            let h16 = header_16bit(ch);
            assert_eq!(h16.frame_stride_bytes(), Some(2 * ch as usize));

            let mut bytes = baseline_header();
            bytes[5] = 0b1000_0000 | ((ch - 1) & 0b111); // q=2 (24-bit)
            let h24 = LpcmHeader::parse(&bytes).unwrap();
            assert_eq!(h24.frame_stride_bytes(), Some(3 * ch as usize));
        }
    }
}
