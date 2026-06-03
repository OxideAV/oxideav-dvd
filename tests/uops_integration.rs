//! Cross-module wiring tests for the [`oxideav_dvd::uops`] module.
//!
//! Validates that the typed `UopMask` accessor exposed on
//! [`Pgc`], [`PciPacket`], and [`DvdTitleEntry`] reads the same bit
//! the parser placed into the raw field — and that the three-level
//! OR-merge prescribed by `mpucoder-uops.html` reproduces the
//! "set bit in *any* mask inhibits the associated control"
//! semantics.

use oxideav_dvd::ifo::{DvdTitleEntry, Pgc};
use oxideav_dvd::uops::{UopLevel, UopMask, UserOp};
use oxideav_dvd::vob::PciPacket;

/// Build a TT_SRPT title entry by hand — bypasses the on-disc
/// parser so the test only validates the accessor logic.
fn make_title_entry(title_type: u8) -> DvdTitleEntry {
    DvdTitleEntry {
        title_type,
        angle_count: 1,
        chapter_count: 1,
        parental_mask: 0,
        vts_number: 1,
        vts_title_number: 1,
        vts_start_sector: 0,
    }
}

/// Build a minimal PGC byte buffer whose only meaningful field is
/// `prohibited_user_ops` at offset `0x0008`. All other fixed-header
/// fields stay zero / default.
fn make_pgc_bytes(prohibited: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0xEC];
    buf[0x0008..0x000C].copy_from_slice(&prohibited.to_be_bytes());
    buf
}

/// Build a minimal PCI body whose only meaningful field is
/// `vobu_uop_ctl` at `PCI_GI 08`. We size to 0x62 so `parse_highlight`
/// has just enough to peek at HLI_GI + bail on `btn_ns == 0`.
fn make_pci_bytes(vobu_uop_ctl: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0x62];
    buf[0x08..0x0C].copy_from_slice(&vobu_uop_ctl.to_be_bytes());
    buf
}

#[test]
fn dvd_title_entry_uop_mask_extracts_title_type_low_bits() {
    // title_type = 0b0000_0011 → both TT_SRPT-applicable ops prohibited.
    let e = make_title_entry(0b0011);
    let m = e.uop_mask();
    assert!(m.contains(UserOp::TimePlayOrSearch));
    assert!(m.contains(UserOp::PttPlayOrSearch));
    assert!(!e.is_user_op_allowed(UserOp::TimePlayOrSearch));
    assert!(!e.is_user_op_allowed(UserOp::PttPlayOrSearch));

    // title_type's high bits (jump/link/call permission etc.) MUST
    // NOT bleed into the UOP surface.
    let e = make_title_entry(0b1111_1100);
    let m = e.uop_mask();
    assert!(m.is_empty());
    assert!(e.is_user_op_allowed(UserOp::TimePlayOrSearch));
    assert!(e.is_user_op_allowed(UserOp::PttPlayOrSearch));

    // TT_SRPT can never carry any op outside bits 0/1, no matter
    // what the title_type byte looks like — so every other op is
    // trivially "allowed" at this level.
    let e = make_title_entry(0xFF);
    let m = e.uop_mask();
    assert!(m.fits_level(UopLevel::TitleSearchPointer));
    assert!(e.is_user_op_allowed(UserOp::ForwardScan));
    assert!(e.is_user_op_allowed(UserOp::AngleChange));
}

#[test]
fn pgc_uop_mask_reads_offset_0x0008() {
    // Set bit 3 (Stop) and bit 22 (AngleChange).
    let raw = UserOp::Stop.mask() | UserOp::AngleChange.mask();
    let bytes = make_pgc_bytes(raw);
    let pgc = Pgc::parse(&bytes).expect("PGC header should parse");

    assert_eq!(pgc.prohibited_user_ops, raw);
    let m = pgc.uop_mask();
    assert_eq!(m, UopMask::from_bits(raw));
    assert!(!pgc.is_user_op_allowed(UserOp::Stop));
    assert!(!pgc.is_user_op_allowed(UserOp::AngleChange));
    assert!(pgc.is_user_op_allowed(UserOp::Resume));
    assert!(pgc.is_user_op_allowed(UserOp::Stop) == m.is_allowed(UserOp::Stop));
}

#[test]
fn pgc_uop_mask_empty_when_unset() {
    let bytes = make_pgc_bytes(0);
    let pgc = Pgc::parse(&bytes).expect("PGC header should parse");
    assert!(pgc.uop_mask().is_empty());
    // Every op is allowed at this level.
    for op in UserOp::ALL {
        assert!(pgc.is_user_op_allowed(op), "{:?} expected allowed", op);
    }
}

#[test]
fn pci_packet_uop_mask_reads_offset_0x0008() {
    // Bit 8 (ForwardScan) + bit 22 (AngleChange).
    let raw = UserOp::ForwardScan.mask() | UserOp::AngleChange.mask();
    let bytes = make_pci_bytes(raw);
    let pci = PciPacket::parse(&bytes).expect("PCI header should parse");

    assert_eq!(pci.vobu_uop_ctl, raw);
    assert_eq!(pci.uop_mask(), UopMask::from_bits(raw));
    assert!(!pci.is_user_op_allowed(UserOp::ForwardScan));
    assert!(!pci.is_user_op_allowed(UserOp::AngleChange));
    assert!(pci.is_user_op_allowed(UserOp::Resume));
}

#[test]
fn three_level_or_merge_inhibits_when_any_layer_sets_the_bit() {
    // TT_SRPT prohibits bit 0 (TimePlayOrSearch) only.
    let tt = make_title_entry(0b01).uop_mask();
    // PGC prohibits Stop.
    let pgc_bytes = make_pgc_bytes(UserOp::Stop.mask());
    let pgc = Pgc::parse(&pgc_bytes).unwrap();
    // PCI prohibits AngleChange.
    let pci_bytes = make_pci_bytes(UserOp::AngleChange.mask());
    let pci = PciPacket::parse(&pci_bytes).unwrap();

    let merged = UopMask::merge_or(tt, pgc.uop_mask(), pci.uop_mask());

    // Every layer's prohibition surfaces.
    assert!(merged.contains(UserOp::TimePlayOrSearch));
    assert!(merged.contains(UserOp::Stop));
    assert!(merged.contains(UserOp::AngleChange));

    // Three set bits in total — the merge is OR, not sum.
    assert_eq!(merged.count(), 3);

    // Operations no level prohibits stay allowed.
    assert!(merged.is_allowed(UserOp::Resume));
    assert!(merged.is_allowed(UserOp::ForwardScan));
}

#[test]
fn merge_or_identity_when_two_layers_unset() {
    let tt = UopMask::NONE;
    let pgc = make_pgc_bytes(UserOp::PauseOn.mask());
    let pgc = Pgc::parse(&pgc).unwrap();
    let pci = make_pci_bytes(0);
    let pci = PciPacket::parse(&pci).unwrap();

    let merged = UopMask::merge_or(tt, pgc.uop_mask(), pci.uop_mask());
    // Only PauseOn from the PGC layer survives.
    let solo: Vec<UserOp> = merged.iter().collect();
    assert_eq!(solo, vec![UserOp::PauseOn]);
}

#[test]
fn pgc_level_carries_every_op_except_go_up() {
    // PGC sets every defined bit. is_user_op_allowed returns false
    // for every op except GoUp (which the spec table's PGC column
    // leaves blank — i.e. PGC can carry the bit, but if you trust
    // the spec, a compliant disc shouldn't ever set it; either way
    // the accessor should still return what the raw word says).
    let bytes = make_pgc_bytes(oxideav_dvd::uops::UOP_DEFINED_BITS);
    let pgc = Pgc::parse(&bytes).expect("PGC header should parse");
    for op in UserOp::ALL {
        assert!(!pgc.is_user_op_allowed(op), "{:?} should be blocked", op);
    }
    // The raw mask carries the GoUp bit, but fits_level reports
    // false for ProgramChain because the spec table's PGC column
    // is blank at row 4.
    assert!(!pgc.uop_mask().fits_level(UopLevel::ProgramChain));
}
