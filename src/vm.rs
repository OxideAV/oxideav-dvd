//! DVD-Video VM **interpreter** — Phase 3c.
//!
//! [`Vm`] owns the register file (16 GPRMs + 24 SPRMs, the per-GPRM
//! counter-mode bit) and the navigation-resume stack, and exposes
//! [`Vm::step`] which advances one [`NavInstruction`] and returns a
//! [`VmAction`] describing the playback-engine-visible effect. The
//! companion [`Vm::run_list`] walks a pre / post / cell command list
//! end-to-end and honours the intra-list `Goto` / `Break` control
//! flow.
//!
//! The interpreter is **transport-agnostic** — it never reads from
//! disc, never decodes a VOBU, never touches MKV. It mutates its own
//! register state and returns the playback-engine-visible
//! `JumpSS` / `CallSS` / `Link*` / `JumpTT` / `Exit` / `RSM`
//! destinations as typed [`VmAction`] values so a downstream player
//! can decide what to load next.
//!
//! Clean-room per:
//!
//! - `docs/container/dvd/application/mpucoder-vmi.html` — opcode
//!   semantics + SET/CMP sub-op tables + link-subset table.
//! - `docs/container/dvd/application/mpucoder-vmi-sum.html` — the
//!   plain-English instruction-family summary (compound Type 4..6
//!   forms' "Set then Compare & Link" ordering rules).
//! - `docs/container/dvd/application/mpucoder-vmi-jmp.html` — the
//!   per-domain Jump / Call permission tables (used here only as a
//!   sanity-check for the destination kinds the interpreter
//!   surfaces; domain enforcement is the player's job).
//! - `docs/container/dvd/application/mpucoder-sprm.html` — the SPRM
//!   numbering + default values.
//! - `docs/container/dvd/application/mpucoder-uops.html` — User
//!   Operation flag bit numbers.
//!
//! Semantics derive from the `docs/container/dvd/` references
//! listed above.

use crate::ifo::NavCommand;
use crate::nav::{
    CallSSTarget, CmpOp, JumpSSTarget, LinkSubset, NavInstruction, Operand, Register, SetOp,
};

// =====================================================================
// Register file.
// =====================================================================

/// Number of general-purpose registers (`GPRM0..=GPRM15`) per
/// `mpucoder-sprm.html`. The 16-register file is *persistent* — it
/// survives across PGCs and across `JumpSS` / `CallSS` boundaries
/// per the spec page's "GPRM" description.
pub const GPRM_COUNT: usize = 16;

/// Number of system-parameter registers exposed at runtime
/// (`SPRM0..=SPRM23`). The remaining `SPRM24..=SPRM31` slots are
/// reserved on the spec page — they have no defined behaviour so we
/// don't allocate them.
pub const SPRM_COUNT: usize = 24;

/// SPRM 0 — Preferred Menu Language (ISO 639 code, "player specific").
pub const SPRM_MENU_LANG: u8 = 0;
/// SPRM 1 — Audio Stream Number (`ASTN`).
pub const SPRM_AUDIO_STREAM: u8 = 1;
/// SPRM 2 — Sub-picture Stream Number (`SPSTN`).
pub const SPRM_SUBPICTURE_STREAM: u8 = 2;
/// SPRM 3 — Angle Number (`AGLN`).
pub const SPRM_ANGLE: u8 = 3;
/// SPRM 4 — Title Number in volume (`TTN`).
pub const SPRM_TITLE: u8 = 4;
/// SPRM 5 — Title Number in VTS (`VTS_TTN`).
pub const SPRM_VTS_TITLE: u8 = 5;
/// SPRM 6 — PGC Number (`TT_PGCN`).
pub const SPRM_PGCN: u8 = 6;
/// SPRM 7 — PTT Number (`PTTN`).
pub const SPRM_PTT: u8 = 7;
/// SPRM 8 — Highlighted Button Number (`HL_BTNN`).
pub const SPRM_HL_BTNN: u8 = 8;
/// SPRM 9 — Navigation Timer (`NVTMR`) in seconds (0..=65535).
pub const SPRM_NV_TIMER: u8 = 9;
/// SPRM 10 — PGC jump target when the nav timer expires (`NV_PGCN`).
pub const SPRM_NV_PGCN: u8 = 10;
/// SPRM 11 — Karaoke Audio Mixing Mode (`AMXMD`).
pub const SPRM_AMXMD: u8 = 11;
/// SPRM 12 — Parental-management Country Code (`CC_PLT`, ISO 3166).
pub const SPRM_CC_PLT: u8 = 12;
/// SPRM 13 — Parental Level (`PLT`).
pub const SPRM_PARENTAL_LEVEL: u8 = 13;
/// SPRM 14 — Video Preference + current display mode bitfield.
pub const SPRM_VIDEO_PREF: u8 = 14;
/// SPRM 15 — Player audio capability bitmap.
pub const SPRM_AUDIO_CAPS: u8 = 15;
/// SPRM 16 — Preferred audio language (ISO 639 code, default `0xFFFF`).
pub const SPRM_PREF_AUDIO_LANG: u8 = 16;
/// SPRM 17 — Preferred audio language extension code.
pub const SPRM_PREF_AUDIO_LANG_EXT: u8 = 17;
/// SPRM 18 — Preferred sub-picture language (ISO 639 code, default `0xFFFF`).
pub const SPRM_PREF_SUBP_LANG: u8 = 18;
/// SPRM 19 — Preferred sub-picture language extension code.
pub const SPRM_PREF_SUBP_LANG_EXT: u8 = 19;
/// SPRM 20 — Player Region-code mask (bit `i` ⇒ region `i + 1`).
pub const SPRM_REGION_MASK: u8 = 20;

/// The full SPRM-indexed default vector per `mpucoder-sprm.html`.
///
/// "Player specific" cells (SPRM 0 menu-language, SPRM 6 TT_PGCN, SPRM 10
/// NV_PGCN, SPRM 12 country code, SPRM 13 parental level, SPRM 14 video
/// preference, SPRM 15 audio caps, SPRM 20 region mask) are left at `0`
/// — the spec page assigns those to the host player's environment, not to
/// a fixed numeric default. The numeric defaults follow the spec page's
/// `default` column:
///
/// | SPRM | default            | source             |
/// | ---- | ------------------ | ------------------ |
/// |    1 | `15` (none)        | ASTN row           |
/// |    2 | `62` (none)        | SPSTN row          |
/// |    3 | `1`                | AGLN row           |
/// |    4 | `1`                | TTN row            |
/// |    5 | `1`                | VTS_TTN row        |
/// |    7 | `1`                | PTTN row           |
/// |    8 | `1024` (`1<<10`)   | HL_BTNN row, "button 1" in bits 10..15 |
/// |    9 | `0`                | NVTMR row          |
/// |   11 | `0`                | AMXMD row          |
/// |   16 | `0xFFFF` (none)    | preferred audio language row |
/// |   17 | `0` (not spec'd)   | language extension row |
/// |   18 | `0xFFFF` (none)    | preferred sub-picture language row |
/// |   19 | `0` (not spec'd)   | language extension row |
///
/// SPRMs 17 and 19 share the same documented enumeration: `0 = "not
/// specified"`, then a small fixed code set per the spec's `language
/// extension` row. The default `0` therefore matches "no preference
/// expressed", consistent with the umbrella `0xFFFF` on the corresponding
/// language slot. SPRMs 21..=23 are "reserved" on the spec page and we
/// leave them at `0`.
const SPRM_DEFAULTS: [u16; SPRM_COUNT] = {
    let mut v = [0u16; SPRM_COUNT];
    v[1] = 15; // ASTN
    v[2] = 62; // SPSTN
    v[3] = 1; // AGLN
    v[4] = 1; // TTN
    v[5] = 1; // VTS_TTN
    v[7] = 1; // PTTN
    v[8] = 1 << 10; // HL_BTNN — button 1 in bits 10..15
    v[16] = 0xFFFF; // preferred audio language
    v[17] = 0; // preferred audio language extension — "not specified"
    v[18] = 0xFFFF; // preferred sub-picture language
    v[19] = 0; // preferred sub-picture language extension — "not specified"
    v
};

/// 16 × GPRM + 24 × SPRM register file plus the per-GPRM
/// "counter mode" bit that `SetGPRMMD` toggles.
///
/// Per `mpucoder-vmi.html` the `SetGPRMMD` `mf` flag selects whether
/// the GPRM behaves as a plain integer register or as a 1 Hz
/// counter; the spec page reserves that behavioural state but
/// doesn't give it its own register address — so we carry it on the
/// side. The interpreter never *ticks* the counters itself (it has
/// no notion of wall time); the public [`RegisterFile::tick_counters`]
/// helper exists for a playback engine that owns a wall clock.
#[derive(Debug, Clone)]
pub struct RegisterFile {
    gprm: [u16; GPRM_COUNT],
    sprm: [u16; SPRM_COUNT],
    /// Bit `i` set ⇒ `GPRM[i]` is in counter mode (auto-increments
    /// once per second when ticked).
    counter_mask: u16,
}

impl Default for RegisterFile {
    fn default() -> Self {
        Self {
            gprm: [0; GPRM_COUNT],
            sprm: SPRM_DEFAULTS,
            counter_mask: 0,
        }
    }
}

impl RegisterFile {
    /// Construct a fresh register file with the spec-defined SPRM
    /// defaults and all GPRMs cleared.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a GPRM by index (`0..=15`). Out-of-range index returns
    /// `0` — matches the spec's "invalid register reads as 0"
    /// fallback used by malformed PGC command tables in the wild.
    pub fn gprm(&self, index: u8) -> u16 {
        if (index as usize) < GPRM_COUNT {
            self.gprm[index as usize]
        } else {
            0
        }
    }

    /// Write a GPRM by index. Out-of-range index is silently dropped.
    pub fn set_gprm(&mut self, index: u8, value: u16) {
        if (index as usize) < GPRM_COUNT {
            self.gprm[index as usize] = value;
        }
    }

    /// Read an SPRM by index (`0..=23`). Out-of-range returns `0`.
    pub fn sprm(&self, index: u8) -> u16 {
        if (index as usize) < SPRM_COUNT {
            self.sprm[index as usize]
        } else {
            0
        }
    }

    /// Write an SPRM by index. Out-of-range index is silently dropped.
    ///
    /// SPRMs are largely read-only at the bit-stream level (the
    /// `SetSystem` opcodes are the only legal entry points), but
    /// nothing in the spec page forbids a runtime / debugger from
    /// pre-loading the file; we surface the write so tests + tooling
    /// can.
    pub fn set_sprm(&mut self, index: u8, value: u16) {
        if (index as usize) < SPRM_COUNT {
            self.sprm[index as usize] = value;
        }
    }

    /// Read whichever register the [`Register`] enum names. SPRMs
    /// out of the supported range and the catch-all `Invalid`
    /// variant both return `0`.
    pub fn read(&self, reg: Register) -> u16 {
        match reg {
            Register::Gprm(i) => self.gprm(i),
            Register::Sprm(i) => self.sprm(i),
            Register::Invalid(_) => 0,
        }
    }

    /// Resolve an [`Operand`] to its 16-bit value.
    pub fn read_operand(&self, op: Operand) -> u16 {
        match op {
            Operand::Register(r) => self.read(r),
            Operand::Immediate(v) => v,
        }
    }

    /// Flip the per-GPRM counter-mode flag.
    pub fn set_counter_mode(&mut self, gprm_index: u8, on: bool) {
        if (gprm_index as usize) < GPRM_COUNT {
            let bit = 1u16 << gprm_index;
            if on {
                self.counter_mask |= bit;
            } else {
                self.counter_mask &= !bit;
            }
        }
    }

    /// `true` ⇒ the named GPRM is acting as a 1 Hz counter.
    pub fn counter_mode(&self, gprm_index: u8) -> bool {
        if (gprm_index as usize) < GPRM_COUNT {
            (self.counter_mask >> gprm_index) & 1 == 1
        } else {
            false
        }
    }

    /// Advance every counter-mode GPRM by `delta` seconds (saturating
    /// at `u16::MAX`). The interpreter never invokes this — it's a
    /// hook for a playback engine that owns a wall clock.
    pub fn tick_counters(&mut self, delta: u16) {
        let mut mask = self.counter_mask;
        while mask != 0 {
            let bit = mask.trailing_zeros() as usize;
            self.gprm[bit] = self.gprm[bit].saturating_add(delta);
            mask &= !(1u16 << bit);
        }
    }

    // -----------------------------------------------------------------
    // Bitfield-aware accessors for the 6 SPRMs whose contents are not a
    // single integer but a documented bit-packed payload. Each accessor
    // decomposes the register against the layout published on
    // `docs/container/dvd/application/mpucoder-sprm.html`.
    //
    // The accessors **read** from the raw `u16` slot — they don't
    // shadow the storage. A `SetSystem` opcode that writes the packed
    // register still goes through `set_sprm`; these helpers exist so a
    // player engine can ask "which subpicture stream is active and is
    // it currently being displayed?" without re-implementing the
    // bit-packing on every callsite.
    // -----------------------------------------------------------------

    /// SPRM 2 sub-picture stream — bits 0..=5 carry the stream
    /// number (`0..=31`, `62` = none, `63` = forced); bit 6 = `0`
    /// means "do not display". Returns the pair as a typed view.
    pub fn subpicture_stream(&self) -> SubpictureStreamView {
        let raw = self.sprm(SPRM_SUBPICTURE_STREAM);
        SubpictureStreamView {
            stream: (raw & 0x3F) as u8,
            display: (raw >> 6) & 1 == 1,
            raw,
        }
    }

    /// SPRM 8 highlighted button — bits 10..=15 carry the button
    /// number (`1..=36`); bits 0..=9 are documented as `0`.
    /// Returns the decoded button number, or `0` when the field is
    /// outside `1..=36` (which a malformed disc could encode).
    pub fn highlight_button(&self) -> u8 {
        let raw = self.sprm(SPRM_HL_BTNN);
        let n = (raw >> 10) as u8;
        if (1..=36).contains(&n) {
            n
        } else {
            0
        }
    }

    /// SPRM 11 karaoke audio mixing mode — returns the channel-routing
    /// matrix. Bits 2/3/4 enable mixing channel 2/3/4 into the front
    /// (front-left) channel; bits 10/11/12 enable mixing the same
    /// channels into the rear (back) channel. Other bits are reserved.
    pub fn audio_mix_mode(&self) -> AudioMixMode {
        let raw = self.sprm(SPRM_AMXMD);
        AudioMixMode {
            mix_2_to_front: (raw >> 2) & 1 == 1,
            mix_3_to_front: (raw >> 3) & 1 == 1,
            mix_4_to_front: (raw >> 4) & 1 == 1,
            mix_2_to_rear: (raw >> 10) & 1 == 1,
            mix_3_to_rear: (raw >> 11) & 1 == 1,
            mix_4_to_rear: (raw >> 12) & 1 == 1,
            raw,
        }
    }

    /// SPRM 14 video preference and current mode — bits 10..=11 carry
    /// the preferred display aspect ratio; bits 8..=9 carry the
    /// current display mode.
    pub fn video_preference(&self) -> VideoPreference {
        let raw = self.sprm(SPRM_VIDEO_PREF);
        VideoPreference {
            aspect: AspectRatio::from_bits(((raw >> 10) & 0b11) as u8),
            mode: DisplayMode::from_bits(((raw >> 8) & 0b11) as u8),
            raw,
        }
    }

    /// SPRM 15 player audio capabilities — decodes the per-codec
    /// capability bitmap into a typed view. `0` (no bits set) means
    /// "cannot play anything" per the spec page.
    pub fn audio_capabilities(&self) -> AudioCapabilities {
        let raw = self.sprm(SPRM_AUDIO_CAPS);
        AudioCapabilities {
            sdds_karaoke: (raw >> 2) & 1 == 1,
            dts_karaoke: (raw >> 3) & 1 == 1,
            mpeg_karaoke: (raw >> 4) & 1 == 1,
            dolby_karaoke: (raw >> 6) & 1 == 1,
            pcm_karaoke: (raw >> 7) & 1 == 1,
            sdds: (raw >> 10) & 1 == 1,
            dts: (raw >> 11) & 1 == 1,
            mpeg: (raw >> 12) & 1 == 1,
            dolby: (raw >> 14) & 1 == 1,
            raw,
        }
    }

    /// SPRM 20 player region-code mask — bit `i` set ⇒ this player is
    /// authorised to play region `i + 1` discs. Returns the bool
    /// for the supplied region number (`1..=8`). Region `0` and
    /// regions `9..` always return `false`.
    pub fn region_allowed(&self, region: u8) -> bool {
        if !(1..=8).contains(&region) {
            return false;
        }
        let raw = self.sprm(SPRM_REGION_MASK);
        (raw >> (region - 1)) & 1 == 1
    }

    /// SPRM 20 — full region-mask byte for callers that want to push
    /// the bitmap downstream verbatim.
    pub fn region_mask(&self) -> u8 {
        self.sprm(SPRM_REGION_MASK) as u8
    }

    // -----------------------------------------------------------------
    // Language / region / parental / audio-angle accessors for the
    // remaining SPRMs whose 16-bit slot carries either two ASCII
    // characters (ISO 639 language / ISO 3166 country) or a sentinel-
    // typed integer. Layout per
    // `docs/container/dvd/application/mpucoder-sprm.html`.
    // -----------------------------------------------------------------

    /// SPRM 0 preferred menu language — two-byte ISO 639 alpha-2 code
    /// stored as high byte first character, low byte second character.
    pub fn menu_language(&self) -> LanguageCode {
        LanguageCode::from_raw(self.sprm(SPRM_MENU_LANG))
    }

    /// SPRM 1 audio stream number (`0..=7`, `15` = none) — returns
    /// the typed view that distinguishes the `15` sentinel from a
    /// real stream index.
    pub fn audio_stream(&self) -> AudioStreamSelector {
        AudioStreamSelector::from_raw(self.sprm(SPRM_AUDIO_STREAM))
    }

    /// SPRM 3 angle number (`1..=9`) — returns the raw lower byte.
    /// Values outside `1..=9` collapse to `None`.
    pub fn angle_number(&self) -> Option<u8> {
        let v = self.sprm(SPRM_ANGLE) as u8;
        if (1..=9).contains(&v) {
            Some(v)
        } else {
            None
        }
    }

    /// SPRM 12 parental management country code — ISO 3166 alpha-2
    /// code packed identically to SPRM 0.
    pub fn parental_country(&self) -> LanguageCode {
        LanguageCode::from_raw(self.sprm(SPRM_CC_PLT))
    }

    /// SPRM 13 parental level — typed view that distinguishes the
    /// `15` sentinel ("none / parental control off") from a real
    /// level (`1..=8`).
    pub fn parental_level(&self) -> ParentalLevel {
        ParentalLevel::from_raw(self.sprm(SPRM_PARENTAL_LEVEL))
    }

    /// SPRM 16 preferred audio language — ISO 639 alpha-2 code, or
    /// the `0xFFFF` "not specified" sentinel.
    pub fn preferred_audio_language(&self) -> LanguageCode {
        LanguageCode::from_raw(self.sprm(SPRM_PREF_AUDIO_LANG))
    }

    /// SPRM 17 preferred audio language extension.
    pub fn preferred_audio_language_ext(&self) -> AudioLanguageExt {
        AudioLanguageExt::from_raw(self.sprm(SPRM_PREF_AUDIO_LANG_EXT) as u8)
    }

    /// SPRM 18 preferred sub-picture language — ISO 639 alpha-2 code,
    /// or the `0xFFFF` "not specified" sentinel.
    pub fn preferred_subpicture_language(&self) -> LanguageCode {
        LanguageCode::from_raw(self.sprm(SPRM_PREF_SUBP_LANG))
    }

    /// SPRM 19 preferred sub-picture language extension.
    pub fn preferred_subpicture_language_ext(&self) -> SubpictureLanguageExt {
        SubpictureLanguageExt::from_raw(self.sprm(SPRM_PREF_SUBP_LANG_EXT) as u8)
    }
}

/// Decoded view of SPRM 2 (`SPSTN`).
///
/// `stream` is the raw 6-bit field (0..=31 = stream index, 62 = none,
/// 63 = forced); `display` is bit 6 — when `false`, the sub-picture
/// must not be displayed even if a stream is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubpictureStreamView {
    /// Bits 0..=5 of SPRM 2 — the raw stream identifier.
    pub stream: u8,
    /// Bit 6 of SPRM 2 — `true` ⇒ display the chosen sub-picture
    /// stream; `false` ⇒ suppress display regardless of `stream`.
    pub display: bool,
    /// The original SPRM 2 word for callers that want to round-trip it.
    pub raw: u16,
}

impl SubpictureStreamView {
    /// `true` when the stream field encodes the "none" sentinel (`62`).
    pub fn is_none_sentinel(self) -> bool {
        self.stream == 62
    }
    /// `true` when the stream field encodes the "forced" sentinel (`63`).
    pub fn is_forced_sentinel(self) -> bool {
        self.stream == 63
    }
}

/// Decoded view of SPRM 11 (`AMXMD`).
///
/// Each `mix_<N>_to_<front|rear>` flag follows the spec page's bit
/// allocation: bit 2 = "mix ch 2 into front", bit 3 = "mix ch 3 into
/// front", bit 4 = "mix ch 4 into front", bits 10/11/12 do the same
/// for the rear destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioMixMode {
    /// Mix channel 2 into the front-left channel.
    pub mix_2_to_front: bool,
    /// Mix channel 3 into the front-left channel.
    pub mix_3_to_front: bool,
    /// Mix channel 4 into the front-left channel.
    pub mix_4_to_front: bool,
    /// Mix channel 2 into the rear (back) channel.
    pub mix_2_to_rear: bool,
    /// Mix channel 3 into the rear (back) channel.
    pub mix_3_to_rear: bool,
    /// Mix channel 4 into the rear (back) channel.
    pub mix_4_to_rear: bool,
    /// Raw SPRM 11 word.
    pub raw: u16,
}

/// Decoded view of SPRM 14 (video preference + current mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPreference {
    /// Bits 10..=11 — preferred display aspect ratio.
    pub aspect: AspectRatio,
    /// Bits 8..=9 — current display mode.
    pub mode: DisplayMode,
    /// Raw SPRM 14 word.
    pub raw: u16,
}

/// Preferred display aspect-ratio code per SPRM 14 bits 10..=11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AspectRatio {
    /// `0` — 4:3.
    Ar4x3,
    /// `1` — not specified.
    NotSpecified,
    /// `2` — reserved.
    Reserved,
    /// `3` — 16:9.
    Ar16x9,
}

impl AspectRatio {
    /// Decode from the 2-bit field.
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0 => Self::Ar4x3,
            1 => Self::NotSpecified,
            2 => Self::Reserved,
            _ => Self::Ar16x9,
        }
    }
}

/// Current display mode per SPRM 14 bits 8..=9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// `0` — normal.
    Normal,
    /// `1` — pan/scan.
    PanScan,
    /// `2` — letterbox.
    Letterbox,
    /// `3` — reserved.
    Reserved,
}

impl DisplayMode {
    /// Decode from the 2-bit field.
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0 => Self::Normal,
            1 => Self::PanScan,
            2 => Self::Letterbox,
            _ => Self::Reserved,
        }
    }
}

/// Decoded view of SPRM 15 (player audio capability bitmap).
///
/// `false` in every slot ⇒ the spec page's "cannot play" interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioCapabilities {
    /// Bit 2 — SDDS karaoke.
    pub sdds_karaoke: bool,
    /// Bit 3 — DTS karaoke.
    pub dts_karaoke: bool,
    /// Bit 4 — MPEG karaoke.
    pub mpeg_karaoke: bool,
    /// Bit 6 — Dolby karaoke.
    pub dolby_karaoke: bool,
    /// Bit 7 — PCM karaoke.
    pub pcm_karaoke: bool,
    /// Bit 10 — SDDS.
    pub sdds: bool,
    /// Bit 11 — DTS.
    pub dts: bool,
    /// Bit 12 — MPEG.
    pub mpeg: bool,
    /// Bit 14 — Dolby.
    pub dolby: bool,
    /// Raw SPRM 15 word.
    pub raw: u16,
}

impl AudioCapabilities {
    /// `true` ⇒ no capability bits are set; the player cannot play
    /// any of the documented audio types per the spec page.
    pub fn cannot_play(self) -> bool {
        self.raw == 0
    }
}

/// Two-byte ASCII language / country code packed into a single `u16`
/// register slot — the high byte is the first character, the low byte
/// the second character, per the SPRM 0 / 12 / 16 / 18 layout on
/// `docs/container/dvd/application/mpucoder-sprm.html`. The
/// `0xFFFF` value spelled out in the SPRM 16 / 18 default column is
/// the spec page's "not specified" sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageCode {
    /// The original SPRM word — exposed so a caller can round-trip
    /// the slot bit-for-bit including the `0xFFFF` sentinel.
    pub raw: u16,
}

impl LanguageCode {
    /// `0xFFFF` — the SPRM 16 / 18 default "preferred language not
    /// specified" sentinel.
    pub const NOT_SPECIFIED: u16 = 0xFFFF;

    /// Wrap a raw 16-bit SPRM slot.
    pub fn from_raw(raw: u16) -> Self {
        Self { raw }
    }

    /// `true` when the slot encodes the `0xFFFF` "not specified"
    /// sentinel used by the SPRM 16 / 18 defaults.
    pub fn is_not_specified(self) -> bool {
        self.raw == Self::NOT_SPECIFIED
    }

    /// Return the two-byte ASCII code as a `[u8; 2]` when the slot
    /// carries one (i.e. is not the `0xFFFF` sentinel and both bytes
    /// are printable ASCII letters per ISO 639 / ISO 3166). Returns
    /// `None` otherwise — including the "player specific" default
    /// for SPRM 0 / 12 (which has no on-wire representation; the
    /// spec leaves the slot uninitialised).
    pub fn ascii_bytes(self) -> Option<[u8; 2]> {
        if self.is_not_specified() {
            return None;
        }
        let hi = (self.raw >> 8) as u8;
        let lo = self.raw as u8;
        if hi.is_ascii_alphabetic() && lo.is_ascii_alphabetic() {
            Some([hi, lo])
        } else {
            None
        }
    }

    /// Return the code as a 2-character `String`, lowercased per
    /// ISO 639 / ISO 3166 alpha-2 convention. `None` when no valid
    /// ASCII code is present.
    pub fn as_string(self) -> Option<String> {
        let [a, b] = self.ascii_bytes()?;
        Some(format!(
            "{}{}",
            (a.to_ascii_lowercase()) as char,
            (b.to_ascii_lowercase()) as char,
        ))
    }
}

/// Decoded view of SPRM 1 (`ASTN`).
///
/// The spec table allows `0..=7` (a real stream index) and `15` (the
/// "no audio selected" sentinel). Any other value is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioStreamSelector {
    /// `0..=7` — concrete audio stream index.
    Stream(u8),
    /// `15` — "no audio" sentinel.
    None,
    /// Out-of-range raw value preserved verbatim for round-tripping.
    Invalid(u16),
}

impl AudioStreamSelector {
    /// Decode the SPRM 1 slot.
    pub fn from_raw(raw: u16) -> Self {
        match raw {
            0..=7 => Self::Stream(raw as u8),
            15 => Self::None,
            other => Self::Invalid(other),
        }
    }

    /// `true` ⇒ a concrete stream index was selected (not `15`, not
    /// out-of-range).
    pub fn is_stream(self) -> bool {
        matches!(self, Self::Stream(_))
    }

    /// `true` ⇒ the slot encodes the `15` "no audio" sentinel.
    pub fn is_none_sentinel(self) -> bool {
        matches!(self, Self::None)
    }
}

/// Decoded view of SPRM 13 (`PLT`).
///
/// The spec table allows `1..=8` (a real level) and `15` (the
/// "no parental control" sentinel). The "player specific" default
/// is whatever the implementing player writes into the slot at boot;
/// values outside `1..=8` / `15` are surfaced as `Invalid` so the
/// caller can choose how to handle them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentalLevel {
    /// `1..=8` — concrete parental level.
    Level(u8),
    /// `15` — "parental control off / no level set" sentinel.
    None,
    /// Out-of-range raw value preserved verbatim.
    Invalid(u16),
}

impl ParentalLevel {
    /// Decode the SPRM 13 slot.
    pub fn from_raw(raw: u16) -> Self {
        match raw {
            1..=8 => Self::Level(raw as u8),
            15 => Self::None,
            other => Self::Invalid(other),
        }
    }

    /// `true` ⇒ a concrete level (`1..=8`) is set.
    pub fn is_level(self) -> bool {
        matches!(self, Self::Level(_))
    }

    /// `true` ⇒ the slot encodes the `15` "off" sentinel.
    pub fn is_none_sentinel(self) -> bool {
        matches!(self, Self::None)
    }
}

/// Decoded view of SPRM 17 (preferred audio language extension).
///
/// Values per `mpucoder-sprm.html`: `0` = not specified, `1` = normal,
/// `2` = for visually impaired, `3` = director comments,
/// `4` = alternate director comments. Any other value collapses to
/// `Reserved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioLanguageExt {
    /// `0` — not specified.
    NotSpecified,
    /// `1` — normal.
    Normal,
    /// `2` — for visually impaired.
    VisuallyImpaired,
    /// `3` — director comments.
    DirectorComments,
    /// `4` — alternate director comments.
    AlternateDirectorComments,
    /// Any other value — preserved verbatim for round-trip.
    Reserved(u8),
}

impl AudioLanguageExt {
    /// Decode the SPRM 17 low byte.
    pub fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::NotSpecified,
            1 => Self::Normal,
            2 => Self::VisuallyImpaired,
            3 => Self::DirectorComments,
            4 => Self::AlternateDirectorComments,
            other => Self::Reserved(other),
        }
    }
}

/// Decoded view of SPRM 19 (preferred sub-picture language extension).
///
/// Values per `mpucoder-sprm.html`: `0` = not specified,
/// `1` = normal, `2` = large, `3` = children, `5` = normal captions,
/// `6` = large captions, `7` = children's captions, `9` = forced,
/// `13` = director comments, `14` = large director comments,
/// `15` = director comments for children. Other values collapse to
/// `Reserved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubpictureLanguageExt {
    /// `0` — not specified.
    NotSpecified,
    /// `1` — normal.
    Normal,
    /// `2` — large.
    Large,
    /// `3` — children.
    Children,
    /// `5` — normal captions.
    NormalCaptions,
    /// `6` — large captions.
    LargeCaptions,
    /// `7` — children's captions.
    ChildrensCaptions,
    /// `9` — forced.
    Forced,
    /// `13` — director comments.
    DirectorComments,
    /// `14` — large director comments.
    LargeDirectorComments,
    /// `15` — director comments for children.
    DirectorCommentsForChildren,
    /// Any other value — preserved verbatim for round-trip.
    Reserved(u8),
}

impl SubpictureLanguageExt {
    /// Decode the SPRM 19 low byte.
    pub fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::NotSpecified,
            1 => Self::Normal,
            2 => Self::Large,
            3 => Self::Children,
            5 => Self::NormalCaptions,
            6 => Self::LargeCaptions,
            7 => Self::ChildrensCaptions,
            9 => Self::Forced,
            13 => Self::DirectorComments,
            14 => Self::LargeDirectorComments,
            15 => Self::DirectorCommentsForChildren,
            other => Self::Reserved(other),
        }
    }
}

// =====================================================================
// VmAction — the playback-engine-visible effect of one step.
// =====================================================================

/// Effect of a single executed [`NavInstruction`] visible to the
/// playback engine.
///
/// "Visible" here means "the interpreter has finished applying any
/// register / counter mutations the instruction implied, and the
/// engine must now translate this action into a disc-layer
/// operation": load a different PGC, start a different cell,
/// resume from a saved CallSS state, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmAction {
    /// Instruction completed; continue to the next word in the same
    /// command list. The interpreter's `pc` advances by one.
    Continue,
    /// `Break` was executed inside a command list; the playback
    /// engine should leave the list and proceed to whatever follows
    /// (pre → cell, cell → post, post → "next PGC").
    Break,
    /// `Exit` was executed; playback should stop entirely.
    Exit,
    /// One of the Type-1 Link family fired; the playback engine
    /// should re-enter the same PGC (or restart the current cell /
    /// program) per the [`LinkAction`] descriptor.
    Link(LinkAction),
    /// `JumpTT` — jump to a different title in the volume.
    JumpTitle { ttn: u8 },
    /// `JumpVTS_TT` — jump to a different title inside the same VTS.
    JumpVtsTitle { ttn: u8 },
    /// `JumpVTS_PTT` — jump to a specific chapter of a VTS-internal
    /// title.
    JumpVtsPtt { ttn: u8, pttn: u16 },
    /// `JumpSS` — cross-domain jump (no resume registered).
    JumpSs(JumpSSTarget),
    /// `CallSS` — cross-domain call (resume point pushed onto the
    /// RSM stack before transferring control).
    CallSs(CallSSTarget),
    /// A Type-1 link subset selected `RSM` — pop the RSM stack and
    /// return to whichever cell + PC was saved when the matching
    /// CallSS fired. The `target` carries the saved location the
    /// engine should resume to.
    Resume(ResumePoint),
    /// `SetNVTMR` — the navigation timer was loaded; the playback
    /// engine owns the wall clock and must arrange to fire a
    /// `LinkPGCN(pgcn)` once the timer expires.
    SetNavTimer { seconds: u16, pgcn: u16 },
    /// The instruction was structurally `Unknown` (Type-7) or
    /// `Invalid` (red row in the opcode table). The interpreter
    /// applied no mutation and advanced the PC; surfacing the raw
    /// command lets a downstream debugger inspect what was on disc.
    NoOpRaw(NavCommand),
}

/// Detailed form of a Type-1 link-family transfer.
///
/// The Type-1 family covers two related-but-distinct destination
/// styles: the *coarse* `Link*` enum-style subset (restart current
/// cell, advance to next PG, etc. — destination is "wherever the
/// PGC's pre/post/cell layout says") and the *numbered* family
/// (`LinkPGCN(pgcn)`, `LinkPTTN(pttn)`, `LinkPGN(pgn)`, `LinkCN(cn)`
/// — destination is an explicit numeric index). We surface both
/// flavours via dedicated variants so a player can dispatch with
/// `match` and avoid re-decoding the originating instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    /// One of the 13 [`LinkSubset`] forms — `LinkTopCell` /
    /// `LinkNextCell` / `LinkPrevCell` / `LinkTopPG` / … /
    /// `LinkTailPGC` / `Nop` plus the spec's `Invalid` bag.
    Subset { subset: LinkSubset, hl_bn: u8 },
    /// `LinkPGCN pgcn` — switch to a different PGC by number.
    Pgcn { pgcn: u16 },
    /// `LinkPTTN pttn` — switch to a specific PTT (chapter) by
    /// number.
    Pttn { pttn: u16, hl_bn: u8 },
    /// `LinkPGN pgn` — switch to a specific PG (program) by number
    /// inside the current PGC.
    Pgn { pgn: u8, hl_bn: u8 },
    /// `LinkCN cn` — switch to a specific Cell by number inside the
    /// current PGC.
    Cn { cn: u8, hl_bn: u8 },
}

/// Saved playback location pushed by `CallSS` and popped by `RSM`.
///
/// The spec page lets `CallSS` optionally name a *different* resume
/// cell than the one that was active at call time — when the field
/// is non-zero the engine resumes to that cell index instead of the
/// caller's. We preserve it verbatim so the engine can decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumePoint {
    /// The cell index to resume to (0 = "the cell that was active
    /// when CallSS fired" — the engine consults its own bookkeeping).
    pub resume_cell: u8,
    /// The highlight-button override carried by the matching `RSM`
    /// subset word (byte 6 bits 5..0 in the link-subset encoding).
    pub hl_btn: u8,
}

// =====================================================================
// Vm — the interpreter.
// =====================================================================

/// Maximum depth of the call/resume stack. The spec page doesn't
/// publish a hard bound; commercial discs are routinely seen with
/// 1–2 simultaneous CallSS frames (Menu Call into a PGC that itself
/// CallSS's a sub-menu). 8 is a comfortable bound that detects
/// runaway nesting without restricting real content.
pub const MAX_RSM_DEPTH: usize = 8;

/// DVD-Video VM interpreter — owns the register file + RSM stack
/// + the per-list program counter.
///
/// The interpreter is intentionally **single-list-scoped**: one
/// instance covers one pre / post / cell command list at a time, with
/// the PC indexing into the originating [`Vec<NavCommand>`]. A
/// playback engine instantiates a new [`Vm`] (or rewinds an existing
/// one) for each transition between lists. The persistent state that
/// outlives a list — GPRMs, SPRMs, counter modes, RSM stack — lives
/// on the [`Vm`] and survives `run_list` calls.
#[derive(Debug, Clone, Default)]
pub struct Vm {
    /// Mutable register file (GPRMs + SPRMs + counter-mode bits).
    pub regs: RegisterFile,
    /// CallSS resume stack. Top of stack is the most-recently-pushed
    /// frame.
    rsm_stack: Vec<ResumePoint>,
    /// Program counter inside the currently-running list. Bumped by
    /// `Continue` / explicit `Goto`. Cleared between `run_list`
    /// invocations.
    pc: usize,
}

impl Vm {
    /// Construct a fresh VM with the spec-defined SPRM defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the current PC. Inside [`Vm::run_list`] this advances
    /// instruction by instruction; outside, it's the PC at which the
    /// last `run_list` either completed (`pc == list.len()`) or
    /// terminated via `Break` / `Exit` / a transfer action.
    pub fn pc(&self) -> usize {
        self.pc
    }

    /// Reset the PC to `0`. Use when switching to a fresh command
    /// list.
    pub fn reset_pc(&mut self) {
        self.pc = 0;
    }

    /// Push a CallSS resume frame. Returns `false` if the stack is
    /// already at [`MAX_RSM_DEPTH`] — the call is dropped rather than
    /// silently overflowing.
    pub fn push_resume(&mut self, frame: ResumePoint) -> bool {
        if self.rsm_stack.len() >= MAX_RSM_DEPTH {
            false
        } else {
            self.rsm_stack.push(frame);
            true
        }
    }

    /// Pop the most-recent CallSS resume frame, or `None` when the
    /// stack is empty (spec's "RSM with no matching CallSS" is a
    /// no-op on real players).
    pub fn pop_resume(&mut self) -> Option<ResumePoint> {
        self.rsm_stack.pop()
    }

    /// Inspect the current resume-stack depth (testing convenience).
    pub fn resume_depth(&self) -> usize {
        self.rsm_stack.len()
    }

    /// Evaluate a comparison predicate against two values.
    ///
    /// Per `mpucoder-vmi.html`'s "SET and CMP operations" table:
    /// `BC` is the bit-clear test `(lhs & rhs) == 0`; the named
    /// arithmetic predicates are unsigned 16-bit comparisons. The
    /// `None` predicate yields `true` so a "compare + something"
    /// encoding with no comparator runs the inner action
    /// unconditionally — that's how the spec page's compound rows
    /// describe their unconditional sub-cases.
    pub fn evaluate(cmp: CmpOp, lhs: u16, rhs: u16) -> bool {
        match cmp {
            CmpOp::None => true,
            CmpOp::Bc => (lhs & rhs) == 0,
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ne => lhs != rhs,
            CmpOp::Ge => lhs >= rhs,
            CmpOp::Gt => lhs > rhs,
            CmpOp::Le => lhs <= rhs,
            CmpOp::Lt => lhs < rhs,
        }
    }

    /// Apply a SET sub-op to `(dst, src)` and return the post-op
    /// destination value.
    ///
    /// Per `mpucoder-vmi.html`: the named arithmetic ops are unsigned
    /// 16-bit modular arithmetic; `Div` / `Mod` with a zero divisor
    /// leave the destination unchanged (the spec page doesn't define
    /// the result, and crashing the VM on a malformed disc is a
    /// worse outcome than no-op'ing the divide). `Swp` returns the
    /// new destination value; the caller is responsible for writing
    /// the swapped source back via [`Vm::set_swap_source`] when
    /// `Swp` was the op.
    pub fn apply_set(op: SetOp, dst: u16, src: u16) -> u16 {
        match op {
            SetOp::None => dst,
            SetOp::Mov => src,
            SetOp::Swp => src, // caller writes dst's old value into the source slot
            SetOp::Add => dst.wrapping_add(src),
            SetOp::Sub => dst.wrapping_sub(src),
            SetOp::Mul => dst.wrapping_mul(src),
            SetOp::Div => dst.checked_div(src).unwrap_or(dst),
            SetOp::Mod => dst.checked_rem(src).unwrap_or(dst),
            SetOp::Rnd => {
                // The spec page leaves the operand column blank for
                // `rnd`; treat the source as the upper bound of a
                // `[0, src)` half-open range. With `src == 0` we
                // leave the destination unchanged (same as div/mod
                // zero-divisor fallback). The interpreter has no
                // entropy source — a `0` placeholder is deterministic
                // and traceable; callers that need true randomness
                // wrap the VM and post-process this slot.
                if src == 0 {
                    dst
                } else {
                    0
                }
            }
            SetOp::And => dst & src,
            SetOp::Or => dst | src,
            SetOp::Xor => dst ^ src,
            SetOp::Invalid(_) => dst,
        }
    }

    /// Execute one decoded [`NavInstruction`] and return its
    /// playback-engine-visible effect. The PC is **not** mutated by
    /// this method — [`Vm::run_list`] owns that bookkeeping so a
    /// caller that wants single-step debugging can re-use `step`
    /// directly.
    pub fn step(&mut self, ins: NavInstruction) -> VmAction {
        match ins {
            // -------- Type 0 ----------------------------------------
            NavInstruction::Nop => VmAction::Continue,
            NavInstruction::Goto { line: _ } => {
                // Goto is resolved by run_list (the PC index is the
                // 1-based line number); a bare step() call doesn't
                // own the PC and treats Goto as a Continue. run_list
                // intercepts before this point.
                VmAction::Continue
            }
            NavInstruction::Break => VmAction::Break,
            NavInstruction::SetTmpPml { level, line: _ } => {
                // SetTmpPML asks the player to set a *temporary*
                // parental level — distinct from SPRM 13 (the
                // persistent level). We don't model the password
                // workflow at this layer; record the request on
                // SPRM 13 as a best-effort and continue. (The spec
                // page describes the password / approval flow as
                // player-policy; the VM word itself only carries the
                // proposed level.)
                self.regs.set_sprm(SPRM_PARENTAL_LEVEL, u16::from(level));
                VmAction::Continue
            }

            // -------- Type 1 — link family --------------------------
            NavInstruction::LinkSub { subset, hl_bn } => match subset {
                LinkSubset::Rsm => match self.pop_resume() {
                    Some(mut rp) => {
                        rp.hl_btn = hl_bn;
                        VmAction::Resume(rp)
                    }
                    None => VmAction::Continue,
                },
                _ => VmAction::Link(LinkAction::Subset { subset, hl_bn }),
            },
            NavInstruction::LinkPgcn { pgcn } => VmAction::Link(LinkAction::Pgcn { pgcn }),
            NavInstruction::LinkPttn { pttn, hl_bn } => {
                VmAction::Link(LinkAction::Pttn { pttn, hl_bn })
            }
            NavInstruction::LinkPgn { pgn, hl_bn } => {
                VmAction::Link(LinkAction::Pgn { pgn, hl_bn })
            }
            NavInstruction::LinkCn { cn, hl_bn } => VmAction::Link(LinkAction::Cn { cn, hl_bn }),

            // -------- Type 1 — jump / call family -------------------
            NavInstruction::Exit => VmAction::Exit,
            NavInstruction::JumpTT { ttn } => VmAction::JumpTitle { ttn },
            NavInstruction::JumpVtsTt { ttn } => VmAction::JumpVtsTitle { ttn },
            NavInstruction::JumpVtsPtt { ttn, pttn } => VmAction::JumpVtsPtt { ttn, pttn },
            NavInstruction::JumpSs(t) => VmAction::JumpSs(t),
            NavInstruction::CallSs(t) => {
                let rsm_cell = match t {
                    CallSSTarget::FirstPlay { rsm_cell } => rsm_cell,
                    CallSSTarget::VmgmMenu { rsm_cell, .. } => rsm_cell,
                    CallSSTarget::VtsmMenu { rsm_cell, .. } => rsm_cell,
                    CallSSTarget::VmgmPgcn { rsm_cell, .. } => rsm_cell,
                };
                let _pushed = self.push_resume(ResumePoint {
                    resume_cell: rsm_cell,
                    hl_btn: 0,
                });
                VmAction::CallSs(t)
            }

            // -------- Type 2 — SetSystem family ---------------------
            NavInstruction::SetStn {
                direct,
                af,
                audio_src,
                sf,
                subpic_src,
                nf,
                angle_src,
            } => {
                // Register form reads from G<src>; immediate form
                // uses the 7-bit literal directly. The spec page
                // makes the per-flag application order indifferent
                // (flags are independent).
                if af {
                    let v = if direct {
                        u16::from(audio_src)
                    } else {
                        self.regs.gprm(audio_src)
                    };
                    self.regs.set_sprm(SPRM_AUDIO_STREAM, v);
                }
                if sf {
                    let v = if direct {
                        u16::from(subpic_src)
                    } else {
                        self.regs.gprm(subpic_src)
                    };
                    self.regs.set_sprm(SPRM_SUBPICTURE_STREAM, v);
                }
                if nf {
                    let v = if direct {
                        u16::from(angle_src)
                    } else {
                        self.regs.gprm(angle_src)
                    };
                    self.regs.set_sprm(SPRM_ANGLE, v);
                }
                VmAction::Continue
            }
            NavInstruction::SetNvtmr { src, pgcn } => {
                let seconds = self.regs.read_operand(src);
                self.regs.set_sprm(SPRM_NV_TIMER, seconds);
                self.regs.set_sprm(SPRM_NV_PGCN, pgcn);
                VmAction::SetNavTimer { seconds, pgcn }
            }
            NavInstruction::SetGprmMd { src, dst, counter } => {
                let v = self.regs.read_operand(src);
                if let Register::Gprm(i) = dst {
                    self.regs.set_gprm(i, v);
                    self.regs.set_counter_mode(i, counter);
                }
                VmAction::Continue
            }
            NavInstruction::SetAmxMd { src } => {
                let v = self.regs.read_operand(src);
                self.regs.set_sprm(SPRM_AMXMD, v);
                VmAction::Continue
            }
            NavInstruction::SetHlBtnn { src } => {
                let v = self.regs.read_operand(src);
                self.regs.set_sprm(SPRM_HL_BTNN, v);
                VmAction::Continue
            }

            // -------- Type 3 — Set arithmetic -----------------------
            NavInstruction::Set { op, dst, src } => {
                if let Register::Gprm(i) = dst {
                    let cur = self.regs.gprm(i);
                    let rhs = self.regs.read_operand(src);
                    let new = Self::apply_set(op, cur, rhs);
                    self.regs.set_gprm(i, new);
                    // Swp also writes the swapped value back into
                    // the source slot when the source was a register.
                    if matches!(op, SetOp::Swp) {
                        if let Operand::Register(Register::Gprm(j)) = src {
                            self.regs.set_gprm(j, cur);
                        }
                    }
                }
                VmAction::Continue
            }

            // -------- Type 4..6 — compound CMP/SET/LNK families -----
            //
            // The decoder now carries the full operand fields for
            // the compound forms, so the executor performs the
            // implied SET + CMP + LINK sequence in spec order per
            // `mpucoder-vmi-sum.html`:
            //
            //   Type 4 SetCLnk  : (1) SET; (2) CMP; (3) Link on true.
            //   Type 5 CSetCLnk : (1) CMP; on true → (2) SET, (3) Link.
            //   Type 6 CmpSetLnk: (1) CMP; on true → (2) SET; (3) Link
            //                     unconditionally.
            //
            // A failing compare in Type 4 / 5 returns `Continue` so
            // the outer command list keeps walking; Type 6 always
            // surfaces the Link target (because its Link runs
            // regardless of the CMP outcome — that's what
            // distinguishes it from Type 5).
            NavInstruction::SetCLnk {
                set_op,
                cmp_op,
                scr,
                set_src,
                cmp_rhs,
                hl_bn,
                link,
            } => self.exec_set_clnk(set_op, cmp_op, scr, set_src, cmp_rhs, hl_bn, link),

            NavInstruction::CSetCLnk {
                set_op,
                cmp_op,
                sr1,
                set_src,
                cmp_lhs,
                cmp_rhs,
                hl_bn,
                link,
            } => self.exec_cset_clnk(set_op, cmp_op, sr1, set_src, cmp_lhs, cmp_rhs, hl_bn, link),

            NavInstruction::CmpSetLnk {
                set_op,
                cmp_op,
                sr1,
                set_src,
                cmp_rhs,
                hl_bn,
                link,
            } => self.exec_cmp_set_lnk(set_op, cmp_op, sr1, set_src, cmp_rhs, hl_bn, link),

            // -------- Type 7 / red rows -----------------------------
            NavInstruction::Unknown | NavInstruction::Invalid => {
                VmAction::NoOpRaw(NavCommand::default())
            }
        }
    }

    // ---- Type 4..6 compound execution helpers --------------------------
    //
    // All three helpers funnel into [`Vm::fire_link`] for the final
    // step: turn the Link subset into the corresponding `VmAction`
    // (Link / Continue / Resume), honouring the spec's RSM-pops-stack
    // semantics inside compound bodies as well.

    /// Resolve a Link-subset code into the appropriate VM action.
    ///
    /// `LinkSubset::Nop` collapses to `Continue` (the compound body
    /// ran but its tail Link was a no-op); `LinkSubset::Rsm` pops the
    /// RSM stack just as a bare Type-1 `LinkSub` would; the 11
    /// remaining named subsets become `VmAction::Link(Subset { … })`.
    /// `LinkSubset::Invalid(_)` falls through to `Continue` so a
    /// malformed disc cannot crash the player.
    fn fire_link(&mut self, link: LinkSubset, hl_bn: u8) -> VmAction {
        match link {
            LinkSubset::Nop => VmAction::Continue,
            LinkSubset::Rsm => match self.pop_resume() {
                Some(mut rp) => {
                    rp.hl_btn = hl_bn;
                    VmAction::Resume(rp)
                }
                None => VmAction::Continue,
            },
            LinkSubset::Invalid(_) => VmAction::Continue,
            _ => VmAction::Link(LinkAction::Subset {
                subset: link,
                hl_bn,
            }),
        }
    }

    /// Apply a SET sub-op against `dst`, writing the result back and
    /// handling the `Swp` cooperative write-back. `dst` must be a
    /// GPRM — non-GPRM destinations (SPRM / Invalid) silently no-op
    /// per the spec page's "compound SET writes a GPRM" wording.
    fn apply_set_to_register(&mut self, op: SetOp, dst: Register, src: Operand) {
        let Register::Gprm(i) = dst else {
            return;
        };
        let cur = self.regs.gprm(i);
        let rhs = self.regs.read_operand(src);
        let new = Self::apply_set(op, cur, rhs);
        self.regs.set_gprm(i, new);
        if matches!(op, SetOp::Swp) {
            if let Operand::Register(Register::Gprm(j)) = src {
                self.regs.set_gprm(j, cur);
            }
        }
    }

    /// Type 4 — `SetCLnk`: SET first, then CMP, then Link if the
    /// compare succeeded. The CMP uses the post-SET value of `scr`.
    #[allow(clippy::too_many_arguments)]
    fn exec_set_clnk(
        &mut self,
        set_op: SetOp,
        cmp_op: CmpOp,
        scr: Register,
        set_src: Operand,
        cmp_rhs: Operand,
        hl_bn: u8,
        link: LinkSubset,
    ) -> VmAction {
        // (1) SET — only fires if `set_op` is a real op; `None` makes
        // the family collapse into a plain compare-link.
        if !matches!(set_op, SetOp::None | SetOp::Invalid(_)) {
            self.apply_set_to_register(set_op, scr, set_src);
        }
        // (2) CMP against the post-SET value of `scr`.
        let lhs = self.regs.read(scr);
        let rhs = self.regs.read_operand(cmp_rhs);
        if Self::evaluate(cmp_op, lhs, rhs) {
            // (3) Link on true.
            self.fire_link(link, hl_bn)
        } else {
            VmAction::Continue
        }
    }

    /// Type 5 — `CSetCLnk`: CMP first; on true, SET then Link.
    #[allow(clippy::too_many_arguments)]
    fn exec_cset_clnk(
        &mut self,
        set_op: SetOp,
        cmp_op: CmpOp,
        sr1: Register,
        set_src: Operand,
        cmp_lhs: Register,
        cmp_rhs: Operand,
        hl_bn: u8,
        link: LinkSubset,
    ) -> VmAction {
        // (1) CMP.
        let lhs = self.regs.read(cmp_lhs);
        let rhs = self.regs.read_operand(cmp_rhs);
        if !Self::evaluate(cmp_op, lhs, rhs) {
            return VmAction::Continue;
        }
        // (2) SET on the true branch.
        if !matches!(set_op, SetOp::None | SetOp::Invalid(_)) {
            self.apply_set_to_register(set_op, sr1, set_src);
        }
        // (3) Link on the true branch.
        self.fire_link(link, hl_bn)
    }

    /// Type 6 — `CmpSetLnk`: CMP first; on true, SET; then Link
    /// **unconditionally**. The CMP outcome only gates the SET, not
    /// the Link — that's how Type 6 differs from Type 5 per
    /// `mpucoder-vmi-sum.html`.
    #[allow(clippy::too_many_arguments)]
    fn exec_cmp_set_lnk(
        &mut self,
        set_op: SetOp,
        cmp_op: CmpOp,
        sr1: Register,
        set_src: Operand,
        cmp_rhs: Operand,
        hl_bn: u8,
        link: LinkSubset,
    ) -> VmAction {
        // (1) CMP — uses the pre-SET value of `sr1`.
        let lhs = self.regs.read(sr1);
        let rhs = self.regs.read_operand(cmp_rhs);
        if Self::evaluate(cmp_op, lhs, rhs) {
            // (2) SET on the true branch.
            if !matches!(set_op, SetOp::None | SetOp::Invalid(_)) {
                self.apply_set_to_register(set_op, sr1, set_src);
            }
        }
        // (3) Link — always.
        self.fire_link(link, hl_bn)
    }

    /// Walk a pre / post / cell command list end-to-end.
    ///
    /// The PC starts at `0` (callers can pre-set via [`Vm::reset_pc`]
    /// or by mutating the field) and advances by 1 after each
    /// [`VmAction::Continue`]. Intra-list `Goto` lands the PC on
    /// the spec-defined 1-based line number; `Break` / `Exit` /
    /// every transfer action returns immediately.
    ///
    /// Returns `(VmAction, pc)` — the action that terminated the
    /// walk plus the PC at termination. A walk that runs off the
    /// end of the list returns `(VmAction::Continue, list.len())` so
    /// the caller can tell "completed cleanly" from "transferred
    /// early".
    pub fn run_list(&mut self, list: &[NavCommand]) -> (VmAction, usize) {
        self.pc = 0;
        // A bounded step budget defends against pathological encoded
        // loops (`Goto 1` from line 1, etc.). 128 × 16 = 2048 steps
        // is comfortably above the spec's "≤ 128 commands per list"
        // bound while still terminating in O(disc-size).
        let budget = list.len().saturating_mul(16).max(256);
        let mut spent = 0usize;
        while self.pc < list.len() && spent < budget {
            spent += 1;
            let ins = list[self.pc].decode();
            match ins {
                NavInstruction::Goto { line } => {
                    // Spec page: `line` is 1-based; line 1 = first
                    // command in the list. A `0` or out-of-range
                    // target falls through to the end of the list
                    // (treated as a clean completion).
                    let idx = (line as usize).saturating_sub(1);
                    if idx >= list.len() {
                        self.pc = list.len();
                    } else {
                        self.pc = idx;
                    }
                    continue;
                }
                other => {
                    let action = self.step(other);
                    match action {
                        VmAction::Continue => {
                            self.pc += 1;
                        }
                        _ => return (action, self.pc),
                    }
                }
            }
        }
        // Either we ran off the end (clean completion) or we hit the
        // step budget (pathological loop). Either way the caller
        // sees a clean Continue back at list.len() — they can detect
        // the loop case via the budget if they care.
        (VmAction::Continue, self.pc)
    }
}

// =====================================================================
// Tests.
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ifo::NavCommand;
    use crate::nav::{CmpOp, JumpSSTarget, LinkSubset, NavInstruction, Operand, Register, SetOp};

    // -----------------------------------------------------------------
    // Register file.
    // -----------------------------------------------------------------

    #[test]
    fn register_file_default_matches_spec_defaults() {
        let r = RegisterFile::new();
        // All GPRMs cleared.
        for i in 0..GPRM_COUNT {
            assert_eq!(r.gprm(i as u8), 0);
        }
        // Spec-defaulted SPRMs.
        assert_eq!(r.sprm(SPRM_AUDIO_STREAM), 15);
        assert_eq!(r.sprm(SPRM_SUBPICTURE_STREAM), 62);
        assert_eq!(r.sprm(SPRM_ANGLE), 1);
        assert_eq!(r.sprm(SPRM_TITLE), 1);
        assert_eq!(r.sprm(SPRM_VTS_TITLE), 1);
        assert_eq!(r.sprm(SPRM_PTT), 1);
        assert_eq!(r.sprm(SPRM_HL_BTNN), 1 << 10);
        assert_eq!(r.sprm(SPRM_NV_TIMER), 0);
        assert_eq!(r.sprm(16), 0xFFFF);
        assert_eq!(r.sprm(18), 0xFFFF);
    }

    #[test]
    fn register_out_of_range_indexing_returns_zero() {
        let r = RegisterFile::new();
        assert_eq!(r.gprm(99), 0);
        assert_eq!(r.sprm(99), 0);
        assert_eq!(r.read(Register::Invalid(0x7F)), 0);
    }

    #[test]
    fn register_read_operand_dispatch() {
        let mut r = RegisterFile::new();
        r.set_gprm(3, 0xCAFE);
        r.set_sprm(SPRM_HL_BTNN, 0x0800);
        assert_eq!(r.read_operand(Operand::Register(Register::Gprm(3))), 0xCAFE);
        assert_eq!(
            r.read_operand(Operand::Register(Register::Sprm(SPRM_HL_BTNN))),
            0x0800
        );
        assert_eq!(r.read_operand(Operand::Immediate(0xBEEF)), 0xBEEF);
    }

    #[test]
    fn counter_mode_flag_round_trips() {
        let mut r = RegisterFile::new();
        assert!(!r.counter_mode(5));
        r.set_counter_mode(5, true);
        assert!(r.counter_mode(5));
        assert!(!r.counter_mode(6));
        r.set_counter_mode(5, false);
        assert!(!r.counter_mode(5));
    }

    #[test]
    fn tick_counters_advances_only_counter_mode_gprms() {
        let mut r = RegisterFile::new();
        r.set_gprm(0, 100);
        r.set_gprm(1, 100);
        r.set_counter_mode(1, true);
        r.tick_counters(5);
        assert_eq!(r.gprm(0), 100);
        assert_eq!(r.gprm(1), 105);
    }

    #[test]
    fn tick_counters_saturates_at_u16_max() {
        let mut r = RegisterFile::new();
        r.set_gprm(2, u16::MAX - 3);
        r.set_counter_mode(2, true);
        r.tick_counters(10);
        assert_eq!(r.gprm(2), u16::MAX);
    }

    // -----------------------------------------------------------------
    // Comparison evaluator (covers every CmpOp variant).
    // -----------------------------------------------------------------

    #[test]
    fn evaluate_covers_every_cmp_op() {
        assert!(Vm::evaluate(CmpOp::None, 0, 0));
        assert!(Vm::evaluate(CmpOp::Bc, 0xF0, 0x0F)); // disjoint bit-sets
        assert!(!Vm::evaluate(CmpOp::Bc, 0xF0, 0x10)); // overlapping bit
        assert!(Vm::evaluate(CmpOp::Eq, 5, 5));
        assert!(!Vm::evaluate(CmpOp::Eq, 5, 4));
        assert!(Vm::evaluate(CmpOp::Ne, 5, 4));
        assert!(!Vm::evaluate(CmpOp::Ne, 5, 5));
        assert!(Vm::evaluate(CmpOp::Ge, 5, 5));
        assert!(Vm::evaluate(CmpOp::Ge, 6, 5));
        assert!(!Vm::evaluate(CmpOp::Ge, 4, 5));
        assert!(Vm::evaluate(CmpOp::Gt, 6, 5));
        assert!(!Vm::evaluate(CmpOp::Gt, 5, 5));
        assert!(Vm::evaluate(CmpOp::Le, 5, 5));
        assert!(Vm::evaluate(CmpOp::Le, 4, 5));
        assert!(!Vm::evaluate(CmpOp::Le, 6, 5));
        assert!(Vm::evaluate(CmpOp::Lt, 4, 5));
        assert!(!Vm::evaluate(CmpOp::Lt, 5, 5));
    }

    // -----------------------------------------------------------------
    // SET sub-op application.
    // -----------------------------------------------------------------

    #[test]
    fn apply_set_covers_named_arithmetic_ops() {
        assert_eq!(Vm::apply_set(SetOp::None, 5, 99), 5);
        assert_eq!(Vm::apply_set(SetOp::Mov, 5, 99), 99);
        assert_eq!(Vm::apply_set(SetOp::Add, 5, 3), 8);
        assert_eq!(Vm::apply_set(SetOp::Sub, 5, 3), 2);
        assert_eq!(Vm::apply_set(SetOp::Mul, 5, 3), 15);
        assert_eq!(Vm::apply_set(SetOp::Div, 14, 3), 4);
        assert_eq!(Vm::apply_set(SetOp::Mod, 14, 3), 2);
        assert_eq!(Vm::apply_set(SetOp::And, 0xF0F0, 0x0FF0), 0x00F0);
        assert_eq!(Vm::apply_set(SetOp::Or, 0xF000, 0x000F), 0xF00F);
        assert_eq!(Vm::apply_set(SetOp::Xor, 0xFF00, 0x0FF0), 0xF0F0);
    }

    #[test]
    fn apply_set_handles_zero_divisor_safely() {
        assert_eq!(Vm::apply_set(SetOp::Div, 5, 0), 5);
        assert_eq!(Vm::apply_set(SetOp::Mod, 5, 0), 5);
        assert_eq!(Vm::apply_set(SetOp::Rnd, 5, 0), 5);
    }

    #[test]
    fn apply_set_swp_returns_src_caller_writes_dst_back() {
        // `Swp` is intentionally cooperative; the helper returns the
        // value the destination should take, the executor stamps the
        // source-side register with the old dst value.
        assert_eq!(Vm::apply_set(SetOp::Swp, 5, 99), 99);
    }

    #[test]
    fn apply_set_arithmetic_overflow_wraps() {
        assert_eq!(Vm::apply_set(SetOp::Add, u16::MAX, 1), 0);
        assert_eq!(Vm::apply_set(SetOp::Sub, 0, 1), u16::MAX);
        assert_eq!(Vm::apply_set(SetOp::Mul, 0x0100, 0x0100), 0); // 1<<16 wraps
    }

    #[test]
    fn apply_set_invalid_sub_op_is_noop() {
        assert_eq!(Vm::apply_set(SetOp::Invalid(0x0C), 5, 99), 5);
    }

    // -----------------------------------------------------------------
    // step() — per-instruction dispatch.
    // -----------------------------------------------------------------

    #[test]
    fn step_nop_continues() {
        let mut vm = Vm::new();
        assert_eq!(vm.step(NavInstruction::Nop), VmAction::Continue);
    }

    #[test]
    fn step_break_and_exit_terminate() {
        let mut vm = Vm::new();
        assert_eq!(vm.step(NavInstruction::Break), VmAction::Break);
        assert_eq!(vm.step(NavInstruction::Exit), VmAction::Exit);
    }

    #[test]
    fn step_set_writes_gprm_via_mov() {
        let mut vm = Vm::new();
        vm.step(NavInstruction::Set {
            op: SetOp::Mov,
            dst: Register::Gprm(4),
            src: Operand::Immediate(0x1234),
        });
        assert_eq!(vm.regs.gprm(4), 0x1234);
    }

    #[test]
    fn step_set_arithmetic_chain_through_gprm() {
        let mut vm = Vm::new();
        vm.regs.set_gprm(0, 10);
        vm.step(NavInstruction::Set {
            op: SetOp::Add,
            dst: Register::Gprm(0),
            src: Operand::Immediate(5),
        });
        assert_eq!(vm.regs.gprm(0), 15);
        vm.step(NavInstruction::Set {
            op: SetOp::Mul,
            dst: Register::Gprm(0),
            src: Operand::Register(Register::Gprm(0)),
        });
        assert_eq!(vm.regs.gprm(0), 225);
    }

    #[test]
    fn step_set_swp_exchanges_two_gprms() {
        let mut vm = Vm::new();
        vm.regs.set_gprm(1, 0xAAAA);
        vm.regs.set_gprm(2, 0x5555);
        vm.step(NavInstruction::Set {
            op: SetOp::Swp,
            dst: Register::Gprm(1),
            src: Operand::Register(Register::Gprm(2)),
        });
        assert_eq!(vm.regs.gprm(1), 0x5555);
        assert_eq!(vm.regs.gprm(2), 0xAAAA);
    }

    #[test]
    fn step_setstn_honours_per_flag_application() {
        let mut vm = Vm::new();
        // Direct form: af set, sf cleared, nf set. SPRM 2 (subpic)
        // must remain at default.
        vm.step(NavInstruction::SetStn {
            direct: true,
            af: true,
            audio_src: 4,
            sf: false,
            subpic_src: 7,
            nf: true,
            angle_src: 3,
        });
        assert_eq!(vm.regs.sprm(SPRM_AUDIO_STREAM), 4);
        assert_eq!(vm.regs.sprm(SPRM_SUBPICTURE_STREAM), 62); // default
        assert_eq!(vm.regs.sprm(SPRM_ANGLE), 3);
    }

    #[test]
    fn step_setnvtmr_loads_timer_pair_and_surfaces_action() {
        let mut vm = Vm::new();
        let act = vm.step(NavInstruction::SetNvtmr {
            src: Operand::Immediate(120),
            pgcn: 42,
        });
        assert_eq!(
            act,
            VmAction::SetNavTimer {
                seconds: 120,
                pgcn: 42,
            }
        );
        assert_eq!(vm.regs.sprm(SPRM_NV_TIMER), 120);
        assert_eq!(vm.regs.sprm(SPRM_NV_PGCN), 42);
    }

    #[test]
    fn step_setgprmmd_with_counter_flag_toggles_mode_bit() {
        let mut vm = Vm::new();
        vm.step(NavInstruction::SetGprmMd {
            src: Operand::Immediate(99),
            dst: Register::Gprm(7),
            counter: true,
        });
        assert_eq!(vm.regs.gprm(7), 99);
        assert!(vm.regs.counter_mode(7));
        // Toggling off via a counter=false update.
        vm.step(NavInstruction::SetGprmMd {
            src: Operand::Immediate(0),
            dst: Register::Gprm(7),
            counter: false,
        });
        assert!(!vm.regs.counter_mode(7));
    }

    #[test]
    fn step_sethlbtnn_writes_sprm8() {
        let mut vm = Vm::new();
        vm.step(NavInstruction::SetHlBtnn {
            src: Operand::Immediate(0x0C00),
        });
        assert_eq!(vm.regs.sprm(SPRM_HL_BTNN), 0x0C00);
    }

    #[test]
    fn step_set_tmp_pml_writes_sprm13() {
        let mut vm = Vm::new();
        vm.step(NavInstruction::SetTmpPml { level: 7, line: 0 });
        assert_eq!(vm.regs.sprm(SPRM_PARENTAL_LEVEL), 7);
    }

    // -----------------------------------------------------------------
    // step() — Link / Jump / Call surfaces actions.
    // -----------------------------------------------------------------

    #[test]
    fn step_link_subset_surfaces_link_action() {
        let mut vm = Vm::new();
        let a = vm.step(NavInstruction::LinkSub {
            subset: LinkSubset::LinkNextPG,
            hl_bn: 3,
        });
        assert_eq!(
            a,
            VmAction::Link(LinkAction::Subset {
                subset: LinkSubset::LinkNextPG,
                hl_bn: 3,
            })
        );
    }

    #[test]
    fn step_link_pgcn_pttn_pgn_cn_surface_named_targets() {
        let mut vm = Vm::new();
        assert_eq!(
            vm.step(NavInstruction::LinkPgcn { pgcn: 0x1234 }),
            VmAction::Link(LinkAction::Pgcn { pgcn: 0x1234 })
        );
        assert_eq!(
            vm.step(NavInstruction::LinkPttn { pttn: 5, hl_bn: 1 }),
            VmAction::Link(LinkAction::Pttn { pttn: 5, hl_bn: 1 })
        );
        assert_eq!(
            vm.step(NavInstruction::LinkPgn { pgn: 9, hl_bn: 2 }),
            VmAction::Link(LinkAction::Pgn { pgn: 9, hl_bn: 2 })
        );
        assert_eq!(
            vm.step(NavInstruction::LinkCn { cn: 11, hl_bn: 4 }),
            VmAction::Link(LinkAction::Cn { cn: 11, hl_bn: 4 })
        );
    }

    #[test]
    fn step_jump_family_surfaces_typed_actions() {
        let mut vm = Vm::new();
        assert_eq!(
            vm.step(NavInstruction::JumpTT { ttn: 7 }),
            VmAction::JumpTitle { ttn: 7 }
        );
        assert_eq!(
            vm.step(NavInstruction::JumpVtsTt { ttn: 8 }),
            VmAction::JumpVtsTitle { ttn: 8 }
        );
        assert_eq!(
            vm.step(NavInstruction::JumpVtsPtt { ttn: 9, pttn: 4 }),
            VmAction::JumpVtsPtt { ttn: 9, pttn: 4 }
        );
        assert_eq!(
            vm.step(NavInstruction::JumpSs(JumpSSTarget::FirstPlay)),
            VmAction::JumpSs(JumpSSTarget::FirstPlay)
        );
    }

    #[test]
    fn step_callss_pushes_resume_then_rsm_pops_it() {
        let mut vm = Vm::new();
        assert_eq!(vm.resume_depth(), 0);
        let _action = vm.step(NavInstruction::CallSs(CallSSTarget::FirstPlay {
            rsm_cell: 7,
        }));
        assert_eq!(vm.resume_depth(), 1);
        let act = vm.step(NavInstruction::LinkSub {
            subset: LinkSubset::Rsm,
            hl_bn: 5,
        });
        assert_eq!(
            act,
            VmAction::Resume(ResumePoint {
                resume_cell: 7,
                hl_btn: 5,
            })
        );
        assert_eq!(vm.resume_depth(), 0);
    }

    #[test]
    fn step_rsm_with_empty_stack_is_continue() {
        let mut vm = Vm::new();
        let act = vm.step(NavInstruction::LinkSub {
            subset: LinkSubset::Rsm,
            hl_bn: 0,
        });
        assert_eq!(act, VmAction::Continue);
        assert_eq!(vm.resume_depth(), 0);
    }

    #[test]
    fn step_callss_stack_depth_bounded_to_max_rsm_depth() {
        let mut vm = Vm::new();
        for _ in 0..(MAX_RSM_DEPTH + 4) {
            let _action = vm.step(NavInstruction::CallSs(CallSSTarget::FirstPlay {
                rsm_cell: 1,
            }));
        }
        assert_eq!(vm.resume_depth(), MAX_RSM_DEPTH);
    }

    #[test]
    fn step_unknown_and_invalid_yield_noopraw() {
        let mut vm = Vm::new();
        let pre = vm.regs.clone();
        let a = vm.step(NavInstruction::Unknown);
        assert!(matches!(a, VmAction::NoOpRaw(_)));
        let b = vm.step(NavInstruction::Invalid);
        assert!(matches!(b, VmAction::NoOpRaw(_)));
        // No mutation on either path.
        assert_eq!(vm.regs.gprm(0), pre.gprm(0));
        assert_eq!(vm.regs.sprm(0), pre.sprm(0));
    }

    // -----------------------------------------------------------------
    // Type 4..6 compound execution — SET / CMP / LINK sequencing.
    // -----------------------------------------------------------------

    #[test]
    fn step_set_clnk_runs_set_then_compare_links_on_true() {
        // SetCLnk: G3 += 5; if (G3 == 10) Link LinkNextPG.
        // Set G3 = 5 first so post-SET G3 == 10.
        let mut vm = Vm::new();
        vm.regs.set_gprm(3, 5);
        let action = vm.step(NavInstruction::SetCLnk {
            set_op: SetOp::Add,
            cmp_op: CmpOp::Eq,
            scr: Register::Gprm(3),
            set_src: Operand::Immediate(5),
            cmp_rhs: Operand::Immediate(10),
            hl_bn: 2,
            link: LinkSubset::LinkNextPG,
        });
        assert_eq!(vm.regs.gprm(3), 10);
        assert_eq!(
            action,
            VmAction::Link(LinkAction::Subset {
                subset: LinkSubset::LinkNextPG,
                hl_bn: 2,
            })
        );
    }

    #[test]
    fn step_set_clnk_runs_set_but_skips_link_on_false() {
        // SetCLnk: G3 += 1 (1 -> 2); if (G3 == 10) Link. Compare
        // fails; SET still ran, but no Link surfaces.
        let mut vm = Vm::new();
        vm.regs.set_gprm(3, 1);
        let action = vm.step(NavInstruction::SetCLnk {
            set_op: SetOp::Add,
            cmp_op: CmpOp::Eq,
            scr: Register::Gprm(3),
            set_src: Operand::Immediate(1),
            cmp_rhs: Operand::Immediate(10),
            hl_bn: 0,
            link: LinkSubset::LinkNextPG,
        });
        assert_eq!(vm.regs.gprm(3), 2);
        assert_eq!(action, VmAction::Continue);
    }

    #[test]
    fn step_cset_clnk_runs_set_only_on_true() {
        // CSetCLnk: if (G7 == 9) { G3 = 99; Link LinkTopPGC }.
        // CMP true → SET runs + Link surfaces.
        let mut vm = Vm::new();
        vm.regs.set_gprm(7, 9);
        let action = vm.step(NavInstruction::CSetCLnk {
            set_op: SetOp::Mov,
            cmp_op: CmpOp::Eq,
            sr1: Register::Gprm(3),
            set_src: Operand::Immediate(99),
            cmp_lhs: Register::Gprm(7),
            cmp_rhs: Operand::Immediate(9),
            hl_bn: 0,
            link: LinkSubset::LinkTopPGC,
        });
        assert_eq!(vm.regs.gprm(3), 99);
        assert_eq!(
            action,
            VmAction::Link(LinkAction::Subset {
                subset: LinkSubset::LinkTopPGC,
                hl_bn: 0,
            })
        );
    }

    #[test]
    fn step_cset_clnk_skips_set_and_link_on_false() {
        // CSetCLnk on false: neither SET nor LINK runs.
        let mut vm = Vm::new();
        vm.regs.set_gprm(7, 1);
        vm.regs.set_gprm(3, 42);
        let action = vm.step(NavInstruction::CSetCLnk {
            set_op: SetOp::Mov,
            cmp_op: CmpOp::Eq,
            sr1: Register::Gprm(3),
            set_src: Operand::Immediate(99),
            cmp_lhs: Register::Gprm(7),
            cmp_rhs: Operand::Immediate(9),
            hl_bn: 0,
            link: LinkSubset::LinkTopPGC,
        });
        // SET did not run — G3 still 42.
        assert_eq!(vm.regs.gprm(3), 42);
        assert_eq!(action, VmAction::Continue);
    }

    #[test]
    fn step_cmp_set_lnk_links_unconditionally_even_on_false_cmp() {
        // CmpSetLnk on false: SET skipped, but LINK still fires (the
        // distinguishing semantic from Type 5).
        let mut vm = Vm::new();
        vm.regs.set_gprm(1, 1);
        let action = vm.step(NavInstruction::CmpSetLnk {
            set_op: SetOp::Mov,
            cmp_op: CmpOp::Eq,
            sr1: Register::Gprm(1),
            set_src: Operand::Immediate(99),
            cmp_rhs: Operand::Immediate(9),
            hl_bn: 5,
            link: LinkSubset::LinkNextPGC,
        });
        // SET skipped.
        assert_eq!(vm.regs.gprm(1), 1);
        // Link still fires.
        assert_eq!(
            action,
            VmAction::Link(LinkAction::Subset {
                subset: LinkSubset::LinkNextPGC,
                hl_bn: 5,
            })
        );
    }

    #[test]
    fn step_cmp_set_lnk_runs_set_on_true_then_links() {
        // CmpSetLnk on true: SET runs then LINK fires.
        let mut vm = Vm::new();
        vm.regs.set_gprm(2, 7);
        let action = vm.step(NavInstruction::CmpSetLnk {
            set_op: SetOp::Add,
            cmp_op: CmpOp::Eq,
            sr1: Register::Gprm(2),
            set_src: Operand::Immediate(3),
            cmp_rhs: Operand::Immediate(7),
            hl_bn: 0,
            link: LinkSubset::LinkPrevPG,
        });
        assert_eq!(vm.regs.gprm(2), 10);
        assert_eq!(
            action,
            VmAction::Link(LinkAction::Subset {
                subset: LinkSubset::LinkPrevPG,
                hl_bn: 0,
            })
        );
    }

    #[test]
    fn step_compound_with_link_nop_returns_continue() {
        // A compound whose Link subset is NOP collapses to Continue
        // even when the CMP succeeds (the compound body ran but its
        // tail Link is a literal NOP per the link-subset table).
        let mut vm = Vm::new();
        let action = vm.step(NavInstruction::CmpSetLnk {
            set_op: SetOp::None,
            cmp_op: CmpOp::None,
            sr1: Register::Gprm(0),
            set_src: Operand::Immediate(0),
            cmp_rhs: Operand::Immediate(0),
            hl_bn: 0,
            link: LinkSubset::Nop,
        });
        assert_eq!(action, VmAction::Continue);
    }

    #[test]
    fn step_compound_with_link_rsm_pops_resume_stack() {
        // A compound's RSM Link variant pops the same RSM stack as a
        // bare Type-1 LinkSub Rsm. Push a frame, fire a Type-6
        // compound whose Link is Rsm, observe the Resume action.
        let mut vm = Vm::new();
        assert!(vm.push_resume(ResumePoint {
            resume_cell: 4,
            hl_btn: 0,
        }));
        let action = vm.step(NavInstruction::CmpSetLnk {
            set_op: SetOp::None,
            cmp_op: CmpOp::None,
            sr1: Register::Gprm(0),
            set_src: Operand::Immediate(0),
            cmp_rhs: Operand::Immediate(0),
            hl_bn: 9,
            link: LinkSubset::Rsm,
        });
        assert_eq!(
            action,
            VmAction::Resume(ResumePoint {
                resume_cell: 4,
                hl_btn: 9,
            })
        );
        assert_eq!(vm.resume_depth(), 0);
    }

    #[test]
    fn step_compound_with_invalid_link_subset_is_continue() {
        // An `Invalid` link-subset bag (e.g. 0x04, 0x08) collapses to
        // Continue rather than panicking — malformed discs survive.
        let mut vm = Vm::new();
        let action = vm.step(NavInstruction::SetCLnk {
            set_op: SetOp::None,
            cmp_op: CmpOp::None,
            scr: Register::Gprm(0),
            set_src: Operand::Immediate(0),
            cmp_rhs: Operand::Immediate(0),
            hl_bn: 0,
            link: LinkSubset::Invalid(0x04),
        });
        assert_eq!(action, VmAction::Continue);
    }

    #[test]
    fn step_compound_setop_none_skips_set_phase() {
        // SET-op = None means the compound's SET phase is a no-op
        // even on the true branch — the destination keeps its
        // pre-existing value while CMP + LINK still fire normally.
        let mut vm = Vm::new();
        vm.regs.set_gprm(5, 42);
        let action = vm.step(NavInstruction::CSetCLnk {
            set_op: SetOp::None,
            cmp_op: CmpOp::Eq,
            sr1: Register::Gprm(5),
            set_src: Operand::Immediate(99), // would clobber 42 if SET ran
            cmp_lhs: Register::Gprm(5),
            cmp_rhs: Operand::Immediate(42),
            hl_bn: 1,
            link: LinkSubset::LinkTopCell,
        });
        // SET phase skipped.
        assert_eq!(vm.regs.gprm(5), 42);
        // CMP true; Link surfaces.
        assert_eq!(
            action,
            VmAction::Link(LinkAction::Subset {
                subset: LinkSubset::LinkTopCell,
                hl_bn: 1,
            })
        );
    }

    // -----------------------------------------------------------------
    // run_list() — PC / Goto / Break / Exit.
    // -----------------------------------------------------------------

    fn encode_nop() -> NavCommand {
        NavCommand {
            bytes: [0x00, 0x00, 0, 0, 0, 0, 0, 0],
        }
    }

    fn encode_break() -> NavCommand {
        NavCommand {
            bytes: [0x00, 0x02, 0, 0, 0, 0, 0, 0],
        }
    }

    fn encode_exit() -> NavCommand {
        // Type 1 jump/call, cmd nibble 1 = Exit.
        NavCommand {
            bytes: [0x30, 0x01, 0, 0, 0, 0, 0, 0],
        }
    }

    fn encode_goto(line: u8) -> NavCommand {
        // Type 0 cmd nibble 1, line in byte 7.
        NavCommand {
            bytes: [0x00, 0x01, 0, 0, 0, 0, 0, line],
        }
    }

    #[test]
    fn run_list_completes_cleanly_through_nops() {
        let mut vm = Vm::new();
        let list = vec![encode_nop(), encode_nop(), encode_nop()];
        let (action, pc) = vm.run_list(&list);
        assert_eq!(action, VmAction::Continue);
        assert_eq!(pc, 3);
    }

    #[test]
    fn run_list_break_returns_at_break_pc() {
        let mut vm = Vm::new();
        let list = vec![encode_nop(), encode_break(), encode_nop()];
        let (action, pc) = vm.run_list(&list);
        assert_eq!(action, VmAction::Break);
        assert_eq!(pc, 1);
    }

    #[test]
    fn run_list_exit_returns_at_exit_pc() {
        let mut vm = Vm::new();
        let list = vec![encode_nop(), encode_exit(), encode_nop()];
        let (action, pc) = vm.run_list(&list);
        assert_eq!(action, VmAction::Exit);
        assert_eq!(pc, 1);
    }

    #[test]
    fn run_list_goto_jumps_to_one_based_line() {
        let mut vm = Vm::new();
        // line 1 = idx 0; goto(3) at idx 0 → run idx 2 → break.
        let list = vec![encode_goto(3), encode_nop(), encode_break()];
        let (action, pc) = vm.run_list(&list);
        assert_eq!(action, VmAction::Break);
        assert_eq!(pc, 2);
    }

    #[test]
    fn run_list_goto_out_of_range_runs_to_end() {
        let mut vm = Vm::new();
        let list = vec![encode_goto(99), encode_nop()];
        let (action, pc) = vm.run_list(&list);
        assert_eq!(action, VmAction::Continue);
        assert_eq!(pc, list.len());
    }

    #[test]
    fn run_list_runaway_goto_loop_terminates_under_budget() {
        let mut vm = Vm::new();
        // goto(1) at idx 0 jumps back to itself — infinite loop. The
        // bounded step budget guarantees termination.
        let list = vec![encode_goto(1)];
        let (action, _) = vm.run_list(&list);
        // We don't care which final action surfaces; only that the
        // call returns at all. Continue is the budget-exhausted
        // sentinel.
        assert_eq!(action, VmAction::Continue);
    }

    #[test]
    fn run_list_pc_resets_to_zero_between_invocations() {
        let mut vm = Vm::new();
        vm.pc = 17;
        let _ = vm.run_list(&[encode_nop()]);
        // run_list started at 0 (reset), advanced past the single
        // nop, and finished at 1.
        assert_eq!(vm.pc(), 1);
    }

    // -----------------------------------------------------------------
    // Default round-trip — a default NavCommand executes as NOP.
    // -----------------------------------------------------------------

    #[test]
    fn default_navcommand_runs_as_single_nop() {
        let mut vm = Vm::new();
        let (action, pc) = vm.run_list(&[NavCommand::default()]);
        assert_eq!(action, VmAction::Continue);
        assert_eq!(pc, 1);
    }

    // -----------------------------------------------------------------
    // SPRM bitfield accessors — cross-check decode against the spec
    // page's documented bit layout for the 6 packed registers.
    // -----------------------------------------------------------------

    #[test]
    fn sprm_defaults_cover_language_extension_slots() {
        // SPRMs 17 + 19 are language-extension codes whose "not
        // specified" default is `0`; the test fixes that we honour
        // the spec's explicit default rather than leaving a vague
        // zero-fill.
        let r = RegisterFile::new();
        assert_eq!(r.sprm(SPRM_PREF_AUDIO_LANG_EXT), 0);
        assert_eq!(r.sprm(SPRM_PREF_SUBP_LANG_EXT), 0);
    }

    #[test]
    fn sprm_named_constants_match_spec_indices() {
        // Pin the spec's SPRM numbering against the named constants
        // so a renumbering accident is caught at compile-time-equivalent.
        assert_eq!(SPRM_MENU_LANG, 0);
        assert_eq!(SPRM_CC_PLT, 12);
        assert_eq!(SPRM_VIDEO_PREF, 14);
        assert_eq!(SPRM_AUDIO_CAPS, 15);
        assert_eq!(SPRM_PREF_AUDIO_LANG, 16);
        assert_eq!(SPRM_PREF_AUDIO_LANG_EXT, 17);
        assert_eq!(SPRM_PREF_SUBP_LANG, 18);
        assert_eq!(SPRM_PREF_SUBP_LANG_EXT, 19);
        assert_eq!(SPRM_REGION_MASK, 20);
    }

    #[test]
    fn subpicture_stream_default_is_none_with_display_off() {
        let r = RegisterFile::new();
        let view = r.subpicture_stream();
        // SPRM 2 default = 62 — bits 0..=5 = 62 ("none"), bit 6 = 0
        // ("do not display"). Cross-check both decoded fields.
        assert_eq!(view.stream, 62);
        assert!(!view.display);
        assert!(view.is_none_sentinel());
        assert!(!view.is_forced_sentinel());
    }

    #[test]
    fn subpicture_stream_forced_sentinel_with_display_on() {
        let mut r = RegisterFile::new();
        // stream = 63 (forced) with display bit set.
        r.set_sprm(SPRM_SUBPICTURE_STREAM, 63 | (1 << 6));
        let view = r.subpicture_stream();
        assert_eq!(view.stream, 63);
        assert!(view.display);
        assert!(view.is_forced_sentinel());
        assert!(!view.is_none_sentinel());
    }

    #[test]
    fn highlight_button_decodes_default_as_one() {
        let r = RegisterFile::new();
        assert_eq!(r.highlight_button(), 1);
    }

    #[test]
    fn highlight_button_decodes_arbitrary_value() {
        let mut r = RegisterFile::new();
        // Button 17 → bits 10..=15 = 17 → value = 17 << 10 = 17408.
        r.set_sprm(SPRM_HL_BTNN, 17 << 10);
        assert_eq!(r.highlight_button(), 17);
    }

    #[test]
    fn highlight_button_rejects_out_of_range_field() {
        let mut r = RegisterFile::new();
        // Button 0 (below the 1..=36 spec range) → decode returns 0.
        r.set_sprm(SPRM_HL_BTNN, 0);
        assert_eq!(r.highlight_button(), 0);
        // Button 37 (above the 1..=36 spec range) → decode returns 0.
        r.set_sprm(SPRM_HL_BTNN, 37 << 10);
        assert_eq!(r.highlight_button(), 0);
    }

    #[test]
    fn audio_mix_mode_decodes_all_six_documented_bits() {
        let mut r = RegisterFile::new();
        // Set all six documented bits.
        let bits = (1 << 2) | (1 << 3) | (1 << 4) | (1 << 10) | (1 << 11) | (1 << 12);
        r.set_sprm(SPRM_AMXMD, bits);
        let m = r.audio_mix_mode();
        assert!(m.mix_2_to_front);
        assert!(m.mix_3_to_front);
        assert!(m.mix_4_to_front);
        assert!(m.mix_2_to_rear);
        assert!(m.mix_3_to_rear);
        assert!(m.mix_4_to_rear);
        assert_eq!(m.raw, bits);
    }

    #[test]
    fn audio_mix_mode_default_is_no_routing() {
        let r = RegisterFile::new();
        let m = r.audio_mix_mode();
        assert!(!m.mix_2_to_front);
        assert!(!m.mix_3_to_front);
        assert!(!m.mix_4_to_front);
        assert!(!m.mix_2_to_rear);
        assert!(!m.mix_3_to_rear);
        assert!(!m.mix_4_to_rear);
    }

    #[test]
    fn video_preference_decodes_aspect_and_mode() {
        let mut r = RegisterFile::new();
        // aspect = 3 (16:9) in bits 10..=11, mode = 2 (letterbox) in bits 8..=9.
        r.set_sprm(SPRM_VIDEO_PREF, (0b11 << 10) | (0b10 << 8));
        let p = r.video_preference();
        assert_eq!(p.aspect, AspectRatio::Ar16x9);
        assert_eq!(p.mode, DisplayMode::Letterbox);
    }

    #[test]
    fn video_preference_covers_every_named_code() {
        // Round-trip each aspect/mode pair so a bit-position regression
        // surfaces here rather than as a misdecoded run-time field.
        assert_eq!(AspectRatio::from_bits(0), AspectRatio::Ar4x3);
        assert_eq!(AspectRatio::from_bits(1), AspectRatio::NotSpecified);
        assert_eq!(AspectRatio::from_bits(2), AspectRatio::Reserved);
        assert_eq!(AspectRatio::from_bits(3), AspectRatio::Ar16x9);
        assert_eq!(DisplayMode::from_bits(0), DisplayMode::Normal);
        assert_eq!(DisplayMode::from_bits(1), DisplayMode::PanScan);
        assert_eq!(DisplayMode::from_bits(2), DisplayMode::Letterbox);
        assert_eq!(DisplayMode::from_bits(3), DisplayMode::Reserved);
    }

    #[test]
    fn audio_capabilities_cannot_play_when_zero() {
        let r = RegisterFile::new();
        let c = r.audio_capabilities();
        assert!(c.cannot_play());
        assert!(!c.dolby);
        assert!(!c.dts);
        assert!(!c.mpeg);
    }

    #[test]
    fn audio_capabilities_decodes_each_documented_bit() {
        let mut r = RegisterFile::new();
        let bits = (1 << 2)   // sdds_karaoke
            | (1 << 3)        // dts_karaoke
            | (1 << 4)        // mpeg_karaoke
            | (1 << 6)        // dolby_karaoke
            | (1 << 7)        // pcm_karaoke
            | (1 << 10)       // sdds
            | (1 << 11)       // dts
            | (1 << 12)       // mpeg
            | (1 << 14); // dolby
        r.set_sprm(SPRM_AUDIO_CAPS, bits);
        let c = r.audio_capabilities();
        assert!(c.sdds_karaoke);
        assert!(c.dts_karaoke);
        assert!(c.mpeg_karaoke);
        assert!(c.dolby_karaoke);
        assert!(c.pcm_karaoke);
        assert!(c.sdds);
        assert!(c.dts);
        assert!(c.mpeg);
        assert!(c.dolby);
        assert!(!c.cannot_play());
    }

    #[test]
    fn region_mask_default_is_all_disallowed() {
        let r = RegisterFile::new();
        for region in 1..=8 {
            assert!(!r.region_allowed(region));
        }
        // Out-of-range queries always return false.
        assert!(!r.region_allowed(0));
        assert!(!r.region_allowed(9));
    }

    #[test]
    fn region_mask_per_region_decode() {
        let mut r = RegisterFile::new();
        // Allow regions 1, 2, 4, 8 (bit positions 0, 1, 3, 7).
        let mask = (1u16 << 0) | (1 << 1) | (1 << 3) | (1 << 7);
        r.set_sprm(SPRM_REGION_MASK, mask);
        assert!(r.region_allowed(1));
        assert!(r.region_allowed(2));
        assert!(!r.region_allowed(3));
        assert!(r.region_allowed(4));
        assert!(!r.region_allowed(5));
        assert!(!r.region_allowed(6));
        assert!(!r.region_allowed(7));
        assert!(r.region_allowed(8));
        assert_eq!(r.region_mask(), mask as u8);
    }

    // -----------------------------------------------------------------
    // SPRM 0 / 12 / 16 / 18 — ISO 639 / ISO 3166 language codes.
    // -----------------------------------------------------------------

    #[test]
    fn menu_language_default_is_uninitialised() {
        let r = RegisterFile::new();
        // SPRM 0 default ("player specific") = 0 in our table.
        let lc = r.menu_language();
        assert!(!lc.is_not_specified());
        assert_eq!(lc.ascii_bytes(), None);
    }

    #[test]
    fn menu_language_encodes_ascii_pair() {
        let mut r = RegisterFile::new();
        // "en" — high byte 'e' = 0x65, low byte 'n' = 0x6E.
        r.set_sprm(SPRM_MENU_LANG, 0x656E);
        let lc = r.menu_language();
        assert_eq!(lc.ascii_bytes(), Some([b'e', b'n']));
        assert_eq!(lc.as_string().as_deref(), Some("en"));
        assert_eq!(lc.raw, 0x656E);
    }

    #[test]
    fn preferred_audio_language_default_is_not_specified() {
        let r = RegisterFile::new();
        let lc = r.preferred_audio_language();
        assert!(lc.is_not_specified());
        assert_eq!(lc.ascii_bytes(), None);
        assert_eq!(lc.as_string(), None);
        assert_eq!(lc.raw, LanguageCode::NOT_SPECIFIED);
    }

    #[test]
    fn preferred_subpicture_language_round_trips_uppercase_ascii() {
        let mut r = RegisterFile::new();
        // "JA" — uppercase Japanese, as a DVD might emit. The accessor
        // lowercases on `as_string` but preserves the bytes in
        // `ascii_bytes`.
        r.set_sprm(SPRM_PREF_SUBP_LANG, 0x4A41);
        let lc = r.preferred_subpicture_language();
        assert_eq!(lc.ascii_bytes(), Some([b'J', b'A']));
        assert_eq!(lc.as_string().as_deref(), Some("ja"));
    }

    #[test]
    fn parental_country_garbage_collapses_to_none() {
        let mut r = RegisterFile::new();
        // Non-letter bytes — `ascii_bytes` rejects.
        r.set_sprm(SPRM_CC_PLT, 0x3030); // "00"
        let lc = r.parental_country();
        assert_eq!(lc.ascii_bytes(), None);
        assert_eq!(lc.as_string(), None);
        // But the raw is preserved for round-trip.
        assert_eq!(lc.raw, 0x3030);
    }

    // -----------------------------------------------------------------
    // SPRM 1 (audio stream selector).
    // -----------------------------------------------------------------

    #[test]
    fn audio_stream_default_is_none_sentinel() {
        let r = RegisterFile::new();
        let s = r.audio_stream();
        assert_eq!(s, AudioStreamSelector::None);
        assert!(s.is_none_sentinel());
        assert!(!s.is_stream());
    }

    #[test]
    fn audio_stream_concrete_index() {
        let mut r = RegisterFile::new();
        for i in 0..=7u16 {
            r.set_sprm(SPRM_AUDIO_STREAM, i);
            let s = r.audio_stream();
            assert_eq!(s, AudioStreamSelector::Stream(i as u8));
            assert!(s.is_stream());
            assert!(!s.is_none_sentinel());
        }
    }

    #[test]
    fn audio_stream_invalid_preserves_raw() {
        let mut r = RegisterFile::new();
        r.set_sprm(SPRM_AUDIO_STREAM, 8);
        assert_eq!(r.audio_stream(), AudioStreamSelector::Invalid(8));
        r.set_sprm(SPRM_AUDIO_STREAM, 99);
        assert_eq!(r.audio_stream(), AudioStreamSelector::Invalid(99));
    }

    // -----------------------------------------------------------------
    // SPRM 3 angle.
    // -----------------------------------------------------------------

    #[test]
    fn angle_default_is_one() {
        let r = RegisterFile::new();
        assert_eq!(r.angle_number(), Some(1));
    }

    #[test]
    fn angle_in_range_round_trip() {
        let mut r = RegisterFile::new();
        for i in 1..=9u16 {
            r.set_sprm(SPRM_ANGLE, i);
            assert_eq!(r.angle_number(), Some(i as u8));
        }
    }

    #[test]
    fn angle_out_of_range_is_none() {
        let mut r = RegisterFile::new();
        r.set_sprm(SPRM_ANGLE, 0);
        assert_eq!(r.angle_number(), None);
        r.set_sprm(SPRM_ANGLE, 10);
        assert_eq!(r.angle_number(), None);
        r.set_sprm(SPRM_ANGLE, 0xFFFF);
        assert_eq!(r.angle_number(), None);
    }

    // -----------------------------------------------------------------
    // SPRM 13 parental level.
    // -----------------------------------------------------------------

    #[test]
    fn parental_level_default_is_uninitialised_zero() {
        let r = RegisterFile::new();
        // SPRM 13 default ("player specific") = 0 → Invalid(0).
        assert_eq!(r.parental_level(), ParentalLevel::Invalid(0));
        assert!(!r.parental_level().is_level());
        assert!(!r.parental_level().is_none_sentinel());
    }

    #[test]
    fn parental_level_in_range() {
        let mut r = RegisterFile::new();
        for i in 1..=8u16 {
            r.set_sprm(SPRM_PARENTAL_LEVEL, i);
            assert_eq!(r.parental_level(), ParentalLevel::Level(i as u8));
            assert!(r.parental_level().is_level());
        }
    }

    #[test]
    fn parental_level_none_sentinel() {
        let mut r = RegisterFile::new();
        r.set_sprm(SPRM_PARENTAL_LEVEL, 15);
        assert_eq!(r.parental_level(), ParentalLevel::None);
        assert!(r.parental_level().is_none_sentinel());
    }

    // -----------------------------------------------------------------
    // SPRM 17 / 19 — audio + subpicture language extensions.
    // -----------------------------------------------------------------

    #[test]
    fn audio_language_ext_decode_table() {
        let mut r = RegisterFile::new();
        assert_eq!(
            r.preferred_audio_language_ext(),
            AudioLanguageExt::NotSpecified
        );
        for (raw, expected) in [
            (1, AudioLanguageExt::Normal),
            (2, AudioLanguageExt::VisuallyImpaired),
            (3, AudioLanguageExt::DirectorComments),
            (4, AudioLanguageExt::AlternateDirectorComments),
            (5, AudioLanguageExt::Reserved(5)),
            (250, AudioLanguageExt::Reserved(250)),
        ] {
            r.set_sprm(SPRM_PREF_AUDIO_LANG_EXT, raw);
            assert_eq!(r.preferred_audio_language_ext(), expected);
        }
    }

    #[test]
    fn subpicture_language_ext_decode_table() {
        let mut r = RegisterFile::new();
        assert_eq!(
            r.preferred_subpicture_language_ext(),
            SubpictureLanguageExt::NotSpecified
        );
        for (raw, expected) in [
            (1, SubpictureLanguageExt::Normal),
            (2, SubpictureLanguageExt::Large),
            (3, SubpictureLanguageExt::Children),
            (5, SubpictureLanguageExt::NormalCaptions),
            (6, SubpictureLanguageExt::LargeCaptions),
            (7, SubpictureLanguageExt::ChildrensCaptions),
            (9, SubpictureLanguageExt::Forced),
            (13, SubpictureLanguageExt::DirectorComments),
            (14, SubpictureLanguageExt::LargeDirectorComments),
            (15, SubpictureLanguageExt::DirectorCommentsForChildren),
            // Gaps in the spec table — 4 / 8 / 10 / 11 / 12 — collapse
            // to `Reserved`.
            (4, SubpictureLanguageExt::Reserved(4)),
            (8, SubpictureLanguageExt::Reserved(8)),
            (10, SubpictureLanguageExt::Reserved(10)),
            (11, SubpictureLanguageExt::Reserved(11)),
            (12, SubpictureLanguageExt::Reserved(12)),
            (200, SubpictureLanguageExt::Reserved(200)),
        ] {
            r.set_sprm(SPRM_PREF_SUBP_LANG_EXT, raw);
            assert_eq!(r.preferred_subpicture_language_ext(), expected);
        }
    }
}
