//! High-level Phase 3b pipeline glue.
//!
//! Wraps [`crate::mkv_writer::write_title_to_mkv`] in the same source-
//! URI surface the rest of oxideav uses: a caller passes a
//! `dvd:///path/disc.iso` URI (or a plain filesystem path), a title
//! index, and an output file path; the function opens the disc,
//! resolves the title, and writes the MKV.
//!
//! ## Why a separate module
//!
//! `mkv_writer` is the bit-level glue between
//! [`crate::vob::PesPacket`] and [`oxideav_mkv::mux::MkvMuxer`]. This
//! module is the front door: it owns URI parsing, title enumeration,
//! and the lookup conventions a CLI ([`oxideav-cli-convert`]) would
//! call into. The split keeps `mkv_writer` testable against an
//! already-opened [`crate::DvdDisc`] while `pipeline` is the
//! integration seam.
//!
//! ## Source URIs accepted
//!
//! * `dvd:///abs/path/to/disc.iso` — explicit disc image / block dev.
//! * `dvd:///dev/sr0` — Unix block device.
//! * `/abs/path/to/disc.iso` — bare filesystem path (handy for tests).
//!
//! `dvd://` (auto-detect) is rejected — see [`crate::source::DvdUri`]
//! for Phase-2 status.
//!
//! ## Wall
//!
//! No external implementation source consulted — clean-room from the
//! `docs/container/dvd/` references only.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::source::{parse_dvd_uri, DvdUri};
use crate::DvdDisc;

/// Phase 3b high-level entry: open `source`, pick `title_idx`
/// (1-based), and write the title to `out_path` as a Matroska file.
///
/// `source` may be a `dvd://...` URI or a bare filesystem path.
///
/// Returns `Error::NotDvdVideo` for auto-detect URIs (Phase 2),
/// missing files, or out-of-range `title_idx`. Returns
/// `Error::InvalidUdf` for malformed IFOs/VOBs (the same surface
/// `crate::DvdDisc::open` uses).
pub fn convert_dvd_to_mkv(
    source: &str,
    title_idx: usize,
    out_path: impl AsRef<Path>,
) -> Result<()> {
    let image_path = resolve_source(source)?;
    let disc = DvdDisc::open(&image_path)?;
    crate::mkv_writer::write_title_to_mkv(&disc, title_idx, &image_path, out_path)
}

/// Enumerate the disc's titles (`1..=title_count`).
///
/// Convenience for CLI front-ends that want to surface the title list
/// before letting the user pick one. Delegates to
/// [`crate::DvdDisc::enumerate_titles`].
pub fn list_titles(source: &str) -> Result<Vec<crate::ifo::DvdTitleEntry>> {
    let image_path = resolve_source(source)?;
    let disc = DvdDisc::open(&image_path)?;
    let mut reader = std::fs::File::open(&image_path)?;
    disc.enumerate_titles(&mut reader)
}

/// Parse a `dvd://...` URI (or accept a bare filesystem path) into a
/// concrete image path the [`DvdDisc::open`] reader can consume.
fn resolve_source(source: &str) -> Result<PathBuf> {
    if source.starts_with("dvd:") {
        match parse_dvd_uri(source)? {
            DvdUri::AutoDetect => Err(Error::NotDvdVideo(
                "convert_dvd_to_mkv: dvd:// auto-detect is Phase 2 — \
                 pass an explicit dvd:///path/to/disc.iso URI",
            )),
            DvdUri::Path(p) => Ok(p),
        }
    } else {
        Ok(PathBuf::from(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_dvd_uri_with_path() {
        let p = resolve_source("dvd:///tmp/disc.iso").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/disc.iso"));
    }

    #[test]
    fn resolve_bare_path() {
        let p = resolve_source("/var/lib/disc.iso").unwrap();
        assert_eq!(p, PathBuf::from("/var/lib/disc.iso"));
    }

    #[test]
    fn resolve_dvd_auto_rejected() {
        let err = resolve_source("dvd://").unwrap_err();
        match err {
            Error::NotDvdVideo(_) => {}
            other => panic!("expected NotDvdVideo for auto-detect, got {other:?}"),
        }
    }
}
