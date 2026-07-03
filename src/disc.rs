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
use crate::ifo::{
    DvdTitleEntry, Pgc, PgciUt, TtSrpt, VmgIfo, VmgPtlMait, VmgVtsAtrt, VobuAdmap, VtsCAdt, VtsIfo,
};
use crate::iso9660::Iso9660Volume;
use crate::udf::{UdfFile, UdfVolume};

/// Top-level kind of a DVD-Video file. The discriminator is purely
/// lexical (filename pattern matching) per the spec naming
/// convention in `docs/container/dvd/application/mpucoder-ifo.html`;
/// we don't peek inside any of the bytes.
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

    /// Read `VIDEO_TS.IFO`'s [`VmgVtsAtrt`] — the per-VTS attribute
    /// table that mirrors each VTS IFO's attribute block (`0x100..`)
    /// onto the VMG side, so a player can answer per-VTS attribute
    /// queries without opening every `VTS_xx_0.IFO` individually.
    ///
    /// Returns `Ok(None)` when the MAT's `vts_atrt_sector` is zero
    /// (the spec allows the table to be elided when no VTSs are
    /// authored — extremely rare in practice).
    pub fn parse_vmg_vts_atrt<R: Read + Seek>(&self, reader: &mut R) -> Result<Option<VmgVtsAtrt>> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        if mat.vts_atrt_sector == 0 {
            return Ok(None);
        }
        // VTS_ATRT can span multiple sectors on discs with many VTSs
        // (99 entries × ~0x308 bytes ≈ 76 KB ≈ 38 sectors). Bound the
        // read at the IFO file's tail to avoid pulling past EOF.
        let max_sectors = ((mat.last_sector_ifo + 1).saturating_sub(mat.vts_atrt_sector)).max(1);
        let buf = read_sector_range(reader, f.lba + mat.vts_atrt_sector, max_sectors)?;
        VmgVtsAtrt::parse(&buf).map(Some)
    }

    /// Read `VIDEO_TS.IFO`'s [`VmgPtlMait`] — the parental management
    /// table that lists, per country, the 16-bit allow-mask of each
    /// title set at each of the 8 parental levels.
    ///
    /// Returns `Ok(None)` when the MAT's `ptl_mait_sector` is zero
    /// (no parental management on this disc — common on unrated /
    /// region-free authoring).
    pub fn parse_vmg_ptl_mait<R: Read + Seek>(&self, reader: &mut R) -> Result<Option<VmgPtlMait>> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        if mat.ptl_mait_sector == 0 {
            return Ok(None);
        }
        // PTL_MAIT is bounded similarly to VTS_ATRT — sized by
        // (num_countries × 8 + num_title_sets-derived body) but
        // capped by the IFO file's tail. Read up to the next-table
        // boundary so we don't pull garbage from a different table.
        let next_table_sector = [
            mat.vts_atrt_sector,
            mat.txtdt_mg_sector,
            mat.vmgm_c_adt_sector,
            mat.vmgm_vobu_admap_sector,
            mat.last_sector_ifo + 1,
        ]
        .iter()
        .copied()
        .filter(|s| *s > mat.ptl_mait_sector)
        .min()
        .unwrap_or(mat.last_sector_ifo + 1);
        let max_sectors = next_table_sector.saturating_sub(mat.ptl_mait_sector).max(1);
        let buf = read_sector_range(reader, f.lba + mat.ptl_mait_sector, max_sectors)?;
        VmgPtlMait::parse(&buf).map(Some)
    }

    /// Read `VIDEO_TS.IFO`'s **First-Play PGC** (`FP_PGC`) — the
    /// program chain a player enters when the disc is inserted,
    /// before any title or menu domain is active.
    ///
    /// Per `docs/container/dvd/application/mpucoder-ifo.html` the
    /// VMGI_MAT word at `0x0084` is the *start byte address* of
    /// `FP_PGC` (same byte-address unit as the `0x0080`
    /// "end byte address of VMGI_MAT" word — relative to the start of
    /// `VIDEO_TS.IFO`), and the body is an ordinary PGC per
    /// `mpucoder-pgc.html` (the MAT row links straight to the PGC
    /// page), so [`Pgc::parse`] decodes it unchanged. On commercial
    /// discs the FP_PGC typically carries no cells — just a
    /// pre-command list ending in a `JumpSS` / `JumpTT`, which is the
    /// disc's startup routing; feed [`Pgc::commands`]`.pre` through
    /// [`crate::Vm::run_list`] to obtain the first [`crate::VmAction`].
    ///
    /// Returns `Ok(None)` when the MAT's `fp_pgc_addr` is zero (no
    /// First-Play PGC authored — the spec marks the field optional).
    pub fn parse_fp_pgc<R: Read + Seek>(&self, reader: &mut R) -> Result<Option<Pgc>> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        if mat.fp_pgc_addr == 0 {
            return Ok(None);
        }
        // FP_PGC lives inside the IFO between the MAT and the first
        // sector-aligned table (it is the only VMGI structure
        // addressed in bytes rather than sectors). Bound the read at
        // the first non-zero table sector so a malformed address
        // can't pull bytes from an unrelated table; fall back to the
        // IFO file's last sector.
        let first_table_sector = [
            mat.tt_srpt_sector,
            mat.vmgm_pgci_ut_sector,
            mat.ptl_mait_sector,
            mat.vts_atrt_sector,
            mat.txtdt_mg_sector,
            mat.vmgm_c_adt_sector,
            mat.vmgm_vobu_admap_sector,
        ]
        .iter()
        .copied()
        .filter(|s| *s != 0)
        .min()
        .unwrap_or(mat.last_sector_ifo + 1)
        .min(mat.last_sector_ifo + 1);
        let start = mat.fp_pgc_addr as usize;
        let end = first_table_sector as usize * crate::ifo::DVD_SECTOR;
        if start >= end {
            return Err(Error::InvalidUdf(
                "VMGI_MAT: fp_pgc_addr points past the first VMG table",
            ));
        }
        let buf = read_sector_range(reader, f.lba, first_table_sector.max(1))?;
        Pgc::parse(&buf[start..]).map(Some)
    }

    /// Compute the static, command-free cell schedule of the 1-based
    /// volume-wide title `ttn` for camera angle `angle` — TT_SRPT
    /// lookup, title-set IFO parse, and
    /// [`crate::engine::plan_title_cells`] composed into one call.
    ///
    /// The returned [`TitlePlan`] carries the angle-resolved
    /// [`crate::engine::PlannedCell`] rows (VOB-relative sector
    /// spans) plus the addressing context —
    /// `TT_SRPT::vts_start_sector` and `VTSI_MAT::title_vob_sector`
    /// — so [`TitlePlan::absolute_lba`] can turn each span into a
    /// disc-absolute LBA. Navigation commands / stills / menus are
    /// deliberately not executed; the interactive path is
    /// [`crate::engine::PgcRunner`].
    pub fn plan_title<R: Read + Seek>(
        &self,
        reader: &mut R,
        ttn: u8,
        angle: u8,
    ) -> Result<TitlePlan> {
        let srpt = self.parse_vmg_tt_srpt(reader)?;
        let entry = *srpt
            .title(ttn)
            .ok_or(Error::InvalidUdf("TT_SRPT: no such title"))?;
        let vts = self.parse_vts(reader, entry.vts_number)?;
        let cells = crate::engine::plan_title_cells(
            &vts.pgci_srp,
            &vts.pgcs,
            entry.vts_title_number,
            angle,
        )
        .ok_or(Error::InvalidUdf("VTS_PGCI: no entry PGC for this title"))?;
        Ok(TitlePlan {
            ttn,
            vts: entry.vts_number,
            vts_ttn: entry.vts_title_number,
            vts_start_sector: entry.vts_start_sector,
            title_vob_sector: vts.mat.title_vob_sector,
            cells,
        })
    }

    /// Read `VIDEO_TS.IFO`'s [`PgciUt`] — the VMGM (First-Play / VMG
    /// menu) program-chain table indexed by ISO 639 language unit.
    ///
    /// Per `docs/container/dvd/application/mpucoder-ifo_vmg.html`
    /// §VMGM_PGCI_UT — two-level hierarchy: outer search-pointer list
    /// keyed by language, each pointing at a Language Unit (`PGCI_LU`)
    /// that lists the per-PGC search pointers + the PGC bodies
    /// themselves (parsed via [`crate::ifo::Pgc::parse`]).
    ///
    /// Returns `Ok(None)` when the MAT's `vmgm_pgci_ut_sector` is zero
    /// (no VMG menu authored — possible on minimal discs).
    pub fn parse_vmgm_pgci_ut<R: Read + Seek>(&self, reader: &mut R) -> Result<Option<PgciUt>> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        if mat.vmgm_pgci_ut_sector == 0 {
            return Ok(None);
        }
        // Bound the read at the next non-zero VMG table boundary so
        // a malformed `end_address` field can't pull bytes from an
        // unrelated table. Falls back to the IFO file's last sector.
        let next_table_sector = [
            mat.ptl_mait_sector,
            mat.vts_atrt_sector,
            mat.txtdt_mg_sector,
            mat.vmgm_c_adt_sector,
            mat.vmgm_vobu_admap_sector,
            mat.last_sector_ifo + 1,
        ]
        .iter()
        .copied()
        .filter(|s| *s > mat.vmgm_pgci_ut_sector)
        .min()
        .unwrap_or(mat.last_sector_ifo + 1);
        let max_sectors = next_table_sector
            .saturating_sub(mat.vmgm_pgci_ut_sector)
            .max(1);
        let buf = read_sector_range(reader, f.lba + mat.vmgm_pgci_ut_sector, max_sectors)?;
        PgciUt::parse(&buf).map(Some)
    }

    /// Read `VTS_xx_0.IFO`'s [`PgciUt`] — the VTSM (per-title-set
    /// menu) program-chain table indexed by ISO 639 language unit.
    ///
    /// Per `docs/container/dvd/application/mpucoder-ifo_vts.html`
    /// §VTSM_PGCI_UT — same shape as the VMG side except the per-LU
    /// search-pointer carries a richer menu-existence flag byte
    /// (root / sub-picture / audio / angle / PTT — see
    /// [`crate::ifo::menu_existence`]).
    ///
    /// Returns `Ok(None)` when the VTSI_MAT's `vtsm_pgci_ut_sector`
    /// is zero (no per-title-set menus authored on this title set).
    pub fn parse_vtsm_pgci_ut<R: Read + Seek>(
        &self,
        reader: &mut R,
        ts_index: u8,
    ) -> Result<Option<PgciUt>> {
        let f = self
            .vtsi(ts_index)
            .ok_or(Error::NotDvdVideo("VTS_xx_0.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = crate::ifo::VtsiMat::parse(&mat_buf)?;
        if mat.vtsm_pgci_ut_sector == 0 {
            return Ok(None);
        }
        // Same bounded-read pattern: clip at the next non-zero VTS
        // table boundary so a malformed length field is harmless.
        let next_table_sector = [
            mat.vts_pgci_sector,
            mat.vts_tmapti_sector,
            mat.vtsm_c_adt_sector,
            mat.vtsm_vobu_admap_sector,
            mat.vts_c_adt_sector,
            mat.vts_vobu_admap_sector,
            mat.last_sector_ifo + 1,
        ]
        .iter()
        .copied()
        .filter(|s| *s > mat.vtsm_pgci_ut_sector)
        .min()
        .unwrap_or(mat.last_sector_ifo + 1);
        let max_sectors = next_table_sector
            .saturating_sub(mat.vtsm_pgci_ut_sector)
            .max(1);
        let buf = read_sector_range(reader, f.lba + mat.vtsm_pgci_ut_sector, max_sectors)?;
        PgciUt::parse(&buf).map(Some)
    }

    /// Read `VIDEO_TS.IFO`'s [`VtsCAdt`] for the VMG menu — the
    /// `VMGM_C_ADT` cell-address table that maps each `(VOBidn,
    /// CELLidn)` pair to its `[start_sector, end_sector]` range inside
    /// the VMG menu VOB (`VIDEO_TS.VOB`).
    ///
    /// Per `docs/container/dvd/application/mpucoder-ifo.html` §c_adt —
    /// the `#c_adt` anchor documents `VMGM_C_ADT`, `VTSM_C_ADT`, and
    /// `VTS_C_ADT` under one heading because all three share the wire
    /// format (16-bit number-of-VOB-IDs + 32-bit end-address header
    /// followed by 12-byte entries), so [`VtsCAdt::parse`] decodes the
    /// VMG menu copy unchanged.
    ///
    /// Returns `Ok(None)` when the MAT's `vmgm_c_adt_sector` is zero
    /// (no VMG menu VOB authored on this disc).
    pub fn parse_vmgm_c_adt<R: Read + Seek>(&self, reader: &mut R) -> Result<Option<VtsCAdt>> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        if mat.vmgm_c_adt_sector == 0 {
            return Ok(None);
        }
        // Bound the read at the next non-zero VMG table boundary so a
        // malformed `end_address` can't pull bytes from an unrelated
        // table.
        let next_table_sector = [mat.vmgm_vobu_admap_sector, mat.last_sector_ifo + 1]
            .iter()
            .copied()
            .filter(|s| *s > mat.vmgm_c_adt_sector)
            .min()
            .unwrap_or(mat.last_sector_ifo + 1);
        let max_sectors = next_table_sector
            .saturating_sub(mat.vmgm_c_adt_sector)
            .max(1);
        let buf = read_sector_range(reader, f.lba + mat.vmgm_c_adt_sector, max_sectors)?;
        VtsCAdt::parse(&buf).map(Some)
    }

    /// Read `VIDEO_TS.IFO`'s [`VobuAdmap`] for the VMG menu — the
    /// `VMGM_VOBU_ADMAP` absolute-sector list covering every VOBU in
    /// the VMG menu VOB.
    ///
    /// Per `docs/container/dvd/application/mpucoder-ifo.html` §vam —
    /// the `#vam` anchor documents `VMGM_VOBU_ADMAP`,
    /// `VTSM_VOBU_ADMAP`, and `VTS_VOBU_ADMAP` under one heading
    /// because all three share the wire format (32-bit end-address
    /// header followed by 4-byte VOB-relative sector words), so
    /// [`VobuAdmap::parse`] decodes the VMG menu copy unchanged.
    ///
    /// Returns `Ok(None)` when the MAT's `vmgm_vobu_admap_sector` is
    /// zero (no VMG menu VOB authored on this disc).
    pub fn parse_vmgm_vobu_admap<R: Read + Seek>(
        &self,
        reader: &mut R,
    ) -> Result<Option<VobuAdmap>> {
        let f = self
            .vmgi()
            .ok_or(Error::NotDvdVideo("VIDEO_TS.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = VmgIfo::parse(&mat_buf)?;
        if mat.vmgm_vobu_admap_sector == 0 {
            return Ok(None);
        }
        // VMGM_VOBU_ADMAP is the last VMG table; bound at the IFO
        // file's tail.
        let max_sectors = (mat.last_sector_ifo + 1)
            .saturating_sub(mat.vmgm_vobu_admap_sector)
            .max(1);
        let buf = read_sector_range(reader, f.lba + mat.vmgm_vobu_admap_sector, max_sectors)?;
        VobuAdmap::parse(&buf).map(Some)
    }

    /// Read `VTS_xx_0.IFO`'s [`VtsCAdt`] for the per-title-set menu —
    /// the `VTSM_C_ADT` cell-address table that maps each `(VOBidn,
    /// CELLidn)` pair to its `[start_sector, end_sector]` range inside
    /// the VTS menu VOB (`VTS_xx_0.VOB`).
    ///
    /// Per `docs/container/dvd/application/mpucoder-ifo.html` §c_adt
    /// (same shared heading as the VMG menu copy; see
    /// [`Self::parse_vmgm_c_adt`]).
    ///
    /// Returns `Ok(None)` when the VTSI_MAT's `vtsm_c_adt_sector` is
    /// zero (no per-title-set menu VOB authored on this title set).
    pub fn parse_vtsm_c_adt<R: Read + Seek>(
        &self,
        reader: &mut R,
        ts_index: u8,
    ) -> Result<Option<VtsCAdt>> {
        let f = self
            .vtsi(ts_index)
            .ok_or(Error::NotDvdVideo("VTS_xx_0.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = crate::ifo::VtsiMat::parse(&mat_buf)?;
        if mat.vtsm_c_adt_sector == 0 {
            return Ok(None);
        }
        // Bound at the next non-zero VTS table boundary after the
        // menu C_ADT so a malformed length field is harmless.
        let next_table_sector = [
            mat.vtsm_vobu_admap_sector,
            mat.vts_c_adt_sector,
            mat.vts_vobu_admap_sector,
            mat.last_sector_ifo + 1,
        ]
        .iter()
        .copied()
        .filter(|s| *s > mat.vtsm_c_adt_sector)
        .min()
        .unwrap_or(mat.last_sector_ifo + 1);
        let max_sectors = next_table_sector
            .saturating_sub(mat.vtsm_c_adt_sector)
            .max(1);
        let buf = read_sector_range(reader, f.lba + mat.vtsm_c_adt_sector, max_sectors)?;
        VtsCAdt::parse(&buf).map(Some)
    }

    /// Read `VTS_xx_0.IFO`'s [`VobuAdmap`] for the per-title-set menu —
    /// the `VTSM_VOBU_ADMAP` absolute-sector list covering every VOBU
    /// in the VTS menu VOB.
    ///
    /// Per `docs/container/dvd/application/mpucoder-ifo.html` §vam
    /// (same shared heading as the VMG menu copy; see
    /// [`Self::parse_vmgm_vobu_admap`]).
    ///
    /// Returns `Ok(None)` when the VTSI_MAT's `vtsm_vobu_admap_sector`
    /// is zero (no per-title-set menu VOB authored on this title set).
    pub fn parse_vtsm_vobu_admap<R: Read + Seek>(
        &self,
        reader: &mut R,
        ts_index: u8,
    ) -> Result<Option<VobuAdmap>> {
        let f = self
            .vtsi(ts_index)
            .ok_or(Error::NotDvdVideo("VTS_xx_0.IFO absent"))?;
        let mat_buf = read_sector_range(reader, f.lba, 1)?;
        let mat = crate::ifo::VtsiMat::parse(&mat_buf)?;
        if mat.vtsm_vobu_admap_sector == 0 {
            return Ok(None);
        }
        // Bound at the next non-zero VTS table boundary after the
        // menu VOBU_ADMAP so a malformed length field is harmless.
        let next_table_sector = [
            mat.vts_c_adt_sector,
            mat.vts_vobu_admap_sector,
            mat.last_sector_ifo + 1,
        ]
        .iter()
        .copied()
        .filter(|s| *s > mat.vtsm_vobu_admap_sector)
        .min()
        .unwrap_or(mat.last_sector_ifo + 1);
        let max_sectors = next_table_sector
            .saturating_sub(mat.vtsm_vobu_admap_sector)
            .max(1);
        let buf = read_sector_range(reader, f.lba + mat.vtsm_vobu_admap_sector, max_sectors)?;
        VobuAdmap::parse(&buf).map(Some)
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

/// The result of [`DvdDisc::plan_title`] — one title's static cell
/// schedule plus the addressing context that turns its VOB-relative
/// sector spans into disc-absolute LBAs.
#[derive(Debug, Clone)]
pub struct TitlePlan {
    /// 1-based volume-wide title number (SPRM 4).
    pub ttn: u8,
    /// 1-based title set the title lives in.
    pub vts: u8,
    /// 1-based title number within that VTS (SPRM 5).
    pub vts_ttn: u8,
    /// Disc-absolute start sector of the title set (TT_SRPT entry).
    pub vts_start_sector: u32,
    /// Start sector of the title-content VOBs relative to the title
    /// set (`VTSI_MAT` offset `0x00C4`).
    pub title_vob_sector: u32,
    /// The angle-resolved cell schedule in presentation order;
    /// sector spans are relative to the title-content VOB set.
    pub cells: Vec<crate::engine::PlannedCell>,
}

impl TitlePlan {
    /// Translate a VOB-relative sector (a [`crate::engine::PlannedCell`]
    /// span endpoint) into a disc-absolute LBA:
    /// `vts_start_sector + title_vob_sector + vob_relative_sector`.
    pub fn absolute_lba(&self, vob_relative_sector: u32) -> u32 {
        self.vts_start_sector
            .saturating_add(self.title_vob_sector)
            .saturating_add(vob_relative_sector)
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

    // ---- menu C_ADT / VOBU_ADMAP reader helpers --------------------
    //
    // The body decoders are exercised in `ifo.rs`; these tests pin the
    // sector-pointer plumbing of the four `DvdDisc::parse_*` helpers
    // (zero-pointer → None, non-zero → followed + parsed).

    use crate::ifo::{DVD_SECTOR, VMG_MAGIC, VTS_MAGIC};
    use std::io::Cursor;

    /// A C_ADT body per `mpucoder-ifo.html` §c_adt: 2-byte
    /// number-of-VOB-IDs + 2-byte reserved + 4-byte end_address, then
    /// 12-byte entries (VOBidn:2 / CELLidn:1 / reserved:1 /
    /// start_sector:4 / end_sector:4). One entry here.
    fn synth_c_adt() -> Vec<u8> {
        let mut b = vec![0u8; DVD_SECTOR];
        b[0..2].copy_from_slice(&1u16.to_be_bytes()); // number of VOB IDs
        b[4..8].copy_from_slice(&19u32.to_be_bytes()); // end_address = 8 + 12 - 1
                                                       // entry 0
        b[8..10].copy_from_slice(&7u16.to_be_bytes()); // VOBidn
        b[10] = 3; // CELLidn
        b[12..16].copy_from_slice(&100u32.to_be_bytes()); // start sector
        b[16..20].copy_from_slice(&199u32.to_be_bytes()); // end sector
        b
    }

    /// A VOBU_ADMAP body per `mpucoder-ifo.html` §vam: 4-byte
    /// end_address then 4-byte VOB-relative sector words. Two entries.
    fn synth_vobu_admap() -> Vec<u8> {
        let mut b = vec![0u8; DVD_SECTOR];
        b[0..4].copy_from_slice(&11u32.to_be_bytes()); // end_address = 4 + 2*4 - 1
        b[4..8].copy_from_slice(&0u32.to_be_bytes()); // VOBU 1 @ sector 0
        b[8..12].copy_from_slice(&50u32.to_be_bytes()); // VOBU 2 @ sector 50
        b
    }

    /// Build a single-VMGI / single-VTSI disc image laid out as:
    /// VMGI MAT @ lba 0 (FP_PGC at byte 0x0400, inside sector 0),
    /// VMGM_C_ADT @ 1, VMGM_VOBU_ADMAP @ 2, VTSI MAT @ 4,
    /// VTSM_C_ADT @ 5, VTSM_VOBU_ADMAP @ 6. Zero pointers are
    /// written when `populate` is false to drive the `None` paths.
    fn synth_disc(populate: bool) -> (DvdDisc, Cursor<Vec<u8>>) {
        let mut image = vec![0u8; DVD_SECTOR * 8];

        // VMGI MAT @ sector 0.
        let vmgi = &mut image[0..DVD_SECTOR];
        vmgi[0..12].copy_from_slice(VMG_MAGIC);
        vmgi[0x1C..0x20].copy_from_slice(&3u32.to_be_bytes()); // last_sector_ifo
        if populate {
            vmgi[0x84..0x88].copy_from_slice(&0x0400u32.to_be_bytes()); // FP_PGC byte addr
            vmgi[0xD8..0xDC].copy_from_slice(&1u32.to_be_bytes()); // VMGM_C_ADT
            vmgi[0xDC..0xE0].copy_from_slice(&2u32.to_be_bytes()); // VMGM_VOBU_ADMAP

            // FP_PGC body @ byte 0x0400 (still sector 0, after the
            // 0x0200-byte MAT region): a cell-less PGC whose only
            // content is a one-entry pre-command list — `JumpTT 1`,
            // the canonical "skip straight to the main feature"
            // startup routing.
            vmgi[0x0400 + 0xE4..0x0400 + 0xE6].copy_from_slice(&0x00ECu16.to_be_bytes());
            let ct = 0x0400 + 0xEC; // command table, right after the header
            vmgi[ct..ct + 2].copy_from_slice(&1u16.to_be_bytes()); // pre count
            vmgi[ct + 6..ct + 8].copy_from_slice(&15u16.to_be_bytes()); // end address
            vmgi[ct + 8..ct + 16].copy_from_slice(&[0x30, 0x02, 0, 0, 0, 0x01, 0, 0]);
        }
        image[DVD_SECTOR..2 * DVD_SECTOR].copy_from_slice(&synth_c_adt());
        image[2 * DVD_SECTOR..3 * DVD_SECTOR].copy_from_slice(&synth_vobu_admap());

        // VTSI MAT @ sector 4.
        let vtsi = &mut image[4 * DVD_SECTOR..5 * DVD_SECTOR];
        vtsi[0..12].copy_from_slice(VTS_MAGIC);
        vtsi[0x1C..0x20].copy_from_slice(&3u32.to_be_bytes()); // last_sector_ifo
        if populate {
            vtsi[0xD8..0xDC].copy_from_slice(&1u32.to_be_bytes()); // VTSM_C_ADT
            vtsi[0xDC..0xE0].copy_from_slice(&2u32.to_be_bytes()); // VTSM_VOBU_ADMAP
        }
        image[5 * DVD_SECTOR..6 * DVD_SECTOR].copy_from_slice(&synth_c_adt());
        image[6 * DVD_SECTOR..7 * DVD_SECTOR].copy_from_slice(&synth_vobu_admap());

        let disc = DvdDisc {
            volume_id: "SYNTH".to_string(),
            title_set_count: 1,
            video_ts_files: vec![
                DvdFile {
                    kind: DvdFileKind::Vmgi,
                    name: "VIDEO_TS.IFO".to_string(),
                    lba: 0,
                    size: DVD_SECTOR as u64,
                    title_set: 0,
                    vob_index: 0,
                },
                DvdFile {
                    kind: DvdFileKind::Vtsi(1),
                    name: "VTS_01_0.IFO".to_string(),
                    lba: 4,
                    size: DVD_SECTOR as u64,
                    title_set: 1,
                    vob_index: 0,
                },
            ],
            audio_ts_files: Vec::new(),
        };
        (disc, Cursor::new(image))
    }

    /// Build a disc image with a *complete* one-title VTS so the
    /// full TT_SRPT → VTSI → VTS_PGCI → plan pipeline runs:
    /// VMGI MAT @ 0 (TT_SRPT @ 3), TT_SRPT @ 3,
    /// VTSI MAT @ 4 (PTT_SRPT @ +1, PGCI @ +2, C_ADT @ +3).
    fn synth_title_disc() -> (DvdDisc, Cursor<Vec<u8>>) {
        let mut image = vec![0u8; DVD_SECTOR * 8];

        // VMGI MAT @ sector 0.
        image[0..12].copy_from_slice(VMG_MAGIC);
        image[0x1C..0x20].copy_from_slice(&3u32.to_be_bytes()); // last_sector_ifo
        image[0xC4..0xC8].copy_from_slice(&3u32.to_be_bytes()); // TT_SRPT @ 3

        // TT_SRPT @ sector 3: one title → VTS 1 / VTS_TTN 1, VTS
        // starts at disc LBA 4.
        {
            let t = &mut image[3 * DVD_SECTOR..4 * DVD_SECTOR];
            t[0..2].copy_from_slice(&1u16.to_be_bytes()); // title count
            t[4..8].copy_from_slice(&19u32.to_be_bytes()); // end address
            t[8] = 0; // title_type
            t[9] = 1; // angle count
            t[10..12].copy_from_slice(&1u16.to_be_bytes()); // chapters
            t[14] = 1; // VTS number
            t[15] = 1; // VTS title number
            t[16..20].copy_from_slice(&4u32.to_be_bytes()); // VTS start sector
        }

        // VTSI MAT @ sector 4.
        {
            let m = &mut image[4 * DVD_SECTOR..5 * DVD_SECTOR];
            m[0..12].copy_from_slice(VTS_MAGIC);
            m[0x1C..0x20].copy_from_slice(&3u32.to_be_bytes()); // last_sector_ifo
            m[0xC4..0xC8].copy_from_slice(&100u32.to_be_bytes()); // title VOB start
            m[0xC8..0xCC].copy_from_slice(&1u32.to_be_bytes()); // PTT_SRPT @ +1
            m[0xCC..0xD0].copy_from_slice(&2u32.to_be_bytes()); // PGCI @ +2
            m[0xE0..0xE4].copy_from_slice(&3u32.to_be_bytes()); // C_ADT @ +3
        }

        // VTS_PTT_SRPT @ sector 5: 1 title, 1 chapter → (PGC 1, PG 1).
        {
            let t = &mut image[5 * DVD_SECTOR..6 * DVD_SECTOR];
            t[0..2].copy_from_slice(&1u16.to_be_bytes());
            t[4..8].copy_from_slice(&15u32.to_be_bytes()); // end address
            t[8..12].copy_from_slice(&12u32.to_be_bytes()); // offset to PTT[1]
            t[12..14].copy_from_slice(&1u16.to_be_bytes()); // pgcn
            t[14..16].copy_from_slice(&1u16.to_be_bytes()); // pgn
        }

        // VTS_PGCI @ sector 6: one entry PGC for title 1, two cells.
        {
            let g = &mut image[6 * DVD_SECTOR..7 * DVD_SECTOR];
            g[0..2].copy_from_slice(&1u16.to_be_bytes()); // 1 PGC
            g[8..12].copy_from_slice(&0x8100_0000u32.to_be_bytes()); // entry, title 1
            g[12..16].copy_from_slice(&16u32.to_be_bytes()); // PGC body @ 16
            let b = 16;
            g[b + 0x02] = 1; // programs
            g[b + 0x03] = 2; // cells
            let off_pmap = 0xECu16;
            let off_cpbi = 0xEE_u16; // 1-byte map padded to 2
            let off_cpos = off_cpbi + 48;
            g[b + 0xE6..b + 0xE8].copy_from_slice(&off_pmap.to_be_bytes());
            g[b + 0xE8..b + 0xEA].copy_from_slice(&off_cpbi.to_be_bytes());
            g[b + 0xEA..b + 0xEC].copy_from_slice(&off_cpos.to_be_bytes());
            g[b + usize::from(off_pmap)] = 1; // program 1 → cell 1
            for (i, (first, last)) in [(0u32, 99u32), (100, 199)].iter().enumerate() {
                let c = b + usize::from(off_cpbi) + i * 24;
                g[c + 8..c + 12].copy_from_slice(&first.to_be_bytes());
                g[c + 20..c + 24].copy_from_slice(&last.to_be_bytes());
            }
            for i in 0..2usize {
                let cp = b + usize::from(off_cpos) + i * 4;
                g[cp..cp + 2].copy_from_slice(&1u16.to_be_bytes()); // VOB id
                g[cp + 3] = (i + 1) as u8; // cell id
            }
        }

        // VTS_C_ADT @ sector 7.
        image[7 * DVD_SECTOR..8 * DVD_SECTOR].copy_from_slice(&synth_c_adt());

        let disc = DvdDisc {
            volume_id: "SYNTH-TITLE".to_string(),
            title_set_count: 1,
            video_ts_files: vec![
                DvdFile {
                    kind: DvdFileKind::Vmgi,
                    name: "VIDEO_TS.IFO".to_string(),
                    lba: 0,
                    size: DVD_SECTOR as u64 * 4,
                    title_set: 0,
                    vob_index: 0,
                },
                DvdFile {
                    kind: DvdFileKind::Vtsi(1),
                    name: "VTS_01_0.IFO".to_string(),
                    lba: 4,
                    size: DVD_SECTOR as u64 * 4,
                    title_set: 1,
                    vob_index: 0,
                },
            ],
            audio_ts_files: Vec::new(),
        };
        (disc, Cursor::new(image))
    }

    #[test]
    fn plan_title_end_to_end() {
        let (disc, mut r) = synth_title_disc();
        let plan = disc.plan_title(&mut r, 1, 1).unwrap();
        assert_eq!(plan.ttn, 1);
        assert_eq!(plan.vts, 1);
        assert_eq!(plan.vts_ttn, 1);
        assert_eq!(plan.vts_start_sector, 4);
        assert_eq!(plan.title_vob_sector, 100);
        let flat: Vec<(u16, u8, u32, u32)> = plan
            .cells
            .iter()
            .map(|c| (c.pgcn, c.cell, c.first_sector, c.last_sector))
            .collect();
        assert_eq!(flat, vec![(1, 1, 0, 99), (1, 2, 100, 199)]);
        // Disc-absolute addressing: VTS @ 4 + title VOB @ 100.
        assert_eq!(plan.absolute_lba(plan.cells[0].first_sector), 104);
        assert_eq!(plan.absolute_lba(plan.cells[1].last_sector), 303);
    }

    #[test]
    fn plan_title_unknown_title_is_error() {
        let (disc, mut r) = synth_title_disc();
        assert!(disc.plan_title(&mut r, 2, 1).is_err());
        assert!(disc.plan_title(&mut r, 0, 1).is_err());
    }

    #[test]
    fn parse_vmgm_c_adt_populated() {
        let (disc, mut r) = synth_disc(true);
        let c_adt = disc.parse_vmgm_c_adt(&mut r).unwrap().unwrap();
        assert_eq!(c_adt.number_of_vob_ids, 1);
        assert_eq!(c_adt.entries.len(), 1);
        assert_eq!(c_adt.lookup(7, 3), Some((100, 199)));
    }

    #[test]
    fn parse_vmgm_vobu_admap_populated() {
        let (disc, mut r) = synth_disc(true);
        let admap = disc.parse_vmgm_vobu_admap(&mut r).unwrap().unwrap();
        assert_eq!(admap.vobu_count(), 2);
        assert_eq!(admap.vobu_start_sector(2), Some(50));
    }

    #[test]
    fn parse_vtsm_c_adt_populated() {
        let (disc, mut r) = synth_disc(true);
        let c_adt = disc.parse_vtsm_c_adt(&mut r, 1).unwrap().unwrap();
        assert_eq!(c_adt.entries.len(), 1);
        assert_eq!(c_adt.lookup(7, 3), Some((100, 199)));
    }

    #[test]
    fn parse_vtsm_vobu_admap_populated() {
        let (disc, mut r) = synth_disc(true);
        let admap = disc.parse_vtsm_vobu_admap(&mut r, 1).unwrap().unwrap();
        assert_eq!(admap.vobu_count(), 2);
        assert_eq!(admap.vobu_start_sector(1), Some(0));
    }

    #[test]
    fn menu_tables_none_when_pointer_zero() {
        let (disc, mut r) = synth_disc(false);
        assert!(disc.parse_vmgm_c_adt(&mut r).unwrap().is_none());
        assert!(disc.parse_vmgm_vobu_admap(&mut r).unwrap().is_none());
        assert!(disc.parse_vtsm_c_adt(&mut r, 1).unwrap().is_none());
        assert!(disc.parse_vtsm_vobu_admap(&mut r, 1).unwrap().is_none());
    }

    // ---- First-Play PGC reader helper ------------------------------

    #[test]
    fn parse_fp_pgc_populated_and_routes_through_vm() {
        let (disc, mut r) = synth_disc(true);
        let fp = disc.parse_fp_pgc(&mut r).unwrap().unwrap();

        // The synthetic FP_PGC is cell-less — startup routing only.
        assert_eq!(fp.number_of_programs, 0);
        assert_eq!(fp.number_of_cells, 0);
        let commands = fp.commands.as_ref().unwrap();
        assert_eq!(commands.pre.len(), 1);
        assert!(commands.post.is_empty());
        assert!(commands.cell.is_empty());

        // Drive the disc-insertion path end-to-end: FP_PGC
        // pre-commands through the Phase 3c VM must surface the
        // `JumpTT 1` startup routing as a typed action.
        let mut vm = crate::vm::Vm::new();
        let (action, _pc) = vm.run_list(&commands.pre);
        assert_eq!(action, crate::vm::VmAction::JumpTitle { ttn: 1 });
    }

    #[test]
    fn parse_fp_pgc_none_when_pointer_zero() {
        let (disc, mut r) = synth_disc(false);
        assert!(disc.parse_fp_pgc(&mut r).unwrap().is_none());
    }

    #[test]
    fn parse_fp_pgc_rejects_addr_past_first_table() {
        // Point fp_pgc_addr at the VMGM_C_ADT sector (byte 0x0800 =
        // sector 1) — the bounded read must refuse to parse a PGC out
        // of an unrelated table.
        let (disc, r) = synth_disc(true);
        let mut image = r.into_inner();
        image[0x84..0x88].copy_from_slice(&0x0800u32.to_be_bytes());
        let mut r = Cursor::new(image);
        assert!(disc.parse_fp_pgc(&mut r).is_err());
    }
}
