//! High-level DVD-Video disc detection and `VIDEO_TS/` enumeration.
//!
//! Phase 1: open a file or block device, sniff for an ISO 9660 PVD
//! and a UDF AVDP, walk the volume to find `VIDEO_TS/`, and enumerate
//! the title-set files (IFO / VOB / BUP). **No IFO / VOB body parsing
//! yet** — that's Phase 2.
//!
//! ## DVD-Video file naming convention
//!
//! Per ECMA-268 §9 + mpucoder's `dvd.sourceforge.net/dvdinfo/ifo.html`:
//!
//! ```text
//!   VIDEO_TS/                 top-level mandatory directory
//!       VIDEO_TS.IFO          Video Manager information (always)
//!       VIDEO_TS.VOB          VMG menu video (optional)
//!       VIDEO_TS.BUP          backup of VIDEO_TS.IFO (always — per spec)
//!       VTS_xx_0.IFO          Video Title Set xx information (xx = 01..99)
//!       VTS_xx_0.VOB          VTS xx menu video (optional)
//!       VTS_xx_1.VOB          VTS xx title content
//!       VTS_xx_2.VOB          ...continued (up to .9 — 1 GB per VOB)
//!       ...
//!       VTS_xx_9.VOB
//!       VTS_xx_0.BUP          backup of VTS_xx_0.IFO (always)
//!   AUDIO_TS/                 empty on DVD-Video discs (or absent)
//! ```
//!
//! We classify each file we see into [`DvdFileKind`] and surface the
//! resulting list as [`DvdDisc`]. The discriminator for "this is a
//! DVD-Video disc" is `VIDEO_TS/VIDEO_TS.IFO`: a disc with `VIDEO_TS/`
//! but no `VIDEO_TS.IFO` is rejected as non-DVD-Video.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{Error, Result};
use crate::ifo::{DvdTitleEntry, TtSrpt, VmgIfo, VtsIfo};
use crate::iso9660::Iso9660Volume;
use crate::udf::{UdfFile, UdfVolume};

/// Top-level kind of a DVD-Video file. The encoding mirrors what
/// libdvdread / mpucoder name these files, but the discriminator is
/// purely lexical — we don't peek inside any of the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DvdFileKind {
    /// `VIDEO_TS.IFO` — Video Manager Information. Mandatory.
    Vmgi,
    /// `VIDEO_TS.VOB` — VMG menu video. Optional.
    VmgMenu,
    /// `VIDEO_TS.BUP` — backup of `VIDEO_TS.IFO`. Mandatory per spec.
    VmgiBup,
    /// `VTS_xx_0.IFO` — VTS information file. `xx` = 1..=99.
    Vtsi(u8),
    /// `VTS_xx_0.VOB` — VTS menu video. Optional.
    VtsMenu(u8),
    /// `VTS_xx_N.VOB` — VTS title content. `vob` = 1..=9.
    VtsTitle { ts: u8, vob: u8 },
    /// `VTS_xx_0.BUP` — backup of `VTS_xx_0.IFO`.
    VtsiBup(u8),
}

/// A file we found in `VIDEO_TS/` (or `AUDIO_TS/`), enumerated.
#[derive(Debug, Clone)]
pub struct DvdFile {
    pub kind: DvdFileKind,
    pub name: String,
    /// Logical Block Address (sector number) of the file's first extent.
    pub lba: u32,
    /// Total file length in bytes.
    pub size: u64,
    /// Title set this file belongs to (0 = VMG, 1..=99 = VTS).
    pub title_set: u8,
    /// 1-based VOB index for `VtsTitle` files (1..=9), `0` otherwise.
    pub vob_index: u8,
}

/// A successfully-detected DVD-Video disc.
#[derive(Debug, Clone)]
pub struct DvdDisc {
    /// Volume identifier (from ISO 9660 PVD if available, otherwise
    /// from the UDF Primary Volume Descriptor).
    pub volume_id: String,
    /// 1..=99 — number of VTS title sets present.
    pub title_set_count: u8,
    /// All `VIDEO_TS/` files we found, sorted by `(title_set,
    /// vob_index, kind)` for stable iteration.
    pub video_ts_files: Vec<DvdFile>,
    /// All `AUDIO_TS/` files (typically empty on DVD-Video).
    pub audio_ts_files: Vec<DvdFile>,
}

impl DvdDisc {
    /// Open and detect a DVD-Video disc at the given filesystem path
    /// (an `.iso` image or a block-device path on Unix).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let f = File::open(path.as_ref())?;
        Self::from_reader(f)
    }

    /// Detect a DVD-Video disc against an arbitrary `Read + Seek`
    /// source. UDF is tried first (the DVD-mandated file system);
    /// the ISO 9660 PVD fallback runs only if the UDF mount fails,
    /// covering authoring-tool outputs that omit the AVDP.
    pub fn from_reader<R: Read + Seek>(mut reader: R) -> Result<Self> {
        // Strategy: try UDF first (the DVD spec mandates UDF 1.02 as
        // the primary filesystem; ISO 9660 is only the bridge layer).
        // Fall back to ISO 9660 if UDF mount fails — useful for
        // authoring tools that omit the AVDP but still publish a
        // readable VIDEO_TS through the bridge.
        let udf_attempt = UdfVolume::open(&mut reader);
        match udf_attempt {
            Ok(mut udf) => Self::from_udf(&mut udf),
            Err(_udf_err) => Self::from_iso9660(reader),
        }
    }

    /// Build a `DvdDisc` from a UDF volume.
    pub fn from_udf<R: Read + Seek>(udf: &mut UdfVolume<R>) -> Result<Self> {
        let volume_id = if udf.volume_identifier.is_empty() {
            udf.logical_volume_identifier.clone()
        } else {
            udf.volume_identifier.clone()
        };

        let root_entries = udf.read_directory(udf.root_directory_icb)?;
        let video_ts_dir = root_entries
            .iter()
            .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("VIDEO_TS"))
            .ok_or(Error::NotDvdVideo("no VIDEO_TS/ directory at volume root"))?;
        let audio_ts_dir = root_entries
            .iter()
            .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("AUDIO_TS"));

        let video_ts_entries = udf.read_directory(video_ts_dir.icb)?;
        let audio_ts_entries = if let Some(at) = audio_ts_dir {
            udf.read_directory(at.icb)?
        } else {
            Vec::new()
        };

        Self::classify(volume_id, &video_ts_entries, &audio_ts_entries)
    }

    /// Build a `DvdDisc` purely from the ISO 9660 bridge layer.
    pub fn from_iso9660<R: Read + Seek>(reader: R) -> Result<Self> {
        let mut iso = Iso9660Volume::open(reader)?;
        let volume_id = iso.pvd.volume_id.clone();
        let root_entries = iso.list_root()?;
        let video_ts = root_entries
            .iter()
            .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("VIDEO_TS"))
            .ok_or(Error::NotDvdVideo("no VIDEO_TS/ directory at volume root"))?
            .clone();
        let audio_ts = root_entries
            .iter()
            .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("AUDIO_TS"))
            .cloned();

        let video_ts_files = iso.list_dir(video_ts.lba, video_ts.size)?;
        let audio_ts_files = if let Some(at) = audio_ts {
            iso.list_dir(at.lba, at.size)?
        } else {
            Vec::new()
        };

        // Adapt ISO 9660 entries to the UDF-derived classification path.
        let video_ts_udf: Vec<UdfFile> = video_ts_files
            .into_iter()
            .map(|e| UdfFile {
                name: e.name,
                is_dir: e.is_dir,
                extents: Vec::new(),
                length: e.size as u64,
                icb: crate::udf::LongAd {
                    length: 0,
                    extent_type: 0,
                    location: crate::udf::LbAddr {
                        block: e.lba,
                        partition_ref: 0,
                    },
                    implementation_use: [0; 6],
                },
            })
            .collect();
        let audio_ts_udf: Vec<UdfFile> = audio_ts_files
            .into_iter()
            .map(|e| UdfFile {
                name: e.name,
                is_dir: e.is_dir,
                extents: Vec::new(),
                length: e.size as u64,
                icb: crate::udf::LongAd {
                    length: 0,
                    extent_type: 0,
                    location: crate::udf::LbAddr {
                        block: e.lba,
                        partition_ref: 0,
                    },
                    implementation_use: [0; 6],
                },
            })
            .collect();

        Self::classify(volume_id, &video_ts_udf, &audio_ts_udf)
    }

    fn classify(volume_id: String, video_ts: &[UdfFile], audio_ts: &[UdfFile]) -> Result<Self> {
        let mut video_ts_files: Vec<DvdFile> = video_ts
            .iter()
            .filter(|e| !e.is_dir)
            .filter_map(|e| classify_video_ts_file(e).map(|kind| build_dvd_file(e, kind)))
            .collect();

        // Must include VIDEO_TS.IFO.
        let has_vmgi = video_ts_files.iter().any(|f| f.kind == DvdFileKind::Vmgi);
        if !has_vmgi {
            return Err(Error::NotDvdVideo(
                "VIDEO_TS/ present but VIDEO_TS.IFO is missing",
            ));
        }

        let audio_ts_files: Vec<DvdFile> = audio_ts
            .iter()
            .filter(|e| !e.is_dir)
            .filter_map(|e| classify_audio_ts_file(e).map(|kind| build_dvd_file(e, kind)))
            .collect();

        video_ts_files.sort_by_key(|f| (f.title_set, f.vob_index, sort_kind_priority(f.kind)));
        let title_set_count = video_ts_files
            .iter()
            .map(|f| f.title_set)
            .filter(|ts| *ts > 0)
            .max()
            .unwrap_or(0);

        Ok(Self {
            volume_id,
            title_set_count,
            video_ts_files,
            audio_ts_files,
        })
    }

    /// Lookup the first VOB of a given VTS (1..=99), if present.
    pub fn vts_title_vob(&self, ts: u8, vob: u8) -> Option<&DvdFile> {
        self.video_ts_files
            .iter()
            .find(|f| f.kind == DvdFileKind::VtsTitle { ts, vob })
    }

    /// Lookup VIDEO_TS.IFO, if present.
    pub fn vmgi(&self) -> Option<&DvdFile> {
        self.video_ts_files
            .iter()
            .find(|f| f.kind == DvdFileKind::Vmgi)
    }

    /// Look up `VTS_xx_0.IFO` for a given title-set number (1..=99).
    pub fn vtsi(&self, ts: u8) -> Option<&DvdFile> {
        self.video_ts_files
            .iter()
            .find(|f| f.kind == DvdFileKind::Vtsi(ts))
    }

    /// Read `VIDEO_TS.IFO` from `reader` and parse the VMG IFO body.
    ///
    /// This consumes the IFO's first sector(s) (currently only the
    /// `VMGI_MAT` region at offset 0). Phase 3 will materialise the
    /// remaining tables (`TT_SRPT`, `VMGM_PGCI_UT`, `VMG_PTL_MAIT`,
    /// `VMG_VTS_ATRT`); for now we surface those via separate
    /// [`Self::parse_vmg_tt_srpt`] / etc.
    pub fn parse_vmg<R: Read + Seek>(&self, reader: &mut R) -> Result<VmgIfo> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let buf = read_sector_range(reader, f.lba, 1)?;
        VmgIfo::parse(&buf)
    }

    /// Read `VTS_xx_0.IFO` and parse its body (VTSI_MAT + PTT_SRPT +
    /// PGCI + C_ADT, all materialised through [`VtsIfo`]).
    pub fn parse_vts<R: Read + Seek>(&self, reader: &mut R, ts_index: u8) -> Result<VtsIfo> {
        let f = self
            .vtsi(ts_index)
            .ok_or(Error::NotDvdVideo("VTS_xx_0.IFO absent"))?;
        // Read the whole IFO into RAM. IFO files are tiny (usually
        // <512 KB even on multi-hour discs) so the simplicity of a
        // single in-memory buffer outweighs streaming.
        let sectors = (f.size.div_ceil(crate::ifo::DVD_SECTOR as u64)) as u32;
        let buf = read_sector_range(reader, f.lba, sectors.max(1))?;
        VtsIfo::parse(&buf, ts_index)
    }

    /// Read `VIDEO_TS.IFO`'s `TT_SRPT` table (the disc's title list)
    /// and return its entries.
    pub fn enumerate_titles<R: Read + Seek>(&self, reader: &mut R) -> Result<Vec<DvdTitleEntry>> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        // Parse the MAT to find TT_SRPT's sector.
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        if mat.tt_srpt_sector == 0 {
            return Err(Error::InvalidUdf("VMG_MAT: tt_srpt_sector is zero"));
        }
        // Read TT_SRPT — one sector covers up to ~170 titles (sector
        // is 2048 bytes; entry is 12 bytes; 8-byte header). DVDs cap
        // at 99 title sets and ~99 titles total per spec.
        let tt_buf = read_sector_range(reader, f.lba + mat.tt_srpt_sector, 1)?;
        let tt = TtSrpt::parse(&tt_buf)?;
        Ok(tt.entries)
    }

    /// Stand-alone helper used by [`Self::parse_vmg`] callers who only
    /// need the title list (and not the IFO's MAT structure).
    pub fn parse_vmg_tt_srpt<R: Read + Seek>(&self, reader: &mut R) -> Result<TtSrpt> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        let tt_buf = read_sector_range(reader, f.lba + mat.tt_srpt_sector, 1)?;
        TtSrpt::parse(&tt_buf)
    }
}

/// Read `count` consecutive 2048-byte logical sectors starting at
/// disc-LBA `start` from `reader`.
pub(crate) fn read_sector_range<R: Read + Seek>(
    reader: &mut R,
    start: u32,
    count: u32,
) -> Result<Vec<u8>> {
    let sector = crate::ifo::DVD_SECTOR as u64;
    reader.seek(SeekFrom::Start(u64::from(start) * sector))?;
    let mut buf = vec![0u8; (count as usize) * sector as usize];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

fn build_dvd_file(e: &UdfFile, kind: DvdFileKind) -> DvdFile {
    let lba = e
        .extents
        .first()
        .map(|x| x.block_location)
        .unwrap_or(e.icb.location.block);
    let (title_set, vob_index) = match kind {
        DvdFileKind::Vmgi | DvdFileKind::VmgMenu | DvdFileKind::VmgiBup => (0, 0),
        DvdFileKind::Vtsi(ts) | DvdFileKind::VtsMenu(ts) | DvdFileKind::VtsiBup(ts) => (ts, 0),
        DvdFileKind::VtsTitle { ts, vob } => (ts, vob),
    };
    DvdFile {
        kind,
        name: e.name.clone(),
        lba,
        size: e.length,
        title_set,
        vob_index,
    }
}

fn sort_kind_priority(k: DvdFileKind) -> u8 {
    match k {
        DvdFileKind::Vmgi => 0,
        DvdFileKind::Vtsi(_) => 0,
        DvdFileKind::VmgMenu => 1,
        DvdFileKind::VtsMenu(_) => 1,
        DvdFileKind::VtsTitle { .. } => 2,
        DvdFileKind::VmgiBup => 3,
        DvdFileKind::VtsiBup(_) => 3,
    }
}

/// Classify a VIDEO_TS file name (case-insensitive).
pub fn classify_video_ts_file(e: &UdfFile) -> Option<DvdFileKind> {
    let upper = e.name.to_uppercase();
    match upper.as_str() {
        "VIDEO_TS.IFO" => Some(DvdFileKind::Vmgi),
        "VIDEO_TS.VOB" => Some(DvdFileKind::VmgMenu),
        "VIDEO_TS.BUP" => Some(DvdFileKind::VmgiBup),
        _ => parse_vts_filename(&upper),
    }
}

/// Classify an AUDIO_TS file name — DVD-Video discs typically leave
/// `AUDIO_TS/` empty. DVD-Audio uses it; we don't claim DVD-Audio
/// support here so we surface unknowns as `None` and let the caller
/// drop them.
pub fn classify_audio_ts_file(_e: &UdfFile) -> Option<DvdFileKind> {
    None
}

/// Parse `VTS_xx_y.{IFO,VOB,BUP}`. Returns `None` for any other
/// filename (no thrown error — the surrounding classifier silently
/// drops unknowns, since DVD authoring tools sometimes leave stray
/// files like `JACKET_P/`, but those don't break the disc).
fn parse_vts_filename(name: &str) -> Option<DvdFileKind> {
    let rest = name.strip_prefix("VTS_")?;
    // 2-digit title-set + underscore + 1-digit vob + dot + 3-char ext
    // = 8 chars total.
    if rest.len() != 8 {
        return None;
    }
    let ts_str = rest.get(0..2)?;
    if rest.as_bytes().get(2)? != &b'_' {
        return None;
    }
    let vob_str = rest.get(3..4)?;
    if rest.as_bytes().get(4)? != &b'.' {
        return None;
    }
    let ext = rest.get(5..)?;
    let ts = ts_str.parse::<u8>().ok()?;
    let vob = vob_str.parse::<u8>().ok()?;
    if !(1..=99).contains(&ts) {
        return None;
    }
    match (vob, ext) {
        (0, "IFO") => Some(DvdFileKind::Vtsi(ts)),
        (0, "VOB") => Some(DvdFileKind::VtsMenu(ts)),
        (0, "BUP") => Some(DvdFileKind::VtsiBup(ts)),
        (1..=9, "VOB") => Some(DvdFileKind::VtsTitle { ts, vob }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_udf_file(name: &str) -> UdfFile {
        UdfFile {
            name: name.to_string(),
            is_dir: false,
            extents: Vec::new(),
            length: 0,
            icb: crate::udf::LongAd {
                length: 0,
                extent_type: 0,
                location: crate::udf::LbAddr {
                    block: 0,
                    partition_ref: 0,
                },
                implementation_use: [0; 6],
            },
        }
    }

    #[test]
    fn classify_vmgi_files() {
        assert_eq!(
            classify_video_ts_file(&fake_udf_file("VIDEO_TS.IFO")),
            Some(DvdFileKind::Vmgi)
        );
        assert_eq!(
            classify_video_ts_file(&fake_udf_file("video_ts.bup")),
            Some(DvdFileKind::VmgiBup)
        );
        assert_eq!(
            classify_video_ts_file(&fake_udf_file("VIDEO_TS.VOB")),
            Some(DvdFileKind::VmgMenu)
        );
    }

    #[test]
    fn classify_vts_files() {
        assert_eq!(
            classify_video_ts_file(&fake_udf_file("VTS_01_0.IFO")),
            Some(DvdFileKind::Vtsi(1))
        );
        assert_eq!(
            classify_video_ts_file(&fake_udf_file("VTS_07_0.VOB")),
            Some(DvdFileKind::VtsMenu(7))
        );
        assert_eq!(
            classify_video_ts_file(&fake_udf_file("VTS_99_9.VOB")),
            Some(DvdFileKind::VtsTitle { ts: 99, vob: 9 })
        );
        assert_eq!(
            classify_video_ts_file(&fake_udf_file("VTS_03_0.BUP")),
            Some(DvdFileKind::VtsiBup(3))
        );
    }

    #[test]
    fn classify_rejects_garbage() {
        assert_eq!(classify_video_ts_file(&fake_udf_file("VTS_00_0.IFO")), None);
        assert_eq!(classify_video_ts_file(&fake_udf_file("VTS_AB_0.IFO")), None);
        assert_eq!(classify_video_ts_file(&fake_udf_file("VTS_01_0.XYZ")), None);
        assert_eq!(classify_video_ts_file(&fake_udf_file("MENU.IFO")), None);
    }

    #[test]
    fn classify_extracts_title_set_count() {
        let video_ts = vec![
            fake_udf_file("VIDEO_TS.IFO"),
            fake_udf_file("VTS_01_0.IFO"),
            fake_udf_file("VTS_01_1.VOB"),
            fake_udf_file("VTS_02_0.IFO"),
            fake_udf_file("VTS_02_1.VOB"),
        ];
        let disc = DvdDisc::classify("DISC".to_string(), &video_ts, &[]).unwrap();
        assert_eq!(disc.title_set_count, 2);
        assert_eq!(disc.video_ts_files.len(), 5);
        assert!(disc.vmgi().is_some());
        assert!(disc.vts_title_vob(1, 1).is_some());
        assert!(disc.vts_title_vob(2, 1).is_some());
    }

    #[test]
    fn classify_rejects_when_vmgi_missing() {
        let video_ts = vec![fake_udf_file("VTS_01_0.IFO"), fake_udf_file("VTS_01_1.VOB")];
        let err = DvdDisc::classify("DISC".to_string(), &video_ts, &[]).unwrap_err();
        match err {
            Error::NotDvdVideo(_) => {}
            other => panic!("expected NotDvdVideo, got {other:?}"),
        }
    }

    #[test]
    fn classify_handles_empty_audio_ts() {
        let video_ts = vec![
            fake_udf_file("VIDEO_TS.IFO"),
            fake_udf_file("VIDEO_TS.BUP"),
            fake_udf_file("VTS_01_0.IFO"),
            fake_udf_file("VTS_01_1.VOB"),
            fake_udf_file("VTS_01_0.BUP"),
        ];
        let disc = DvdDisc::classify("EMPTY_AUDIO_TS".to_string(), &video_ts, &[]).unwrap();
        assert!(disc.audio_ts_files.is_empty());
        assert_eq!(disc.title_set_count, 1);
    }
}
