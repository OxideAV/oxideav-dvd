//! DVD-Video **User Operation flags** (UOPs).
//!
//! Three on-disc fields carry a UOP-prohibition bitmask:
//!
//! - `TT_SRPT` per-title: only bits `0` (`TimePlayOrSearch`) and `1`
//!   (`PttPlayOrSearch`) — packed into the lower two bits of the
//!   `title_type` byte (see [`DvdTitleEntry::title_type`] on
//!   `mpucoder-ifo_vmg.html`).
//! - `PGC` per-program-chain: the full 25-bit map at PGC offset
//!   `0x0008` ([`Pgc::prohibited_user_ops`]).
//! - `PCI` per-VOBU: the full 25-bit map at `PCI_GI 08`
//!   ([`PciPacket::vobu_uop_ctl`]).
//!
//! The three masks are **OR-ed together** per
//! `mpucoder-uops.html`: a set bit in *any* mask inhibits the
//! associated control. This module surfaces:
//!
//! - [`UserOp`] — a typed enum over the 25 spec-named operations
//!   (`TimePlayOrSearch` .. `VideoPresentationModeChange`).
//! - [`UopMask`] — a newtype around a `u32` bitmask with typed
//!   `contains` / `set` / `clear` / `is_allowed` accessors, a
//!   `merge_or` constructor that implements the three-level OR,
//!   plus `iter` over the currently-prohibited ops.
//! - [`title_type_uop_mask`] / [`UopLevel`] helpers — extract the
//!   2-bit `TT_SRPT` subset, identify which level a field belongs
//!   to, and surface the "which UOPs can a level even carry" cover
//!   set (TT_SRPT carries bits 0+1; PGC carries every bit; VOBU
//!   carries all except 0 / 1 / 2 / 17 per the spec table's blank
//!   `VOBU` column entries).
//!
//! Clean-room per:
//!
//! - `docs/container/dvd/application/mpucoder-uops.html` — 25-entry
//!   bit table, three-level OR-merge rule, per-level applicability.
//! - `docs/container/dvd/application/mpucoder-ifo_vmg.html` — TT_SRPT
//!   entry layout (UOP0/UOP1 packed in `title_type`).
//! - `docs/container/dvd/application/mpucoder-pgc.html` — PGC
//!   `prohibited_user_ops` offset.
//! - `docs/container/dvd/application/mpucoder-pci_pkt.html` — PCI
//!   `vobu_uop_ctl` offset.

use core::fmt;

// =====================================================================
// Bit numbers (per mpucoder-uops.html).
// =====================================================================

/// Bit `0` — Time play or search.
pub const UOP_TIME_PLAY_OR_SEARCH: u8 = 0;
/// Bit `1` — PTT (Part-of-Title) play or search.
pub const UOP_PTT_PLAY_OR_SEARCH: u8 = 1;
/// Bit `2` — Title play.
pub const UOP_TITLE_PLAY: u8 = 2;
/// Bit `3` — Stop.
pub const UOP_STOP: u8 = 3;
/// Bit `4` — GoUp.
pub const UOP_GO_UP: u8 = 4;
/// Bit `5` — Time or PTT search.
pub const UOP_TIME_OR_PTT_SEARCH: u8 = 5;
/// Bit `6` — TopPG or PrevPG search.
pub const UOP_TOP_PG_OR_PREV_PG_SEARCH: u8 = 6;
/// Bit `7` — NextPG search.
pub const UOP_NEXT_PG_SEARCH: u8 = 7;
/// Bit `8` — Forward scan.
pub const UOP_FORWARD_SCAN: u8 = 8;
/// Bit `9` — Backward scan.
pub const UOP_BACKWARD_SCAN: u8 = 9;
/// Bit `10` — Menu call (Title).
pub const UOP_MENU_CALL_TITLE: u8 = 10;
/// Bit `11` — Menu call (Root).
pub const UOP_MENU_CALL_ROOT: u8 = 11;
/// Bit `12` — Menu call (Subpicture).
pub const UOP_MENU_CALL_SUBPICTURE: u8 = 12;
/// Bit `13` — Menu call (Audio).
pub const UOP_MENU_CALL_AUDIO: u8 = 13;
/// Bit `14` — Menu call (Angle).
pub const UOP_MENU_CALL_ANGLE: u8 = 14;
/// Bit `15` — Menu call (PTT).
pub const UOP_MENU_CALL_PTT: u8 = 15;
/// Bit `16` — Resume.
pub const UOP_RESUME: u8 = 16;
/// Bit `17` — Button select or activate.
pub const UOP_BUTTON_SELECT_OR_ACTIVATE: u8 = 17;
/// Bit `18` — Still off.
pub const UOP_STILL_OFF: u8 = 18;
/// Bit `19` — Pause on.
pub const UOP_PAUSE_ON: u8 = 19;
/// Bit `20` — Audio stream change.
pub const UOP_AUDIO_STREAM_CHANGE: u8 = 20;
/// Bit `21` — Subpicture stream change.
pub const UOP_SUBPICTURE_STREAM_CHANGE: u8 = 21;
/// Bit `22` — Angle change.
pub const UOP_ANGLE_CHANGE: u8 = 22;
/// Bit `23` — Karaoke audio mix change.
pub const UOP_KARAOKE_AUDIO_MIX_CHANGE: u8 = 23;
/// Bit `24` — Video presentation mode change.
pub const UOP_VIDEO_PRESENTATION_MODE_CHANGE: u8 = 24;

/// Total number of defined UOP bit positions per the spec table.
pub const UOP_BIT_COUNT: u8 = 25;

/// Mask covering every defined UOP bit (bits 0..=24). Bits 25..=31
/// of the on-disc word are reserved and ignored by `UopMask`.
pub const UOP_DEFINED_BITS: u32 = (1u32 << UOP_BIT_COUNT) - 1;

// =====================================================================
// UserOp — typed UOP variant.
// =====================================================================

/// One of the 25 prohibitable user operations.
///
/// The discriminant matches the spec bit number, so casting to `u32`
/// and `1 << x` recovers the raw bitmask value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum UserOp {
    /// Bit `0`.
    TimePlayOrSearch = UOP_TIME_PLAY_OR_SEARCH,
    /// Bit `1`.
    PttPlayOrSearch = UOP_PTT_PLAY_OR_SEARCH,
    /// Bit `2`.
    TitlePlay = UOP_TITLE_PLAY,
    /// Bit `3`.
    Stop = UOP_STOP,
    /// Bit `4`.
    GoUp = UOP_GO_UP,
    /// Bit `5`.
    TimeOrPttSearch = UOP_TIME_OR_PTT_SEARCH,
    /// Bit `6`.
    TopPgOrPrevPgSearch = UOP_TOP_PG_OR_PREV_PG_SEARCH,
    /// Bit `7`.
    NextPgSearch = UOP_NEXT_PG_SEARCH,
    /// Bit `8`.
    ForwardScan = UOP_FORWARD_SCAN,
    /// Bit `9`.
    BackwardScan = UOP_BACKWARD_SCAN,
    /// Bit `10`.
    MenuCallTitle = UOP_MENU_CALL_TITLE,
    /// Bit `11`.
    MenuCallRoot = UOP_MENU_CALL_ROOT,
    /// Bit `12`.
    MenuCallSubpicture = UOP_MENU_CALL_SUBPICTURE,
    /// Bit `13`.
    MenuCallAudio = UOP_MENU_CALL_AUDIO,
    /// Bit `14`.
    MenuCallAngle = UOP_MENU_CALL_ANGLE,
    /// Bit `15`.
    MenuCallPtt = UOP_MENU_CALL_PTT,
    /// Bit `16`.
    Resume = UOP_RESUME,
    /// Bit `17`.
    ButtonSelectOrActivate = UOP_BUTTON_SELECT_OR_ACTIVATE,
    /// Bit `18`.
    StillOff = UOP_STILL_OFF,
    /// Bit `19`.
    PauseOn = UOP_PAUSE_ON,
    /// Bit `20`.
    AudioStreamChange = UOP_AUDIO_STREAM_CHANGE,
    /// Bit `21`.
    SubpictureStreamChange = UOP_SUBPICTURE_STREAM_CHANGE,
    /// Bit `22`.
    AngleChange = UOP_ANGLE_CHANGE,
    /// Bit `23`.
    KaraokeAudioMixChange = UOP_KARAOKE_AUDIO_MIX_CHANGE,
    /// Bit `24`.
    VideoPresentationModeChange = UOP_VIDEO_PRESENTATION_MODE_CHANGE,
}

impl UserOp {
    /// Spec bit number (0..=24).
    #[inline]
    pub const fn bit(self) -> u8 {
        self as u8
    }

    /// Single-bit mask value (`1 << bit`).
    #[inline]
    pub const fn mask(self) -> u32 {
        1u32 << (self as u8)
    }

    /// All 25 variants in spec order. Useful for `iter()`-style
    /// scans and table-driven tests.
    pub const ALL: [UserOp; 25] = [
        UserOp::TimePlayOrSearch,
        UserOp::PttPlayOrSearch,
        UserOp::TitlePlay,
        UserOp::Stop,
        UserOp::GoUp,
        UserOp::TimeOrPttSearch,
        UserOp::TopPgOrPrevPgSearch,
        UserOp::NextPgSearch,
        UserOp::ForwardScan,
        UserOp::BackwardScan,
        UserOp::MenuCallTitle,
        UserOp::MenuCallRoot,
        UserOp::MenuCallSubpicture,
        UserOp::MenuCallAudio,
        UserOp::MenuCallAngle,
        UserOp::MenuCallPtt,
        UserOp::Resume,
        UserOp::ButtonSelectOrActivate,
        UserOp::StillOff,
        UserOp::PauseOn,
        UserOp::AudioStreamChange,
        UserOp::SubpictureStreamChange,
        UserOp::AngleChange,
        UserOp::KaraokeAudioMixChange,
        UserOp::VideoPresentationModeChange,
    ];

    /// Recover the typed op from a raw bit number, returning
    /// `None` for `bit >= 25`.
    pub const fn from_bit(bit: u8) -> Option<UserOp> {
        match bit {
            0 => Some(UserOp::TimePlayOrSearch),
            1 => Some(UserOp::PttPlayOrSearch),
            2 => Some(UserOp::TitlePlay),
            3 => Some(UserOp::Stop),
            4 => Some(UserOp::GoUp),
            5 => Some(UserOp::TimeOrPttSearch),
            6 => Some(UserOp::TopPgOrPrevPgSearch),
            7 => Some(UserOp::NextPgSearch),
            8 => Some(UserOp::ForwardScan),
            9 => Some(UserOp::BackwardScan),
            10 => Some(UserOp::MenuCallTitle),
            11 => Some(UserOp::MenuCallRoot),
            12 => Some(UserOp::MenuCallSubpicture),
            13 => Some(UserOp::MenuCallAudio),
            14 => Some(UserOp::MenuCallAngle),
            15 => Some(UserOp::MenuCallPtt),
            16 => Some(UserOp::Resume),
            17 => Some(UserOp::ButtonSelectOrActivate),
            18 => Some(UserOp::StillOff),
            19 => Some(UserOp::PauseOn),
            20 => Some(UserOp::AudioStreamChange),
            21 => Some(UserOp::SubpictureStreamChange),
            22 => Some(UserOp::AngleChange),
            23 => Some(UserOp::KaraokeAudioMixChange),
            24 => Some(UserOp::VideoPresentationModeChange),
            _ => None,
        }
    }

    /// `true` when this op may legally appear in the level's mask.
    /// The spec table marks each row with check-marks in the `PGC`
    /// and `VOBU` columns; `TT_SRPT` only carries bits 0 and 1.
    pub const fn applies_to(self, level: UopLevel) -> bool {
        match level {
            UopLevel::TitleSearchPointer => {
                matches!(self, UserOp::TimePlayOrSearch | UserOp::PttPlayOrSearch)
            }
            // The PGC column carries a check-mark on every row
            // except row 4 (GoUp) per the spec table.
            UopLevel::ProgramChain => !matches!(self, UserOp::GoUp),
            // The VOBU column is blank for bits 0, 1, 2, and 17 per
            // the spec table; every other row carries a check-mark.
            UopLevel::Vobu => !matches!(
                self,
                UserOp::TimePlayOrSearch
                    | UserOp::PttPlayOrSearch
                    | UserOp::TitlePlay
                    | UserOp::ButtonSelectOrActivate
            ),
        }
    }
}

// =====================================================================
// UopLevel — which of the three masks a field belongs to.
// =====================================================================

/// The three on-disc levels at which a UOP mask appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UopLevel {
    /// Per-title `TT_SRPT` entry (bits 0+1 packed in `title_type`).
    TitleSearchPointer,
    /// Per-PGC `prohibited_user_ops` (PGC offset `0x0008`).
    ProgramChain,
    /// Per-VOBU PCI `vobu_uop_ctl` (`PCI_GI 08`).
    Vobu,
}

impl UopLevel {
    /// Bit-cover for the level — `1` bits at every UOP that may
    /// legally appear in this mask field.
    pub const fn cover(self) -> u32 {
        match self {
            UopLevel::TitleSearchPointer => {
                (1u32 << UOP_TIME_PLAY_OR_SEARCH) | (1u32 << UOP_PTT_PLAY_OR_SEARCH)
            }
            // PGC carries every defined bit except `GoUp` per the
            // spec table's row 4 PGC-column blank.
            UopLevel::ProgramChain => UOP_DEFINED_BITS & !(1u32 << UOP_GO_UP),
            UopLevel::Vobu => {
                UOP_DEFINED_BITS
                    & !((1u32 << UOP_TIME_PLAY_OR_SEARCH)
                        | (1u32 << UOP_PTT_PLAY_OR_SEARCH)
                        | (1u32 << UOP_TITLE_PLAY)
                        | (1u32 << UOP_BUTTON_SELECT_OR_ACTIVATE))
            }
        }
    }
}

// =====================================================================
// UopMask — typed wrapper around the raw u32 bitmask.
// =====================================================================

/// Typed view over a UOP-prohibition word.
///
/// A `1` bit *prohibits* the corresponding operation — that mirrors
/// the on-disc convention. [`is_allowed`](Self::is_allowed) returns
/// `true` when the bit is clear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct UopMask {
    bits: u32,
}

impl UopMask {
    /// All operations allowed (no bits set).
    pub const NONE: UopMask = UopMask { bits: 0 };

    /// All 25 defined operations prohibited (`bits 0..=24` set).
    pub const ALL: UopMask = UopMask {
        bits: UOP_DEFINED_BITS,
    };

    /// Wrap a raw on-disc word. Bits above `24` are kept as-is in
    /// [`Self::raw`] but ignored by every typed accessor.
    #[inline]
    pub const fn from_bits(bits: u32) -> Self {
        Self { bits }
    }

    /// Raw 32-bit word (round-trips the on-disc value exactly).
    #[inline]
    pub const fn raw(self) -> u32 {
        self.bits
    }

    /// Defined-bit subset — the raw word masked to bits `0..=24`.
    #[inline]
    pub const fn defined_bits(self) -> u32 {
        self.bits & UOP_DEFINED_BITS
    }

    /// `true` when `op`'s bit is set (operation is prohibited).
    #[inline]
    pub const fn contains(self, op: UserOp) -> bool {
        (self.bits & op.mask()) != 0
    }

    /// `true` when `op`'s bit is *clear* (operation is allowed).
    #[inline]
    pub const fn is_allowed(self, op: UserOp) -> bool {
        !self.contains(op)
    }

    /// Set `op`'s bit (prohibit the operation).
    #[inline]
    pub const fn with(self, op: UserOp) -> Self {
        Self {
            bits: self.bits | op.mask(),
        }
    }

    /// Clear `op`'s bit (re-allow the operation).
    #[inline]
    pub const fn without(self, op: UserOp) -> Self {
        Self {
            bits: self.bits & !op.mask(),
        }
    }

    /// In-place set.
    #[inline]
    pub fn set(&mut self, op: UserOp) {
        self.bits |= op.mask();
    }

    /// In-place clear.
    #[inline]
    pub fn clear(&mut self, op: UserOp) {
        self.bits &= !op.mask();
    }

    /// `true` when no UOP bits are set at all (all operations
    /// allowed). Reserved bits above `24` are ignored.
    #[inline]
    pub const fn is_empty(self) -> bool {
        (self.bits & UOP_DEFINED_BITS) == 0
    }

    /// Per-spec three-level merge — a set bit in *any* mask
    /// inhibits the associated control, so the merge is a plain OR.
    /// Pass `UopMask::NONE` for any level the caller hasn't loaded
    /// yet (the OR identity).
    #[inline]
    pub const fn merge_or(a: UopMask, b: UopMask, c: UopMask) -> UopMask {
        UopMask {
            bits: a.bits | b.bits | c.bits,
        }
    }

    /// Bit count of currently-prohibited defined ops. Reserved bits
    /// above `24` don't contribute.
    #[inline]
    pub const fn count(self) -> u32 {
        (self.bits & UOP_DEFINED_BITS).count_ones()
    }

    /// `true` when every set bit in this mask is one this `level`
    /// is allowed to carry per the spec table — useful for an IFO
    /// validator. `false` when the mask carries a bit that the spec
    /// leaves blank for that level.
    #[inline]
    pub const fn fits_level(self, level: UopLevel) -> bool {
        (self.defined_bits() & !level.cover()) == 0
    }

    /// Iterator over the set [`UserOp`] bits in ascending bit order.
    /// Reserved bits above `24` are skipped.
    pub fn iter(self) -> UopIter {
        UopIter {
            remaining: self.defined_bits(),
        }
    }
}

impl fmt::Display for UopMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UopMask(0x{:08X})", self.bits)
    }
}

impl From<u32> for UopMask {
    #[inline]
    fn from(bits: u32) -> UopMask {
        UopMask { bits }
    }
}

impl From<UopMask> for u32 {
    #[inline]
    fn from(m: UopMask) -> u32 {
        m.bits
    }
}

/// Iterator over the [`UserOp`] bits set in a [`UopMask`].
#[derive(Debug, Clone)]
pub struct UopIter {
    remaining: u32,
}

impl Iterator for UopIter {
    type Item = UserOp;

    fn next(&mut self) -> Option<UserOp> {
        if self.remaining == 0 {
            return None;
        }
        let bit = self.remaining.trailing_zeros() as u8;
        self.remaining &= !(1u32 << bit);
        UserOp::from_bit(bit)
    }
}

// =====================================================================
// TT_SRPT helper — bits 0+1 of `title_type`.
// =====================================================================

/// Extract the 2-bit UOP-prohibition subset from a `TT_SRPT`
/// `title_type` byte.
///
/// Per `mpucoder-uops.html`, `TT_SRPT` only carries UOP-0
/// (`TimePlayOrSearch`) and UOP-1 (`PttPlayOrSearch`); they live in
/// the two low bits of `title_type`. The remaining `title_type`
/// bits encode the title's jump/link/call-permission and angle
/// flags per `mpucoder-ifo_vmg.html` and stay outside the UOP
/// surface.
#[inline]
pub const fn title_type_uop_mask(title_type: u8) -> UopMask {
    UopMask::from_bits((title_type & 0b0000_0011) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_numbers_match_spec_table_row_order() {
        // Spec-table numbers 0..=24 map straight onto the enum's
        // discriminants; no shuffling.
        for (i, op) in UserOp::ALL.iter().enumerate() {
            assert_eq!(op.bit() as usize, i);
            assert_eq!(op.mask(), 1u32 << i);
            assert_eq!(UserOp::from_bit(i as u8), Some(*op));
        }
        // 25 ops, no more.
        assert_eq!(UserOp::ALL.len(), UOP_BIT_COUNT as usize);
        assert_eq!(UserOp::from_bit(25), None);
        assert_eq!(UserOp::from_bit(31), None);
    }

    #[test]
    fn defined_bits_cover_lower_25_only() {
        assert_eq!(UOP_DEFINED_BITS, 0x01FF_FFFF);
        assert_eq!(UOP_DEFINED_BITS.count_ones(), 25);
        // Bit 25 not part of the spec table.
        assert_eq!(UOP_DEFINED_BITS & (1 << 25), 0);
    }

    #[test]
    fn from_bits_round_trips_raw_word() {
        let raw = 0xDEAD_BEEFu32;
        let m = UopMask::from_bits(raw);
        assert_eq!(m.raw(), raw);
        // Reserved bits stay in `raw` but are masked from
        // `defined_bits`.
        assert_eq!(m.defined_bits(), raw & UOP_DEFINED_BITS);
    }

    #[test]
    fn contains_and_is_allowed_invert() {
        let m = UopMask::NONE.with(UserOp::Stop).with(UserOp::PauseOn);
        assert!(m.contains(UserOp::Stop));
        assert!(m.contains(UserOp::PauseOn));
        assert!(!m.contains(UserOp::Resume));
        assert!(!m.is_allowed(UserOp::Stop));
        assert!(m.is_allowed(UserOp::Resume));
    }

    #[test]
    fn with_without_set_clear_match() {
        let m1 = UopMask::NONE.with(UserOp::ForwardScan);
        let mut m2 = UopMask::NONE;
        m2.set(UserOp::ForwardScan);
        assert_eq!(m1, m2);

        let m3 = m1.without(UserOp::ForwardScan);
        let mut m4 = m2;
        m4.clear(UserOp::ForwardScan);
        assert_eq!(m3, m4);
        assert!(m3.is_empty());
    }

    #[test]
    fn is_empty_ignores_reserved_bits() {
        // Reserved bits 25..=31 don't count as prohibitions.
        let m = UopMask::from_bits(0xFE00_0000);
        assert!(m.is_empty());
        assert_eq!(m.count(), 0);
    }

    #[test]
    fn merge_or_is_plain_bitwise_or() {
        let tt = title_type_uop_mask(0b11); // TT_SRPT bits 0+1 set
        let pgc = UopMask::NONE.with(UserOp::Stop).with(UserOp::ForwardScan);
        let vobu = UopMask::NONE.with(UserOp::MenuCallRoot);

        let merged = UopMask::merge_or(tt, pgc, vobu);
        for op in [
            UserOp::TimePlayOrSearch,
            UserOp::PttPlayOrSearch,
            UserOp::Stop,
            UserOp::ForwardScan,
            UserOp::MenuCallRoot,
        ] {
            assert!(merged.contains(op), "merged should contain {:?}", op);
        }
        // Unrelated ops stay allowed.
        assert!(merged.is_allowed(UserOp::Resume));
        assert!(merged.is_allowed(UserOp::AngleChange));
    }

    #[test]
    fn merge_or_is_orthogonal_to_argument_position() {
        let a = UopMask::NONE.with(UserOp::Stop);
        let b = UopMask::NONE.with(UserOp::Resume);
        let c = UopMask::NONE.with(UserOp::PauseOn);
        let one = UopMask::merge_or(a, b, c);
        let two = UopMask::merge_or(c, a, b);
        let three = UopMask::merge_or(b, c, a);
        assert_eq!(one, two);
        assert_eq!(two, three);
    }

    #[test]
    fn iter_walks_bits_in_ascending_order() {
        let m = UopMask::NONE
            .with(UserOp::VideoPresentationModeChange)
            .with(UserOp::Stop)
            .with(UserOp::ForwardScan);
        let collected: Vec<UserOp> = m.iter().collect();
        assert_eq!(
            collected,
            vec![
                UserOp::Stop,
                UserOp::ForwardScan,
                UserOp::VideoPresentationModeChange,
            ]
        );
    }

    #[test]
    fn iter_skips_reserved_bits() {
        // Bit 25 set in the raw word; iter ignores it.
        let m = UopMask::from_bits((1u32 << 25) | (1u32 << UOP_STOP as usize));
        let collected: Vec<UserOp> = m.iter().collect();
        assert_eq!(collected, vec![UserOp::Stop]);
    }

    #[test]
    fn iter_over_all_yields_every_userop_in_order() {
        let all: Vec<UserOp> = UopMask::ALL.iter().collect();
        assert_eq!(all.len(), 25);
        for (i, op) in all.iter().enumerate() {
            assert_eq!(op.bit() as usize, i);
        }
    }

    #[test]
    fn count_matches_iter_length() {
        for raw in [0u32, 0x1, 0x100_0001, UOP_DEFINED_BITS, 0xFFFF_FFFF] {
            let m = UopMask::from_bits(raw);
            assert_eq!(m.count() as usize, m.iter().count());
        }
    }

    #[test]
    fn title_type_helper_extracts_low_two_bits_only() {
        // High bits (jump/link/call permission flags etc.) ignored.
        // `0b1010_1011` → low bits `11` → both UOPs prohibited.
        let m = title_type_uop_mask(0b1010_1011);
        assert!(m.contains(UserOp::TimePlayOrSearch));
        assert!(m.contains(UserOp::PttPlayOrSearch));
        // `0b1111_1100` → low bits `00` → empty mask.
        let m = title_type_uop_mask(0b1111_1100);
        assert!(m.is_empty());
        // `0b1111_1101` → low bits `01` → only bit 0 set.
        let m = title_type_uop_mask(0b1111_1101);
        assert!(m.contains(UserOp::TimePlayOrSearch));
        assert!(!m.contains(UserOp::PttPlayOrSearch));
        // `0b1111_1110` → low bits `10` → only bit 1 set.
        let m = title_type_uop_mask(0b1111_1110);
        assert!(!m.contains(UserOp::TimePlayOrSearch));
        assert!(m.contains(UserOp::PttPlayOrSearch));
    }

    #[test]
    fn title_type_helper_never_carries_bits_above_one() {
        for tt in 0u8..=255 {
            let m = title_type_uop_mask(tt);
            assert_eq!(m.raw() & !0b11u32, 0);
            assert!(m.fits_level(UopLevel::TitleSearchPointer));
        }
    }

    #[test]
    fn level_cover_matches_spec_table_columns() {
        // TT_SRPT: only bits 0 + 1.
        let tt = UopLevel::TitleSearchPointer.cover();
        assert_eq!(tt, 0b11);
        // PGC: every defined bit except GoUp (row 4 PGC-column
        // blank).
        let pgc = UopLevel::ProgramChain.cover();
        assert_eq!(pgc, UOP_DEFINED_BITS & !(1u32 << UOP_GO_UP));
        assert_eq!(pgc.count_ones(), 24);
        // VOBU: every defined bit EXCEPT 0/1/2/17.
        let vobu = UopLevel::Vobu.cover();
        let blanks = (1u32 << UOP_TIME_PLAY_OR_SEARCH)
            | (1u32 << UOP_PTT_PLAY_OR_SEARCH)
            | (1u32 << UOP_TITLE_PLAY)
            | (1u32 << UOP_BUTTON_SELECT_OR_ACTIVATE);
        assert_eq!(vobu, UOP_DEFINED_BITS & !blanks);
        assert_eq!(vobu.count_ones(), 21);
    }

    #[test]
    fn user_op_applies_to_matches_level_cover_bit_for_bit() {
        for op in UserOp::ALL {
            for level in [
                UopLevel::TitleSearchPointer,
                UopLevel::ProgramChain,
                UopLevel::Vobu,
            ] {
                let from_level = (level.cover() & op.mask()) != 0;
                assert_eq!(op.applies_to(level), from_level, "{:?} vs {:?}", op, level);
            }
        }
    }

    #[test]
    fn fits_level_rejects_out_of_table_bits() {
        // A mask with every defined bit set carries bit 4 (`GoUp`),
        // which the PGC column is blank on per the spec table — so
        // it fails `fits_level(ProgramChain)`.
        let every_defined = UopMask::ALL;
        assert!(!every_defined.fits_level(UopLevel::ProgramChain));
        // Same mask: blanks at 0/1/2/17 mean it fails Vobu too.
        assert!(!every_defined.fits_level(UopLevel::Vobu));
        // And of course fails TT_SRPT — only bits 0/1 fit.
        assert!(!every_defined.fits_level(UopLevel::TitleSearchPointer));

        // A mask sized to the PGC cover (every defined bit minus
        // GoUp) fits PGC.
        let pgc_full = UopMask::from_bits(UopLevel::ProgramChain.cover());
        assert!(pgc_full.fits_level(UopLevel::ProgramChain));

        // A VOBU-allowed mask (bit 3 + bit 24) fits its own level
        // AND fits the PGC level (neither bit is the PGC-blank
        // GoUp bit), but does not fit TT_SRPT.
        let vobu_ok = UopMask::NONE
            .with(UserOp::Stop)
            .with(UserOp::VideoPresentationModeChange);
        assert!(vobu_ok.fits_level(UopLevel::Vobu));
        assert!(vobu_ok.fits_level(UopLevel::ProgramChain));
        assert!(!vobu_ok.fits_level(UopLevel::TitleSearchPointer));

        // A mask carrying GoUp fits Vobu but not PGC.
        let go_up_only = UopMask::NONE.with(UserOp::GoUp);
        assert!(go_up_only.fits_level(UopLevel::Vobu));
        assert!(!go_up_only.fits_level(UopLevel::ProgramChain));
    }

    #[test]
    fn from_into_round_trip_u32() {
        for raw in [0u32, 0x1, UOP_DEFINED_BITS, 0xFFFF_FFFF] {
            let m: UopMask = raw.into();
            let back: u32 = m.into();
            assert_eq!(raw, back);
        }
    }

    #[test]
    fn display_renders_uppercase_hex() {
        let m = UopMask::from_bits(0x01FF_FFFF);
        let s = format!("{}", m);
        assert_eq!(s, "UopMask(0x01FFFFFF)");
    }

    #[test]
    fn merge_or_is_associative_across_three_levels() {
        // Property: merge_or((merge a b), 0, c) == merge_or(a, b, c).
        let a = UopMask::from_bits(0x0001_5555);
        let b = UopMask::from_bits(0x0000_AAAA);
        let c = UopMask::from_bits(0x0010_0001);
        let direct = UopMask::merge_or(a, b, c);
        let pair_then_c =
            UopMask::merge_or(UopMask::from_bits(a.raw() | b.raw()), UopMask::NONE, c);
        assert_eq!(direct, pair_then_c);
    }

    #[test]
    fn spec_table_check_mark_rows_match_pgc_and_vobu_columns() {
        // Mirrors the spec table exactly: (UserOp, PGC?, VOBU?).
        // PGC has check-marks on every row, VOBU is blank at
        // bits 0 / 1 / 2 / 17, check-marked everywhere else.
        let rows: [(UserOp, bool, bool); 25] = [
            (UserOp::TimePlayOrSearch, true, false),
            (UserOp::PttPlayOrSearch, true, false),
            (UserOp::TitlePlay, true, false),
            (UserOp::Stop, true, true),
            (UserOp::GoUp, false, true),
            (UserOp::TimeOrPttSearch, true, true),
            (UserOp::TopPgOrPrevPgSearch, true, true),
            (UserOp::NextPgSearch, true, true),
            (UserOp::ForwardScan, true, true),
            (UserOp::BackwardScan, true, true),
            (UserOp::MenuCallTitle, true, true),
            (UserOp::MenuCallRoot, true, true),
            (UserOp::MenuCallSubpicture, true, true),
            (UserOp::MenuCallAudio, true, true),
            (UserOp::MenuCallAngle, true, true),
            (UserOp::MenuCallPtt, true, true),
            (UserOp::Resume, true, true),
            (UserOp::ButtonSelectOrActivate, true, false),
            (UserOp::StillOff, true, true),
            (UserOp::PauseOn, true, true),
            (UserOp::AudioStreamChange, true, true),
            (UserOp::SubpictureStreamChange, true, true),
            (UserOp::AngleChange, true, true),
            (UserOp::KaraokeAudioMixChange, true, true),
            (UserOp::VideoPresentationModeChange, true, true),
        ];
        for (op, pgc, vobu) in rows {
            assert_eq!(op.applies_to(UopLevel::ProgramChain), pgc, "PGC {:?}", op);
            assert_eq!(op.applies_to(UopLevel::Vobu), vobu, "VOBU {:?}", op);
        }
    }
}
