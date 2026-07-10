//! Copy Control Information (CCI) — CGMS + APS typed decode.
//!
//! DVD-Video carries two orthogonal analog copy-control fields whose
//! **value encodings** are standardised regardless of which disc
//! field holds them:
//!
//! * **CGMS** — Copy Generation Management System, 2 bits
//!   ([`Cgms`]). Re-emitted on a player's analog output it becomes
//!   "CGMS-A" (the `-A` = analog); the same two bits are carried
//!   digitally on the disc.
//! * **APS** — Analog Protection System, 2 bits ([`ApsType`]).
//!   Selects the AGC-pulse / Colorstripe process applied to the
//!   composite / S-video analog outputs only; it has no effect on
//!   the digital bitstream or component paths.
//!
//! The disc stores/signals CCI at three levels
//! (`dvd-substream-ids-and-copy-protection.md` §3):
//!
//! 1. **Sector level** — the 6-byte `CPR_MAI` field of every
//!    2064-byte Data Frame ([`CprMai`]; ECMA-267 §16.3 declares the
//!    bytes application-dependent, all-ZERO default).
//! 2. **VOBU level** — the PCI `vobu_cat` 16-bit flag field
//!    ([`crate::vob::PciPacket::vobu_cat`]), preserved **raw**: the
//!    exact bit position of the APS trigger inside it is fixed only
//!    by the member-gated DVD Forum *Part 3* book and is not pinned
//!    by any staged public source, so this crate does not invent a
//!    layout for it.
//! 3. **Title / VTS level** — IFO attribute tables (out of scope
//!    here; see `crate::ifo`).
//!
//! ## Clean-room references
//!
//! * `docs/container/dvd/application/dvd-substream-ids-and-copy-protection.md`
//!   §2–§3 — CGMS / APS value tables, analog-output field packing,
//!   `CPR_MAI` byte-0 reconstruction (flagged there as a community
//!   reconstruction, not an ECMA definition).
//! * `docs/container/dvd/physical/ECMA-267_3rd_edition_april_2001.pdf`
//!   §16 — Data Frame layout (ID 4 B + IED 2 B + CPR_MAI 6 B +
//!   2048 Main Data + EDC 4 B) and §16.3 (`CPR_MAI` is 6 bytes,
//!   application-dependent, default all-ZERO).

use crate::error::{Error, Result};

// ------------------------------------------------------------------
// CGMS — Copy Generation Management System (§2a)
// ------------------------------------------------------------------

/// CGMS 2-bit copy-generation state
/// (`dvd-substream-ids-and-copy-protection.md` §2a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cgms {
    /// `00` — unrestricted copying permitted.
    CopyFreely,
    /// `01` — a permitted copy was already made; no further copies.
    CopyNoMore,
    /// `10` — exactly one generation of copies may be made.
    CopyOnce,
    /// `11` — no copying permitted.
    CopyNever,
}

impl Cgms {
    /// Decode from the low two bits of `bits` (higher bits ignored).
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => Self::CopyFreely,
            0b01 => Self::CopyNoMore,
            0b10 => Self::CopyOnce,
            _ => Self::CopyNever,
        }
    }

    /// The 2-bit on-wire encoding.
    pub fn bits(self) -> u8 {
        match self {
            Self::CopyFreely => 0b00,
            Self::CopyNoMore => 0b01,
            Self::CopyOnce => 0b10,
            Self::CopyNever => 0b11,
        }
    }

    /// `true` when at least one further generation of copies is
    /// permitted (`CopyFreely` / `CopyOnce`).
    pub fn copying_permitted(self) -> bool {
        matches!(self, Self::CopyFreely | Self::CopyOnce)
    }
}

// ------------------------------------------------------------------
// APS — Analog Protection System (§2b)
// ------------------------------------------------------------------

/// APS 2-bit analog-protection process selector
/// (`dvd-substream-ids-and-copy-protection.md` §2b). Drives only the
/// composite / S-video analog outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ApsType {
    /// `00` — Type 0: off, no analog protection.
    Off,
    /// `01` — Type 1: automatic-gain-control pulses, Colorstripe off.
    Agc,
    /// `10` — Type 2: AGC + 2-line Colorstripe.
    AgcColorstripe2,
    /// `11` — Type 3: AGC + 4-line Colorstripe.
    AgcColorstripe4,
}

impl ApsType {
    /// Decode from the low two bits of `bits` (higher bits ignored).
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => Self::Off,
            0b01 => Self::Agc,
            0b10 => Self::AgcColorstripe2,
            _ => Self::AgcColorstripe4,
        }
    }

    /// The 2-bit on-wire encoding (the "Type" number).
    pub fn bits(self) -> u8 {
        match self {
            Self::Off => 0b00,
            Self::Agc => 0b01,
            Self::AgcColorstripe2 => 0b10,
            Self::AgcColorstripe4 => 0b11,
        }
    }

    /// `true` when any analog-protection process is applied.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// `true` for the two Colorstripe-adding types (2 and 3).
    pub fn has_colorstripe(self) -> bool {
        matches!(self, Self::AgcColorstripe2 | Self::AgcColorstripe4)
    }
}

// ------------------------------------------------------------------
// Analog-output CCI field (§2c)
// ------------------------------------------------------------------

/// The complete copy-control word a player emits on its analog
/// output (NTSC line-21 data; 50 Hz systems carry the same bits in
/// the Widescreen Signalling data), per
/// `dvd-substream-ids-and-copy-protection.md` §2c.
///
/// Field packing (transmission-side; do not confuse with the on-disc
/// [`CprMai`] storage):
///
/// | Bit   | Field                                          |
/// |-------|------------------------------------------------|
/// | b6–b5 | reserved                                       |
/// | b4–b3 | CGMS ([`Cgms`])                                |
/// | b2–b1 | APS ([`ApsType`])                              |
/// | b0    | ASB — Analog Source Bit (1 = pre-recorded)     |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CopyControlInfo {
    /// Copy-generation state.
    pub cgms: Cgms,
    /// Analog-protection process.
    pub aps: ApsType,
    /// ASB — Analog Source Bit (`true` = pre-recorded material).
    pub analog_source: bool,
}

impl CopyControlInfo {
    /// A fully permissive word: copy freely, no APS, ASB clear.
    pub const UNRESTRICTED: Self = Self {
        cgms: Cgms::CopyFreely,
        aps: ApsType::Off,
        analog_source: false,
    };

    /// Decode the §2c analog-output field. The reserved bits b6–b5
    /// (and anything above) are ignored, mirroring a receiver's
    /// obligation to mask them.
    pub fn from_analog_field(field: u8) -> Self {
        Self {
            cgms: Cgms::from_bits(field >> 3),
            aps: ApsType::from_bits(field >> 1),
            analog_source: field & 0b1 != 0,
        }
    }

    /// Encode the §2c analog-output field (reserved bits emitted as
    /// zero).
    pub fn to_analog_field(self) -> u8 {
        (self.cgms.bits() << 3) | (self.aps.bits() << 1) | u8::from(self.analog_source)
    }
}

// ------------------------------------------------------------------
// Sector-level CPR_MAI (§3a, ECMA-267 §16.3)
// ------------------------------------------------------------------

/// Length of the `CPR_MAI` field in bytes (ECMA-267 §16.3).
pub const CPR_MAI_LEN: usize = 6;

/// Length of a full DVD-ROM Data Frame in bytes: 4-byte ID + 2-byte
/// IED + 6-byte CPR_MAI + 2048 Main Data + 4-byte EDC (ECMA-267 §16).
pub const DATA_FRAME_LEN: usize = 2064;

/// Byte offset of `CPR_MAI` inside a Data Frame — right after the
/// 4-byte ID and its 2-byte IED (ECMA-267 §16).
pub const CPR_MAI_FRAME_OFFSET: usize = 6;

/// Decoded 6-byte sector-level Copyright Management Information
/// (`CPR_MAI`) field.
///
/// ECMA-267 §16.3 defines only the field's size (6 bytes), its
/// application-dependence, and its all-ZERO default. The byte-0
/// bit layout decoded here (`CPM` / `CP_SEC` / `CGMS`) is the
/// community **reconstruction** documented in
/// `dvd-substream-ids-and-copy-protection.md` §3a — widely relied on
/// by CSS-class descramblers but not normatively published. The raw
/// bytes are preserved verbatim alongside the decode so callers can
/// apply their own interpretation.
///
/// Note that a standard 2048-byte user-data sector read (and hence
/// any ordinary `.iso` image) does **not** contain this field — it
/// lives in the Data Frame header, visible only to raw-frame
/// tooling. The parser is provided for that tooling.
///
/// | Byte | Bits  | Field (reconstructed)                          |
/// |------|-------|------------------------------------------------|
/// | 0    | b7    | `CPM` — 1 = sector copyright-protected         |
/// | 0    | b6    | `CP_SEC` — 1 = sector CSS-scrambled            |
/// | 0    | b5–b4 | `CGMS` ([`Cgms`])                              |
/// | 0    | b3–b0 | reserved                                       |
/// | 1    |       | `RMI` — region-management / APS trigger byte   |
/// | 2–5  |       | reserved (all-ZERO default per ECMA-267)       |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CprMai {
    /// `CPM` — `true` when the sector is copyright-protected.
    pub cpm: bool,
    /// `CP_SEC` — `true` when the sector is CSS-scrambled.
    pub cp_sec: bool,
    /// Disc-side copy-generation state (byte 0, b5–b4).
    pub cgms: Cgms,
    /// `RMI` — region-management / analog-protection trigger byte,
    /// preserved raw (its internal layout is not pinned by any
    /// staged public source).
    pub rmi: u8,
    /// The verbatim 6 on-wire bytes.
    pub raw: [u8; CPR_MAI_LEN],
}

impl CprMai {
    /// Parse a 6-byte `CPR_MAI` field. `buf` must be at least
    /// [`CPR_MAI_LEN`] bytes; only the first 6 are consumed.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < CPR_MAI_LEN {
            return Err(Error::InvalidUdf("CPR_MAI: shorter than 6 bytes"));
        }
        let mut raw = [0u8; CPR_MAI_LEN];
        raw.copy_from_slice(&buf[..CPR_MAI_LEN]);
        Ok(Self {
            cpm: raw[0] & 0x80 != 0,
            cp_sec: raw[0] & 0x40 != 0,
            cgms: Cgms::from_bits(raw[0] >> 4),
            rmi: raw[1],
            raw,
        })
    }

    /// Parse the `CPR_MAI` field out of a full 2064-byte Data Frame
    /// (ECMA-267 §16: ID + IED + CPR_MAI + 2048 Main Data + EDC).
    pub fn from_data_frame(frame: &[u8]) -> Result<Self> {
        if frame.len() < DATA_FRAME_LEN {
            return Err(Error::InvalidUdf(
                "CPR_MAI: data frame shorter than 2064 bytes",
            ));
        }
        Self::parse(&frame[CPR_MAI_FRAME_OFFSET..CPR_MAI_FRAME_OFFSET + CPR_MAI_LEN])
    }

    /// `true` when every byte is zero — the ECMA-267 default for a
    /// disc that does not use the field.
    pub fn is_all_zero(&self) -> bool {
        self.raw == [0u8; CPR_MAI_LEN]
    }

    /// `true` when any of byte 0's reserved low nibble or the
    /// reserved bytes 2–5 carries a non-zero value — i.e. the field
    /// holds data the §3a reconstruction does not explain.
    pub fn has_unrecognised_bits(&self) -> bool {
        self.raw[0] & 0x0F != 0 || self.raw[2..].iter().any(|&b| b != 0)
    }

    /// The player-facing copy-control view of this sector: the
    /// disc-side CGMS with APS off (the sector layer does not carry
    /// a decodable APS *type* — the trigger rides in `RMI` / the
    /// VOBU `vobu_cat`, whose bit layouts are not publicly pinned)
    /// and ASB set, since DVD-Video is pre-recorded material.
    pub fn copy_control(&self) -> CopyControlInfo {
        CopyControlInfo {
            cgms: self.cgms,
            aps: ApsType::Off,
            analog_source: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- value tables (§2a / §2b) ---------------------------------

    #[test]
    fn cgms_two_bit_table() {
        // The four documented states, exactly.
        assert_eq!(Cgms::from_bits(0b00), Cgms::CopyFreely);
        assert_eq!(Cgms::from_bits(0b01), Cgms::CopyNoMore);
        assert_eq!(Cgms::from_bits(0b10), Cgms::CopyOnce);
        assert_eq!(Cgms::from_bits(0b11), Cgms::CopyNever);
        // bits() is the exact inverse and upper bits are ignored.
        for v in 0..=255u8 {
            let c = Cgms::from_bits(v);
            assert_eq!(c.bits(), v & 0b11);
        }
        assert!(Cgms::CopyFreely.copying_permitted());
        assert!(Cgms::CopyOnce.copying_permitted());
        assert!(!Cgms::CopyNoMore.copying_permitted());
        assert!(!Cgms::CopyNever.copying_permitted());
    }

    #[test]
    fn aps_type_table() {
        assert_eq!(ApsType::from_bits(0b00), ApsType::Off);
        assert_eq!(ApsType::from_bits(0b01), ApsType::Agc);
        assert_eq!(ApsType::from_bits(0b10), ApsType::AgcColorstripe2);
        assert_eq!(ApsType::from_bits(0b11), ApsType::AgcColorstripe4);
        for v in 0..=255u8 {
            let a = ApsType::from_bits(v);
            assert_eq!(a.bits(), v & 0b11);
        }
        assert!(!ApsType::Off.is_active());
        assert!(ApsType::Agc.is_active() && !ApsType::Agc.has_colorstripe());
        assert!(ApsType::AgcColorstripe2.has_colorstripe());
        assert!(ApsType::AgcColorstripe4.has_colorstripe());
    }

    // ----- analog-output field (§2c) --------------------------------

    /// Every possible CCI combination survives an encode → decode
    /// round trip, and decoding masks the reserved bits.
    #[test]
    fn analog_field_round_trips_exhaustively() {
        for cgms_bits in 0..4u8 {
            for aps_bits in 0..4u8 {
                for asb in [false, true] {
                    let cci = CopyControlInfo {
                        cgms: Cgms::from_bits(cgms_bits),
                        aps: ApsType::from_bits(aps_bits),
                        analog_source: asb,
                    };
                    let field = cci.to_analog_field();
                    // Encoded field uses only b4..b0.
                    assert_eq!(field & !0b0001_1111, 0);
                    assert_eq!(CopyControlInfo::from_analog_field(field), cci);
                }
            }
        }
        // Reserved bits b6–b5 (and b7) are ignored on decode.
        for field in 0..=255u8 {
            let a = CopyControlInfo::from_analog_field(field);
            let b = CopyControlInfo::from_analog_field(field & 0b0001_1111);
            assert_eq!(a, b, "field {field:#04x}");
        }
    }

    #[test]
    fn analog_field_bit_positions() {
        // CGMS at b4–b3.
        let f = CopyControlInfo {
            cgms: Cgms::CopyNever,
            aps: ApsType::Off,
            analog_source: false,
        }
        .to_analog_field();
        assert_eq!(f, 0b0001_1000);
        // APS at b2–b1.
        let f = CopyControlInfo {
            cgms: Cgms::CopyFreely,
            aps: ApsType::AgcColorstripe4,
            analog_source: false,
        }
        .to_analog_field();
        assert_eq!(f, 0b0000_0110);
        // ASB at b0.
        let f = CopyControlInfo {
            cgms: Cgms::CopyFreely,
            aps: ApsType::Off,
            analog_source: true,
        }
        .to_analog_field();
        assert_eq!(f, 0b0000_0001);
        assert_eq!(CopyControlInfo::UNRESTRICTED.to_analog_field(), 0);
    }

    // ----- CPR_MAI (§3a) --------------------------------------------

    #[test]
    fn cpr_mai_decodes_reconstructed_byte0() {
        // CPM=1, CP_SEC=1, CGMS=11 (copy never) → byte 0 = 0xF0.
        let mai = CprMai::parse(&[0xF0, 0x00, 0, 0, 0, 0]).unwrap();
        assert!(mai.cpm);
        assert!(mai.cp_sec);
        assert_eq!(mai.cgms, Cgms::CopyNever);
        assert!(!mai.is_all_zero());
        assert!(!mai.has_unrecognised_bits());

        // CPM=1, CP_SEC=0, CGMS=10 (copy once) → 0xA0.
        let mai = CprMai::parse(&[0xA0, 0x07, 0, 0, 0, 0]).unwrap();
        assert!(mai.cpm);
        assert!(!mai.cp_sec);
        assert_eq!(mai.cgms, Cgms::CopyOnce);
        assert_eq!(mai.rmi, 0x07);

        // The ECMA-267 all-ZERO default: unprotected, copy freely.
        let mai = CprMai::parse(&[0u8; 6]).unwrap();
        assert!(mai.is_all_zero());
        assert!(!mai.cpm && !mai.cp_sec);
        assert_eq!(mai.cgms, Cgms::CopyFreely);
        let cci = mai.copy_control();
        assert_eq!(cci.cgms, Cgms::CopyFreely);
        assert_eq!(cci.aps, ApsType::Off);
        assert!(cci.analog_source);
    }

    #[test]
    fn cpr_mai_flags_unrecognised_bits() {
        // Reserved low nibble of byte 0.
        let mai = CprMai::parse(&[0x01, 0, 0, 0, 0, 0]).unwrap();
        assert!(mai.has_unrecognised_bits());
        // Reserved tail bytes.
        let mai = CprMai::parse(&[0x00, 0, 0, 0, 0, 0xFF]).unwrap();
        assert!(mai.has_unrecognised_bits());
        // RMI alone is NOT unrecognised — it is a named field.
        let mai = CprMai::parse(&[0x00, 0x55, 0, 0, 0, 0]).unwrap();
        assert!(!mai.has_unrecognised_bits());
    }

    #[test]
    fn cpr_mai_rejects_short_input() {
        for len in 0..CPR_MAI_LEN {
            assert!(CprMai::parse(&vec![0u8; len]).is_err(), "len {len}");
        }
        // Extra bytes beyond 6 are fine (only the first 6 consumed).
        let mai = CprMai::parse(&[0xF0, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        assert_eq!(mai.raw, [0xF0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn cpr_mai_from_data_frame() {
        // Frame layout: ID(4) + IED(2) + CPR_MAI(6) + 2048 + EDC(4).
        let mut frame = vec![0u8; DATA_FRAME_LEN];
        frame[6] = 0xE0; // CPM=1, CP_SEC=1, CGMS=10
        frame[7] = 0x2A; // RMI
        let mai = CprMai::from_data_frame(&frame).unwrap();
        assert!(mai.cpm && mai.cp_sec);
        assert_eq!(mai.cgms, Cgms::CopyOnce);
        assert_eq!(mai.rmi, 0x2A);
        // A 2048-byte user sector is NOT a data frame.
        assert!(CprMai::from_data_frame(&frame[..2048]).is_err());
    }

    #[test]
    fn round_trip_via_raw_bytes() {
        // Reconstructed fields re-derive identically from the
        // preserved raw bytes for every byte-0 value.
        for b0 in 0..=255u8 {
            let src = [b0, 0x11, 0x22, 0x33, 0x44, 0x55];
            let mai = CprMai::parse(&src).unwrap();
            assert_eq!(mai.raw, src);
            let again = CprMai::parse(&mai.raw).unwrap();
            assert_eq!(again, mai);
            assert_eq!(mai.cpm, b0 & 0x80 != 0);
            assert_eq!(mai.cp_sec, b0 & 0x40 != 0);
            assert_eq!(mai.cgms.bits(), (b0 >> 4) & 0b11);
        }
    }
}
