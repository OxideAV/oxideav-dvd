//! Read-only **DVD-Video** disc reader — ISO 9660 + UDF 1.02 mount +
//! `VIDEO_TS/` directory walk.
//!
//! Phase 1 (this release) handles the physical + filesystem + disc-
//! identification layers — enough to point a player at a DVD-Video
//! disc image or block device and enumerate the title-set files.
//! **No IFO / VOB / PGC / VM / CSS parsing yet.**
//!
//! ## Scope
//!
//! - ISO 9660 PVD + path-table + directory walk (the ECMA-268
//!   bridge layer).
//! - UDF 1.02 mount: AVDP, Volume Descriptor Sequence, File Set
//!   Descriptor, root File Identifier Descriptor walk, File Entry
//!   parsing with short_ad / long_ad / ext_ad allocation descriptors.
//! - `VIDEO_TS/` file enumeration: `VIDEO_TS.IFO` + per-VTS
//!   `VTS_xx_0.IFO` / `VTS_xx_0.VOB` (menu) / `VTS_xx_1..9.VOB`
//!   (title content) / `VTS_xx_0.BUP`.
//! - `dvd://` URI handler (default-on `registry` feature) that
//!   surfaces a `DvdDiscSource` to `oxideav_core::SourceRegistry`.
//!
//! Out of scope (deferred to Phase 2 / Phase 3):
//! - IFO body parsing (VMGI, VTSI, PGCI, cell-address tables).
//! - VOB demuxing (MPEG-2 Program Stream + nav-pack overlays).
//! - VM execution (HDMV navigation opcodes, SPRMs / GPRMs).
//! - CSS authentication + descrambling (lives in a future
//!   `oxideav-css` sibling crate).
//!
//! ## Clean-room references
//!
//! - `docs/container/dvd/physical/ECMA-267_3rd_edition_april_2001.pdf`
//! - `docs/container/dvd/physical/ECMA-268_3rd_edition_april_2001.pdf`
//! - `docs/container/dvd/physical/OSTA_UDF_1.02.pdf`
//! - `docs/container/bluray/ECMA-167_3rd_edition_june_1997.pdf` (cross-ref)
//! - `docs/container/dvd/application/mpucoder-ifo.html` (for the
//!   VIDEO_TS directory layout, file-naming convention, and BUP
//!   backup semantics only — IFO bodies are out of Phase-1 scope)
//!
//! ## Quick start
//!
//! ```no_run
//! use oxideav_dvd::DvdDisc;
//!
//! let disc = DvdDisc::open("path/to/disc.iso").unwrap();
//! println!("volume_id = {}", disc.volume_id);
//! println!("title_set_count = {}", disc.title_set_count);
//! for f in &disc.video_ts_files {
//!     println!("  {:?}  lba={}  size={}", f.kind, f.lba, f.size);
//! }
//! ```
//!
//! ## Standalone build
//!
//! `oxideav-core` is gated behind the default-on `registry` feature.
//! Drop the framework dependency entirely with:
//!
//! ```toml
//! oxideav-dvd = { version = "0.0", default-features = false }
//! ```

#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod disc;
pub mod error;
pub mod iso9660;
pub mod source;
pub mod udf;

pub use disc::{DvdDisc, DvdFile, DvdFileKind};
pub use error::{Error, Result};
pub use iso9660::{
    DirectoryRecord, Iso9660Entry, Iso9660Volume, PathTableEntry, PrimaryVolumeDescriptor,
    VolumeDescriptorType,
};
pub use source::{parse_dvd_uri, DvdDiscSource, DvdUri};
pub use udf::{
    AdType, AnchorVolumeDescriptorPointer, DescriptorTag, ExtAd, Extent, FileEntry,
    FileIdentifierDescriptor, FileSetDescriptor, IcbTag, LbAddr, LogicalVolumeDescriptor, LongAd,
    PartitionDescriptor, ShortAd, TagId, UdfFile, UdfVolume,
};

#[cfg(feature = "registry")]
pub use source::register;

// Canonical sibling entry point. Registers the `dvd://` source driver
// under `oxideav_core::RuntimeContext::sources`.
#[cfg(feature = "registry")]
oxideav_core::register!("dvd", source::register);
