//! IFO body parser — Video Manager Information (VMGI), Video Title
//! Set Information (VTSI), Program Chain Information (PGCI), Title
//! Search Pointer Table (TT_SRPT), Part-of-Title Search Pointer Table
//! (VTS_PTT_SRPT), Cell Address Table (VTS_C_ADT).
//!
//! Phase 2: structural decoding of the IFO files only — title list,
//! chapter list, program-chain layout, cell sector ranges. No VOB
//! demuxing, no cell concatenation, no virtual-machine command
//! execution, no playback. Those are Phase 3.
//!
//! ## Sector / byte addressing convention
//!
//! IFO files are sequences of 2048-byte logical sectors. Field
//! offsets in `VMGI_MAT` / `VTSI_MAT` that are described as "sector
//! pointers" are sector indexes **relative to the start of the IFO
//! file**, not absolute disc-LBA values. The "start sector" fields
//! inside `TT_SRPT` entries, by contrast, are absolute disc LBAs
//! (referenced to the whole disc, where `VIDEO_TS.IFO` lives at LBA
//! 0 of that title set).
//!
//! All multi-byte integer fields are big-endian (network byte order).
//!
//! ## Clean-room references
//!
//! Material consulted while writing this module:
//!
//! - `docs/container/dvd/application/mpucoder-ifo.html`
//!   (VMGI_MAT / VTSI_MAT field layout, sector-pointer offsets,
//!   C_ADT / VOBU_ADMAP entry format).
//! - `docs/container/dvd/application/mpucoder-ifo_vmg.html`
//!   (TT_SRPT, VMGM_PGCI_UT, VMG_PTL_MAIT, VMG_VTS_ATRT).
//! - `docs/container/dvd/application/mpucoder-ifo_vts.html`
//!   (VTS_PTT_SRPT, VTS_PGCI, VTSM_PGCI_UT, VTS_TMAPTI).
//! - `docs/container/dvd/application/mpucoder-pgc.html`
//!   (PGC header at offset 0..0xEC, command table, program map,
//!   cell playback information table, cell position information).
//! - `docs/container/dvd/application/stnsoft-vmindx.html`
//!   (cross-reference for VTS_C_ADT entry layout).
//!
//! No external implementation source consulted at any point —
//! clean-room from the `docs/container/dvd/application/` references
//! listed above.

use crate::error::{Error, Result};

/// Logical-sector size on a DVD-ROM (per ECMA-267 §1.7).
pub const DVD_SECTOR: usize = 2048;

/// Magic at byte 0 of `VIDEO_TS.IFO`.
pub const VMG_MAGIC: &[u8; 12] = b"DVDVIDEO-VMG";

/// Magic at byte 0 of `VTS_xx_0.IFO`.
pub const VTS_MAGIC: &[u8; 12] = b"DVDVIDEO-VTS";

// ------------------------------------------------------------------
// Common helpers
// ------------------------------------------------------------------

fn read_u16(buf: &[u8], off: usize) -> Result<u16> {
    let slice = buf
        .get(off..off + 2)
        .ok_or(Error::InvalidUdf("ifo: u16 read past end"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn read_u32(buf: &[u8], off: usize) -> Result<u32> {
    let slice = buf
        .get(off..off + 4)
        .ok_or(Error::InvalidUdf("ifo: u32 read past end"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u8(buf: &[u8], off: usize) -> Result<u8> {
    buf.get(off)
        .copied()
        .ok_or(Error::InvalidUdf("ifo: u8 read past end"))
}

// ------------------------------------------------------------------
// PgcTime — BCD playback time + frame-rate bits
// ------------------------------------------------------------------

/// Playback time field used by `PGC_GI` and per-cell `C_PBI` entries.
///
/// Layout per mpucoder-pgc.html: 4 bytes — `hh:mm:ss:ff` in BCD, with
/// bits 7 & 6 of the last byte encoding the frame rate. `11b` = 30 fps
/// (NTSC drop / non-drop), `01b` = 25 fps (PAL). `10b` and `00b` are
/// declared illegal by the spec — we surface them as
/// [`FrameRate::Illegal`] rather than rejecting outright since some
/// authoring tools emit zero-time placeholder fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgcTime {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub frame_rate: FrameRate,
}

/// Frame-rate encoding used by [`PgcTime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRate {
    /// `00b` — illegal per spec, but present in some authoring outputs.
    Illegal,
    /// `01b` — 25 fps (PAL).
    Pal25,
    /// `10b` — illegal per spec.
    Reserved,
    /// `11b` — 30 fps (NTSC; the spec lumps drop/non-drop together).
    Ntsc30,
}

impl PgcTime {
    /// Decode a 4-byte BCD playback time field.
    pub fn from_bytes(bytes: [u8; 4]) -> Self {
        fn bcd(b: u8) -> u8 {
            ((b >> 4) & 0x0F) * 10 + (b & 0x0F)
        }
        let hours = bcd(bytes[0]);
        let minutes = bcd(bytes[1]);
        let seconds = bcd(bytes[2]);
        // Frames byte: bits 7+6 = frame-rate; bits 5..0 = BCD frames.
        // Per mpucoder-pgc.html the frames nibble pair is itself BCD,
        // but only the low 6 bits encode frames (so the tens digit is
        // 0..3, sufficient for max 29 frames at 30 fps).
        let frame_rate = match (bytes[3] >> 6) & 0x03 {
            0b00 => FrameRate::Illegal,
            0b01 => FrameRate::Pal25,
            0b10 => FrameRate::Reserved,
            0b11 => FrameRate::Ntsc30,
            _ => unreachable!(),
        };
        let f_lo = bytes[3] & 0x0F;
        let f_hi = (bytes[3] >> 4) & 0x03;
        let frames = f_hi * 10 + f_lo;
        Self {
            hours,
            minutes,
            seconds,
            frames,
            frame_rate,
        }
    }

    /// Total integer seconds (frames truncated, frame-rate ignored).
    /// Convenient when callers just need a "ballpark length" without
    /// the per-frame rounding.
    pub fn total_seconds(self) -> u32 {
        u32::from(self.hours) * 3600 + u32::from(self.minutes) * 60 + u32::from(self.seconds)
    }
}

// ------------------------------------------------------------------
// VMGI_MAT — Video Manager Information Management Table
// ------------------------------------------------------------------

/// Parsed VMGI_MAT (the first 0x200 bytes of `VIDEO_TS.IFO`).
///
/// Fields are surfaced in the order they appear in mpucoder-ifo.html's
/// "VMG IFO Contents" column. Sector-pointer fields are kept as-is
/// (`0` denotes "table absent" per spec).
#[derive(Debug, Clone)]
pub struct VmgIfo {
    /// Last sector of the VMG set (last sector of `VIDEO_TS.BUP`).
    pub last_sector_vmg_set: u32,
    /// Last sector of `VIDEO_TS.IFO`.
    pub last_sector_ifo: u32,
    /// VMGI version number, packed as `(major << 4) | minor` in the
    /// low byte of a 16-bit BE field (mpucoder-ifo.html "Version
    /// Number"). DVD-Video is typically `0x10` (1.0) or `0x11` (1.1).
    pub version: u16,
    /// VMG category (region mask in byte 1; rest reserved).
    pub vmg_category: u32,
    /// Number of volumes in this title set (e.g. 1 for single-volume
    /// discs; >1 for jukebox-style multi-side authoring).
    pub number_of_volumes: u16,
    /// Volume number (1-based) within the set above.
    pub volume_number: u16,
    /// Side ID (0 = side A, 1 = side B for double-sided discs).
    pub side_id: u8,
    /// Number of Video Title Sets (1..=99).
    pub number_of_title_sets: u16,
    /// Provider ID (32 ASCII bytes, NUL-padded).
    pub provider_id: String,
    /// Last byte address of `VMGI_MAT` itself.
    pub vmgi_mat_end: u32,
    /// Start address (byte offset) of First-Play PGC. `0` if absent.
    pub fp_pgc_addr: u32,
    /// Sector pointer to the VMG menu VOB (`0` if no menu).
    pub menu_vob_sector: u32,
    /// Sector pointer to TT_SRPT. Mandatory; always non-zero on a
    /// well-formed disc.
    pub tt_srpt_sector: u32,
    /// Sector pointer to VMGM_PGCI_UT. `0` if no VMG menu.
    pub vmgm_pgci_ut_sector: u32,
    /// Sector pointer to VMG_PTL_MAIT. `0` if no parental management.
    pub ptl_mait_sector: u32,
    /// Sector pointer to VMG_VTS_ATRT.
    pub vts_atrt_sector: u32,
    /// Sector pointer to VMG_TXTDT_MG (disc text data). `0` if absent.
    pub txtdt_mg_sector: u32,
    /// Sector pointer to VMGM_C_ADT (menu cell address table).
    pub vmgm_c_adt_sector: u32,
    /// Sector pointer to VMGM_VOBU_ADMAP (menu VOBU address map).
    pub vmgm_vobu_admap_sector: u32,
}

impl VmgIfo {
    /// Parse a `VIDEO_TS.IFO` byte buffer. The buffer must cover at
    /// least the VMGI_MAT region (the first 0x200 bytes).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 0x200 {
            return Err(Error::InvalidUdf("VMGI_MAT: buffer shorter than 0x200"));
        }
        if &buf[0..12] != VMG_MAGIC {
            return Err(Error::InvalidUdf("VMGI_MAT: bad magic"));
        }
        let last_sector_vmg_set = read_u32(buf, 0x000C)?;
        let last_sector_ifo = read_u32(buf, 0x001C)?;
        let version = read_u16(buf, 0x0020)?;
        let vmg_category = read_u32(buf, 0x0022)?;
        let number_of_volumes = read_u16(buf, 0x0026)?;
        let volume_number = read_u16(buf, 0x0028)?;
        let side_id = read_u8(buf, 0x002A)?;
        let number_of_title_sets = read_u16(buf, 0x003E)?;
        let provider_id_raw = &buf[0x0040..0x0060];
        let provider_id = decode_ascii_trim(provider_id_raw);
        let vmgi_mat_end = read_u32(buf, 0x0080)?;
        let fp_pgc_addr = read_u32(buf, 0x0084)?;
        let menu_vob_sector = read_u32(buf, 0x00C0)?;
        let tt_srpt_sector = read_u32(buf, 0x00C4)?;
        let vmgm_pgci_ut_sector = read_u32(buf, 0x00C8)?;
        let ptl_mait_sector = read_u32(buf, 0x00CC)?;
        let vts_atrt_sector = read_u32(buf, 0x00D0)?;
        let txtdt_mg_sector = read_u32(buf, 0x00D4)?;
        let vmgm_c_adt_sector = read_u32(buf, 0x00D8)?;
        let vmgm_vobu_admap_sector = read_u32(buf, 0x00DC)?;

        Ok(Self {
            last_sector_vmg_set,
            last_sector_ifo,
            version,
            vmg_category,
            number_of_volumes,
            volume_number,
            side_id,
            number_of_title_sets,
            provider_id,
            vmgi_mat_end,
            fp_pgc_addr,
            menu_vob_sector,
            tt_srpt_sector,
            vmgm_pgci_ut_sector,
            ptl_mait_sector,
            vts_atrt_sector,
            txtdt_mg_sector,
            vmgm_c_adt_sector,
            vmgm_vobu_admap_sector,
        })
    }
}

fn decode_ascii_trim(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).trim_end().to_string()
}

// ------------------------------------------------------------------
// TT_SRPT — Title Search Pointer Table (VMG-side)
// ------------------------------------------------------------------

/// One entry of the VMG-side TT_SRPT (title search pointer table).
///
/// Per mpucoder-ifo_vmg.html "TT_SRPT" each entry is 12 bytes and
/// indexes the disc-global "title number" to the (title-set, title-
/// in-title-set) pair plus the VTS's start sector on the disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DvdTitleEntry {
    /// Title type byte (jump/link/call permission bits — see spec).
    pub title_type: u8,
    /// Number of angles (1..=9).
    pub angle_count: u8,
    /// Number of chapters / parts-of-title (PTTs) in this title.
    pub chapter_count: u16,
    /// Parental management mask (16-bit; bit N set = blocked at level N).
    pub parental_mask: u16,
    /// VTS number (1..=99) this title lives in.
    pub vts_number: u8,
    /// Title number within that VTS.
    pub vts_title_number: u8,
    /// Start sector of the VTS (whole-disc-relative LBA).
    pub vts_start_sector: u32,
}

impl DvdTitleEntry {
    /// 2-bit UOP-prohibition subset packed into the low bits of
    /// [`Self::title_type`].
    ///
    /// Per `docs/container/dvd/application/mpucoder-uops.html`,
    /// TT_SRPT carries only UOP-0 (`TimePlayOrSearch`) and UOP-1
    /// (`PttPlayOrSearch`); they live in the two low bits of
    /// `title_type`. The remaining `title_type` bits encode the
    /// title's jump/link/call permission flags per
    /// `mpucoder-ifo_vmg.html` and stay outside the UOP surface.
    #[inline]
    pub fn uop_mask(&self) -> crate::uops::UopMask {
        crate::uops::title_type_uop_mask(self.title_type)
    }

    /// `true` when `op` is **not** prohibited at the TT_SRPT level
    /// for this title. Only the two TT_SRPT-applicable ops
    /// (`TimePlayOrSearch`, `PttPlayOrSearch`) ever yield a `false`
    /// here; every other op returns `true` because TT_SRPT has no
    /// bit to encode them. The PGC-level and PCI-VOBU-level masks
    /// still need to be consulted via
    /// [`crate::uops::UopMask::merge_or`].
    #[inline]
    pub fn is_user_op_allowed(&self, op: crate::uops::UserOp) -> bool {
        self.uop_mask().is_allowed(op)
    }
}

/// Parsed TT_SRPT body — 8-byte header plus N × 12-byte entries.
#[derive(Debug, Clone)]
pub struct TtSrpt {
    /// Number of titles (= entry count).
    pub title_count: u16,
    /// `end_address` field (last byte of last entry, relative to TT_SRPT start).
    pub end_address: u32,
    /// Parsed entries.
    pub entries: Vec<DvdTitleEntry>,
}

impl TtSrpt {
    /// Parse a TT_SRPT byte buffer. Buffer must include the 8-byte
    /// header and at least `title_count * 12` entry bytes.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 8 {
            return Err(Error::InvalidUdf("TT_SRPT: shorter than 8-byte header"));
        }
        let title_count = read_u16(buf, 0)?;
        let end_address = read_u32(buf, 4)?;
        let needed = 8usize.saturating_add(usize::from(title_count) * 12);
        if buf.len() < needed {
            return Err(Error::InvalidUdf(
                "TT_SRPT: buffer shorter than title_count*12",
            ));
        }
        let mut entries = Vec::with_capacity(usize::from(title_count));
        for i in 0..usize::from(title_count) {
            let base = 8 + i * 12;
            entries.push(DvdTitleEntry {
                title_type: read_u8(buf, base)?,
                angle_count: read_u8(buf, base + 1)?,
                chapter_count: read_u16(buf, base + 2)?,
                parental_mask: read_u16(buf, base + 4)?,
                vts_number: read_u8(buf, base + 6)?,
                vts_title_number: read_u8(buf, base + 7)?,
                vts_start_sector: read_u32(buf, base + 8)?,
            });
        }
        Ok(Self {
            title_count,
            end_address,
            entries,
        })
    }
}

// ------------------------------------------------------------------
// VTSI_MAT — Video Title Set Information Management Table
// ------------------------------------------------------------------

/// Parsed VTSI_MAT (the first 0x300 + audio/sub-picture extension
/// bytes of a `VTS_xx_0.IFO` file).
///
/// Like [`VmgIfo`], sector-pointer fields stay raw — `0` denotes
/// "absent".
#[derive(Debug, Clone)]
pub struct VtsiMat {
    /// Last sector of this title set (last sector of `VTS_xx_0.BUP`).
    pub last_sector_title_set: u32,
    /// Last sector of this IFO file (`VTS_xx_0.IFO`).
    pub last_sector_ifo: u32,
    /// Version number — see [`VmgIfo::version`].
    pub version: u16,
    /// VTS category (0 = unspecified, 1 = Karaoke).
    pub vts_category: u32,
    /// Last byte of the VTSI_MAT region.
    pub vtsi_mat_end: u32,
    /// Start sector of the menu VOB (`0` if no menu).
    pub menu_vob_sector: u32,
    /// Start sector of the title VOBs.
    pub title_vob_sector: u32,
    /// Sector pointer to VTS_PTT_SRPT.
    pub vts_ptt_srpt_sector: u32,
    /// Sector pointer to VTS_PGCI.
    pub vts_pgci_sector: u32,
    /// Sector pointer to VTSM_PGCI_UT (menu PGCI).
    pub vtsm_pgci_ut_sector: u32,
    /// Sector pointer to VTS_TMAPTI (time map table).
    pub vts_tmapti_sector: u32,
    /// Sector pointer to VTSM_C_ADT (menu cell address table).
    pub vtsm_c_adt_sector: u32,
    /// Sector pointer to VTSM_VOBU_ADMAP (menu VOBU address map).
    pub vtsm_vobu_admap_sector: u32,
    /// Sector pointer to VTS_C_ADT (title-set cell address table).
    pub vts_c_adt_sector: u32,
    /// Sector pointer to VTS_VOBU_ADMAP (title-set VOBU address map).
    pub vts_vobu_admap_sector: u32,
}

impl VtsiMat {
    /// Parse a `VTS_xx_0.IFO` byte buffer. Buffer must cover at least
    /// the VTSI_MAT region (the first 0x200 bytes).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 0x200 {
            return Err(Error::InvalidUdf("VTSI_MAT: buffer shorter than 0x200"));
        }
        if &buf[0..12] != VTS_MAGIC {
            return Err(Error::InvalidUdf("VTSI_MAT: bad magic"));
        }
        Ok(Self {
            last_sector_title_set: read_u32(buf, 0x000C)?,
            last_sector_ifo: read_u32(buf, 0x001C)?,
            version: read_u16(buf, 0x0020)?,
            vts_category: read_u32(buf, 0x0022)?,
            vtsi_mat_end: read_u32(buf, 0x0080)?,
            menu_vob_sector: read_u32(buf, 0x00C0)?,
            title_vob_sector: read_u32(buf, 0x00C4)?,
            vts_ptt_srpt_sector: read_u32(buf, 0x00C8)?,
            vts_pgci_sector: read_u32(buf, 0x00CC)?,
            vtsm_pgci_ut_sector: read_u32(buf, 0x00D0)?,
            vts_tmapti_sector: read_u32(buf, 0x00D4)?,
            vtsm_c_adt_sector: read_u32(buf, 0x00D8)?,
            vtsm_vobu_admap_sector: read_u32(buf, 0x00DC)?,
            vts_c_adt_sector: read_u32(buf, 0x00E0)?,
            vts_vobu_admap_sector: read_u32(buf, 0x00E4)?,
        })
    }
}

// ------------------------------------------------------------------
// VTS_PTT_SRPT — Part-of-Title Search Pointer Table (chapters)
// ------------------------------------------------------------------

/// One PTT (chapter) — points to a `(PGCN, PGN)` pair within the VTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ptt {
    /// Program Chain number (1-based).
    pub pgcn: u16,
    /// Program number within the PGC (1-based).
    pub pgn: u16,
}

/// One title in the PTT search pointer table — the list of chapters
/// for that title.
#[derive(Debug, Clone)]
pub struct PttTitle {
    pub chapters: Vec<Ptt>,
}

/// Parsed VTS_PTT_SRPT — 8-byte header + per-title offset list +
/// per-title PTT entries.
#[derive(Debug, Clone)]
pub struct VtsPttSrpt {
    pub title_count: u16,
    pub end_address: u32,
    pub titles: Vec<PttTitle>,
}

impl VtsPttSrpt {
    /// Parse a VTS_PTT_SRPT body.
    ///
    /// Layout per mpucoder-ifo_vts.html:
    ///
    /// ```text
    ///   0000: u16 number_of_titles (Nt)
    ///   0002: u16 reserved
    ///   0004: u32 end_address (last byte of last VTS_PTT)
    ///   0008: u32 offset_to_PTT[1]     ← VTS_PTTI[1]
    ///   000C: u32 offset_to_PTT[2]     ← VTS_PTTI[2]
    ///   ...
    ///   ...
    /// ```
    ///
    /// Each title's PTT region is a list of 4-byte `(PGCN, PGN)` pairs.
    /// The end of title i's region is bounded by either title i+1's
    /// offset (for i < Nt) or by `end_address + 1` (for i == Nt). We
    /// divide that span by 4 to recover the chapter count.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 8 {
            return Err(Error::InvalidUdf(
                "VTS_PTT_SRPT: shorter than 8-byte header",
            ));
        }
        let title_count = read_u16(buf, 0)?;
        let end_address = read_u32(buf, 4)?;
        let nt = usize::from(title_count);
        let offsets_end = 8usize.saturating_add(nt * 4);
        if buf.len() < offsets_end {
            return Err(Error::InvalidUdf(
                "VTS_PTT_SRPT: offset list past end of buffer",
            ));
        }
        let mut offsets = Vec::with_capacity(nt);
        for i in 0..nt {
            offsets.push(read_u32(buf, 8 + i * 4)? as usize);
        }
        let mut titles = Vec::with_capacity(nt);
        for i in 0..nt {
            let start = offsets[i];
            // End of this title's PTT region: next title's offset, or
            // (end_address + 1) for the last title.
            let end_excl = if i + 1 < nt {
                offsets[i + 1]
            } else {
                (end_address as usize).saturating_add(1)
            };
            if end_excl < start {
                return Err(Error::InvalidUdf(
                    "VTS_PTT_SRPT: title offsets not monotonic",
                ));
            }
            let span = end_excl - start;
            if span % 4 != 0 {
                return Err(Error::InvalidUdf(
                    "VTS_PTT_SRPT: title span not a multiple of 4",
                ));
            }
            let n_ptt = span / 4;
            if buf.len() < start + n_ptt * 4 {
                return Err(Error::InvalidUdf(
                    "VTS_PTT_SRPT: title body past end of buffer",
                ));
            }
            let mut chapters = Vec::with_capacity(n_ptt);
            for j in 0..n_ptt {
                let off = start + j * 4;
                chapters.push(Ptt {
                    pgcn: read_u16(buf, off)?,
                    pgn: read_u16(buf, off + 2)?,
                });
            }
            titles.push(PttTitle { chapters });
        }
        Ok(Self {
            title_count,
            end_address,
            titles,
        })
    }
}

// ------------------------------------------------------------------
// PGC — Program Chain (header + cells)
// ------------------------------------------------------------------

/// Per-cell playback information (16 bytes per entry in C_PBI).
///
/// Field layout per mpucoder-pgc.html "cell playback information
/// table entry":
///
/// - byte 0: cell category bits (cell type, block type, seamless,
///   interleaved, STC discontinuity, seamless-angle).
/// - byte 1: restricted flag (`0x80` = trick-play disallowed).
/// - byte 2: cell still time.
/// - byte 3: cell command # (1..=128, 0 = no command).
/// - bytes 4..8: cell playback time (BCD).
/// - bytes 8..12: first VOBU start sector.
/// - bytes 12..16: first ILVU end sector.
/// - bytes 16..20: last VOBU start sector.
/// - bytes 20..24: last VOBU end sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPlaybackInfo {
    pub category_byte0: u8,
    pub restricted: bool,
    pub still_time: u8,
    pub cell_command: u8,
    pub playback_time: PgcTime,
    pub first_vobu_start_sector: u32,
    pub first_ilvu_end_sector: u32,
    pub last_vobu_start_sector: u32,
    pub last_vobu_end_sector: u32,
}

impl CellPlaybackInfo {
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 24 {
            return Err(Error::InvalidUdf("C_PBI entry shorter than 24 bytes"));
        }
        let category_byte0 = read_u8(buf, 0)?;
        let restricted = (read_u8(buf, 1)? & 0x80) != 0;
        let still_time = read_u8(buf, 2)?;
        let cell_command = read_u8(buf, 3)?;
        let mut t = [0u8; 4];
        t.copy_from_slice(&buf[4..8]);
        let playback_time = PgcTime::from_bytes(t);
        let first_vobu_start_sector = read_u32(buf, 8)?;
        let first_ilvu_end_sector = read_u32(buf, 12)?;
        let last_vobu_start_sector = read_u32(buf, 16)?;
        let last_vobu_end_sector = read_u32(buf, 20)?;
        Ok(Self {
            category_byte0,
            restricted,
            still_time,
            cell_command,
            playback_time,
            first_vobu_start_sector,
            first_ilvu_end_sector,
            last_vobu_start_sector,
            last_vobu_end_sector,
        })
    }
}

/// Per-cell position information (4 bytes per entry in C_POS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPositionInfo {
    pub vob_id: u16,
    pub cell_id: u8,
}

impl CellPositionInfo {
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 {
            return Err(Error::InvalidUdf("C_POS entry shorter than 4 bytes"));
        }
        let vob_id = read_u16(buf, 0)?;
        // byte 2 reserved
        let cell_id = read_u8(buf, 3)?;
        Ok(Self { vob_id, cell_id })
    }
}

/// One entry of the PGC subpicture/highlight colour-LUT.
///
/// Per mpucoder-pgc.html the PGC header at offset `0x00A4` carries a
/// `16 × 4`-byte palette laid out as `(0, Y, Cr, Cb)`: byte 0 is a
/// reserved/zero pad, then the luma + the two chroma-difference
/// samples in 8-bit BT.601-range form. These sixteen entries are the
/// colour source a subpicture (SPU) display-control sequence indexes
/// into via its 4-bit colour codes (the SPU itself only stores the
/// 0..=15 palette index + a contrast/alpha nibble — see
/// mpucoder-spu.html), so a renderer needs this table to resolve a
/// subtitle/menu pixel to an actual YCrCb value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaletteEntry {
    /// Luma (Y) sample, 8-bit.
    pub y: u8,
    /// Cr (red chroma-difference) sample, 8-bit.
    pub cr: u8,
    /// Cb (blue chroma-difference) sample, 8-bit.
    pub cb: u8,
}

impl PaletteEntry {
    /// Parse one 4-byte `(0, Y, Cr, Cb)` palette cell. The leading
    /// byte is reserved and ignored.
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 {
            return Err(Error::InvalidUdf("PGC palette entry shorter than 4 bytes"));
        }
        Ok(Self {
            y: buf[1],
            cr: buf[2],
            cb: buf[3],
        })
    }
}

/// One 8-byte DVD-Video navigation command (VM instruction word).
///
/// Per mpucoder-pgc.html every command in a PGC command table is a
/// fixed 8-byte word. Decoding the opcode/operand semantics is
/// Phase 3c VM work (mpucoder-vmi.html); at the container layer we
/// surface the raw word so a downstream interpreter can execute it.
/// We expose the leading byte's top three bits as `command_type`
/// (the VMI command-group selector) for convenience without
/// committing to a full opcode model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NavCommand {
    /// The eight raw command bytes, big-endian on the wire.
    pub bytes: [u8; 8],
}

impl NavCommand {
    /// Wrap an 8-byte command word.
    fn parse(buf: &[u8]) -> Result<Self> {
        let slice = buf
            .get(0..8)
            .ok_or(Error::InvalidUdf("PGC command shorter than 8 bytes"))?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(slice);
        Ok(Self { bytes })
    }

    /// VMI command-type selector — the top three bits of byte 0.
    ///
    /// This is the coarse command-group field per mpucoder-vmi.html;
    /// full opcode decode is deferred to the Phase 3c VM. Provided so
    /// callers can classify a word (e.g. distinguish a link/jump from
    /// a SetSystem/Compare) without a full interpreter.
    pub fn command_type(&self) -> u8 {
        self.bytes[0] >> 5
    }
}

/// PGC command table — pre, post, and cell command lists.
///
/// Per mpucoder-pgc.html "command table" the table opens with an
/// 8-byte header (`pre count`, `post count`, `cell count`, and an
/// `end address` relative to the table start), followed by the three
/// command lists back to back, each entry a fixed 8-byte
/// [`NavCommand`]. The total `pre + post + cell` count is `<= 128`.
///
/// *Pre* commands run before the PGC's first cell; *post* commands
/// run after the last cell finishes; *cell* commands are referenced
/// by the per-cell `cell_command` index in [`CellPlaybackInfo`]
/// (1-based; `0` = none). Executing the words is Phase 3c VM work —
/// here we only carve the raw 8-byte words out of the table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgcCommandTable {
    /// Commands executed when the PGC starts.
    pub pre: Vec<NavCommand>,
    /// Commands executed when the PGC ends.
    pub post: Vec<NavCommand>,
    /// Commands indexed by a cell's `cell_command` field (1-based).
    pub cell: Vec<NavCommand>,
    /// `end address` field — last byte offset of the table relative
    /// to its own start.
    pub end_address: u16,
}

impl PgcCommandTable {
    /// Parse a command table. `buf` must start at the table's first
    /// byte (the `pre count` u16) and span at least through the last
    /// command word.
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 8 {
            return Err(Error::InvalidUdf("PGC command table shorter than header"));
        }
        let pre_count = read_u16(buf, 0)?;
        let post_count = read_u16(buf, 2)?;
        let cell_count = read_u16(buf, 4)?;
        let end_address = read_u16(buf, 6)?;

        let total = usize::from(pre_count) + usize::from(post_count) + usize::from(cell_count);
        // Spec invariant: the three lists together hold <= 128 words.
        if total > 128 {
            return Err(Error::InvalidUdf("PGC command table claims > 128 commands"));
        }

        let read_list = |start: usize, count: u16| -> Result<Vec<NavCommand>> {
            let mut out = Vec::with_capacity(usize::from(count));
            for i in 0..usize::from(count) {
                let off = start + i * 8;
                let word = buf
                    .get(off..off + 8)
                    .ok_or(Error::InvalidUdf("PGC command table list past end"))?;
                out.push(NavCommand::parse(word)?);
            }
            Ok(out)
        };

        let pre_start = 8usize;
        let post_start = pre_start + usize::from(pre_count) * 8;
        let cell_start = post_start + usize::from(post_count) * 8;

        let pre = read_list(pre_start, pre_count)?;
        let post = read_list(post_start, post_count)?;
        let cell = read_list(cell_start, cell_count)?;

        Ok(Self {
            pre,
            post,
            cell,
            end_address,
        })
    }
}

/// Parsed PGC header + cell tables.
///
/// Layout per mpucoder-pgc.html:
///
/// - 0x0000..0x00E4: PGC general information (PGC_GI) +
///   PGC_AST_CTL + PGC_SPST_CTL + PGC_PB_TIME + still time +
///   playback mode + palette (16 × 4-byte `(0, Y, Cr, Cb)` at
///   0x00A4).
/// - 0x00E4: u16 offset_to_commands (relative to PGC start). The
///   command table at that offset holds the pre/post/cell
///   navigation command lists ([`PgcCommandTable`]).
/// - 0x00E6: u16 offset_to_program_map.
/// - 0x00E8: u16 offset_to_cell_playback_information.
/// - 0x00EA: u16 offset_to_cell_position_information.
#[derive(Debug, Clone)]
pub struct Pgc {
    /// Number of programs in this PGC.
    pub number_of_programs: u8,
    /// Number of cells.
    pub number_of_cells: u8,
    /// PGC playback time (BCD).
    pub playback_time: PgcTime,
    /// Prohibited user-operation mask.
    pub prohibited_user_ops: u32,
    /// Next PGCN (`0` = none).
    pub next_pgcn: u16,
    /// Previous PGCN (`0` = none).
    pub prev_pgcn: u16,
    /// "Goup" (group-up) PGCN (`0` = none).
    pub goup_pgcn: u16,
    /// PGC still time (`255` = infinite).
    pub still_time: u8,
    /// Playback mode (0 = sequential; non-zero encodes
    /// random/shuffle + program count, see spec).
    pub playback_mode: u8,
    /// Subpicture/highlight colour-LUT — 16 `(Y, Cr, Cb)` entries
    /// from PGC offset `0x00A4` per mpucoder-pgc.html.
    pub palette: [PaletteEntry; 16],
    /// Offset within PGC to commands table (`0` = absent).
    pub offset_commands: u16,
    /// Offset within PGC to program map (`0` = absent).
    pub offset_program_map: u16,
    /// Offset within PGC to cell-playback-information table.
    pub offset_cell_playback: u16,
    /// Offset within PGC to cell-position-information table.
    pub offset_cell_position: u16,
    /// Per-program entry-cell numbers, length = `number_of_programs`.
    pub program_map: Vec<u8>,
    /// Per-cell playback info.
    pub cells: Vec<CellPlaybackInfo>,
    /// Per-cell position info (VOB id + Cell id pairs).
    pub cell_positions: Vec<CellPositionInfo>,
    /// Parsed pre/post/cell navigation command table. `None` when
    /// `offset_commands == 0` (no command table present).
    pub commands: Option<PgcCommandTable>,
}

impl Pgc {
    /// Parse one PGC blob. `buf` must start at the PGC's first byte
    /// and span at least through the last table referenced by the
    /// offset fields.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 0xEC {
            return Err(Error::InvalidUdf("PGC: buffer shorter than header"));
        }
        let number_of_programs = read_u8(buf, 0x0002)?;
        let number_of_cells = read_u8(buf, 0x0003)?;
        let mut t = [0u8; 4];
        t.copy_from_slice(&buf[0x0004..0x0008]);
        let playback_time = PgcTime::from_bytes(t);
        let prohibited_user_ops = read_u32(buf, 0x0008)?;
        let next_pgcn = read_u16(buf, 0x009C)?;
        let prev_pgcn = read_u16(buf, 0x009E)?;
        let goup_pgcn = read_u16(buf, 0x00A0)?;
        let still_time = read_u8(buf, 0x00A2)?;
        let playback_mode = read_u8(buf, 0x00A3)?;

        // Palette (subpicture colour-LUT): 16 × 4-byte (0, Y, Cr, Cb)
        // entries at PGC offset 0x00A4. This is part of the fixed
        // header (which the 0xEC length check above already covers).
        let mut palette = [PaletteEntry::default(); 16];
        for (i, slot) in palette.iter_mut().enumerate() {
            let base = 0x00A4 + i * 4;
            *slot = PaletteEntry::parse(&buf[base..base + 4])?;
        }

        let offset_commands = read_u16(buf, 0x00E4)?;
        let offset_program_map = read_u16(buf, 0x00E6)?;
        let offset_cell_playback = read_u16(buf, 0x00E8)?;
        let offset_cell_position = read_u16(buf, 0x00EA)?;

        // Program map: number_of_programs × 1 byte. Padded to word
        // boundary with zero per spec, but we only need the first N.
        let mut program_map = Vec::with_capacity(usize::from(number_of_programs));
        if offset_program_map != 0 {
            let base = usize::from(offset_program_map);
            for i in 0..usize::from(number_of_programs) {
                program_map.push(read_u8(buf, base + i)?);
            }
        }

        // Cell playback information table: number_of_cells × 24 bytes.
        let mut cells = Vec::with_capacity(usize::from(number_of_cells));
        if offset_cell_playback != 0 {
            let base = usize::from(offset_cell_playback);
            for i in 0..usize::from(number_of_cells) {
                let entry = &buf
                    .get(base + i * 24..base + (i + 1) * 24)
                    .ok_or(Error::InvalidUdf("PGC: C_PBI past end of buffer"))?;
                cells.push(CellPlaybackInfo::parse(entry)?);
            }
        }

        // Cell position information table: number_of_cells × 4 bytes.
        let mut cell_positions = Vec::with_capacity(usize::from(number_of_cells));
        if offset_cell_position != 0 {
            let base = usize::from(offset_cell_position);
            for i in 0..usize::from(number_of_cells) {
                let entry = &buf
                    .get(base + i * 4..base + (i + 1) * 4)
                    .ok_or(Error::InvalidUdf("PGC: C_POS past end of buffer"))?;
                cell_positions.push(CellPositionInfo::parse(entry)?);
            }
        }

        // Command table (pre/post/cell command lists). Absent when
        // offset_commands == 0 per mpucoder-pgc.html.
        let commands = if offset_commands != 0 {
            let base = usize::from(offset_commands);
            let tbl = buf
                .get(base..)
                .ok_or(Error::InvalidUdf("PGC: command table past end of buffer"))?;
            Some(PgcCommandTable::parse(tbl)?)
        } else {
            None
        };

        Ok(Self {
            number_of_programs,
            number_of_cells,
            playback_time,
            prohibited_user_ops,
            next_pgcn,
            prev_pgcn,
            goup_pgcn,
            still_time,
            playback_mode,
            palette,
            offset_commands,
            offset_program_map,
            offset_cell_playback,
            offset_cell_position,
            program_map,
            cells,
            cell_positions,
            commands,
        })
    }

    /// Typed view over [`Self::prohibited_user_ops`].
    ///
    /// The PGC-level UOP-prohibition mask follows the same 25-bit
    /// layout as the PCI / TT_SRPT levels; this accessor wraps the
    /// raw word so callers can use named [`UserOp`] variants
    /// instead of magic bit numbers. Per
    /// `docs/container/dvd/application/mpucoder-uops.html`, a set
    /// bit inhibits the associated control.
    #[inline]
    pub fn uop_mask(&self) -> crate::uops::UopMask {
        crate::uops::UopMask::from_bits(self.prohibited_user_ops)
    }

    /// `true` when `op` is **not** prohibited at the PGC level.
    /// The full player-visible answer is still subject to the
    /// TT_SRPT and PCI-VOBU masks per the spec's three-level OR
    /// rule; use [`crate::uops::UopMask::merge_or`] to combine.
    #[inline]
    pub fn is_user_op_allowed(&self, op: crate::uops::UserOp) -> bool {
        self.uop_mask().is_allowed(op)
    }
}

// ------------------------------------------------------------------
// PGCI — Program Chain Information (table of PGCs)
// ------------------------------------------------------------------

/// One entry in the PGCI SRP — category byte + offset to the PGC body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgciSrp {
    pub category: u32,
    /// Offset to the PGC body, relative to the PGCI start.
    pub offset: u32,
}

/// Parsed PGCI (VTS_PGCI or VMGM_PGCI body).
#[derive(Debug, Clone)]
pub struct Pgci {
    pub number_of_pgcs: u16,
    pub end_address: u32,
    pub srp: Vec<PgciSrp>,
    pub pgcs: Vec<Pgc>,
}

impl Pgci {
    /// Parse a PGCI body. `buf` must start at the first byte of the
    /// PGCI table and span at least through the last PGC.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 8 {
            return Err(Error::InvalidUdf("PGCI: shorter than 8-byte header"));
        }
        let number_of_pgcs = read_u16(buf, 0)?;
        let end_address = read_u32(buf, 4)?;
        let n = usize::from(number_of_pgcs);
        let srp_end = 8usize.saturating_add(n * 8);
        if buf.len() < srp_end {
            return Err(Error::InvalidUdf("PGCI: SRP list past end of buffer"));
        }
        let mut srp = Vec::with_capacity(n);
        for i in 0..n {
            let base = 8 + i * 8;
            srp.push(PgciSrp {
                category: read_u32(buf, base)?,
                offset: read_u32(buf, base + 4)?,
            });
        }
        let mut pgcs = Vec::with_capacity(n);
        for entry in &srp {
            let off = entry.offset as usize;
            if off == 0 || off >= buf.len() {
                return Err(Error::InvalidUdf("PGCI: PGC offset out of range"));
            }
            let pgc_buf = &buf[off..];
            pgcs.push(Pgc::parse(pgc_buf)?);
        }
        Ok(Self {
            number_of_pgcs,
            end_address,
            srp,
            pgcs,
        })
    }
}

// ------------------------------------------------------------------
// VTS_C_ADT — Cell Address Table
// ------------------------------------------------------------------

/// One row of the cell address table.
///
/// Per stnsoft-vmindx.html / mpucoder-ifo.html `c_adt`: each entry is
/// 12 bytes — `(vob_id u16, cell_id u8, reserved u8, start_sector
/// u32, end_sector u32)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellAddrEntry {
    pub vob_id: u16,
    pub cell_id: u8,
    pub start_sector: u32,
    pub end_sector: u32,
}

/// Parsed VTS_C_ADT body — also covers VMGM_C_ADT and VTSM_C_ADT
/// since they share the wire format.
#[derive(Debug, Clone)]
pub struct VtsCAdt {
    /// Number of distinct VOB IDs covered (NOT the entry count —
    /// per spec, multiple entries can share a VOB ID for the cells
    /// inside that VOB).
    pub number_of_vob_ids: u16,
    /// `end_address` field.
    pub end_address: u32,
    /// Parsed cell-address entries.
    pub entries: Vec<CellAddrEntry>,
}

impl VtsCAdt {
    /// Parse a C_ADT body. Entry count is recovered from
    /// `(end_address - 7) / 12` — the spec stores it implicitly.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 8 {
            return Err(Error::InvalidUdf("C_ADT: shorter than 8-byte header"));
        }
        let number_of_vob_ids = read_u16(buf, 0)?;
        let end_address = read_u32(buf, 4)?;
        // end_address = byte index of the last entry's last byte
        // relative to start of C_ADT. The entries start at byte 8.
        // Entry count = (end_address + 1 - 8) / 12.
        let body_bytes = (end_address as usize).saturating_add(1).saturating_sub(8);
        if body_bytes % 12 != 0 {
            return Err(Error::InvalidUdf(
                "C_ADT: end_address implies non-12-byte entry size",
            ));
        }
        let n = body_bytes / 12;
        let needed = 8 + n * 12;
        if buf.len() < needed {
            return Err(Error::InvalidUdf("C_ADT: buffer shorter than entry table"));
        }
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let base = 8 + i * 12;
            entries.push(CellAddrEntry {
                vob_id: read_u16(buf, base)?,
                cell_id: read_u8(buf, base + 2)?,
                // byte 3 reserved
                start_sector: read_u32(buf, base + 4)?,
                end_sector: read_u32(buf, base + 8)?,
            });
        }
        Ok(Self {
            number_of_vob_ids,
            end_address,
            entries,
        })
    }

    /// Look up the disc-LBA range for a `(vob_id, cell_id)` pair.
    /// Sectors are relative to the start of the title's VOB chunks.
    pub fn lookup(&self, vob_id: u16, cell_id: u8) -> Option<(u32, u32)> {
        self.entries
            .iter()
            .find(|e| e.vob_id == vob_id && e.cell_id == cell_id)
            .map(|e| (e.start_sector, e.end_sector))
    }
}

// ------------------------------------------------------------------
// High-level chapter / title materialisation
// ------------------------------------------------------------------

/// A chapter (Part-of-Title) at the API surface — pulls fields from
/// the PTT entry, the referenced PGC's cell list, and the C_ADT.
#[derive(Debug, Clone)]
pub struct DvdChapter {
    /// 1-based chapter number within the title.
    pub number: u16,
    /// Program-chain number this chapter lives in.
    pub pgcn: u16,
    /// Program number within that PGC.
    pub pgn: u16,
    /// First cell of this chapter (inclusive, 1-based).
    pub start_cell: u8,
    /// Last cell of this chapter (inclusive, 1-based). For all
    /// chapters but the last in a PGC this is the cell immediately
    /// before the next chapter's `start_cell`; for the last chapter
    /// it is the PGC's final cell.
    pub end_cell: u8,
    /// PGC-relative playback time for this chapter — the BCD field
    /// from the PGC header itself (chapters within a PGC don't carry
    /// their own playback time field, so we surface the PGC total).
    pub playback_time: PgcTime,
}

/// A title at the API surface — pulls fields from TT_SRPT (for the
/// title-level header) and VTS_PTT_SRPT (for the chapter list).
#[derive(Debug, Clone)]
pub struct DvdTitle {
    /// 1-based VTS_TTN — title number within its VTS.
    pub number: u8,
    /// Number of camera angles (1..=9).
    pub angle_count: u8,
    /// Number of chapters (= `chapters.len()`).
    pub chapter_count: u16,
    /// Per-chapter detail.
    pub chapters: Vec<DvdChapter>,
}

/// Parsed VTS — pulls VTSI_MAT, VTS_PTT_SRPT, VTS_PGCI, and VTS_C_ADT
/// into a single materialised view that's convenient for chapter
/// enumeration without re-walking the byte buffer.
#[derive(Debug, Clone)]
pub struct VtsIfo {
    /// VTS number (1..=99).
    pub vts_number: u8,
    /// Number of titles in this VTS.
    pub title_count: u8,
    /// Per-title chapter list.
    pub titles: Vec<DvdTitle>,
    /// All program chains in the VTS.
    pub pgcs: Vec<Pgc>,
    /// Cell address table.
    pub cell_adt: VtsCAdt,
    /// Raw VTSI_MAT (kept around so callers can reach sector pointers
    /// for the bits we don't materialise — VOBU_ADMAP, time map, etc.).
    pub mat: VtsiMat,
}

impl VtsIfo {
    /// Build a `VtsIfo` from the full IFO byte buffer (the entire
    /// `VTS_xx_0.IFO`, which is `last_sector_ifo` + 1 sectors long).
    ///
    /// Sector pointers in `VTSI_MAT` are interpreted as offsets into
    /// `buf` after multiplication by [`DVD_SECTOR`].
    pub fn parse(buf: &[u8], vts_number: u8) -> Result<Self> {
        let mat = VtsiMat::parse(buf)?;

        // VTS_PTT_SRPT
        let ptt_off = (mat.vts_ptt_srpt_sector as usize)
            .checked_mul(DVD_SECTOR)
            .ok_or(Error::InvalidUdf("VTSI: PTT sector overflow"))?;
        let ptt_buf = buf
            .get(ptt_off..)
            .ok_or(Error::InvalidUdf("VTSI: PTT sector past end"))?;
        let ptt_srpt = VtsPttSrpt::parse(ptt_buf)?;

        // VTS_PGCI
        let pgci_off = (mat.vts_pgci_sector as usize)
            .checked_mul(DVD_SECTOR)
            .ok_or(Error::InvalidUdf("VTSI: PGCI sector overflow"))?;
        let pgci_buf = buf
            .get(pgci_off..)
            .ok_or(Error::InvalidUdf("VTSI: PGCI sector past end"))?;
        let pgci = Pgci::parse(pgci_buf)?;

        // VTS_C_ADT
        let cadt_off = (mat.vts_c_adt_sector as usize)
            .checked_mul(DVD_SECTOR)
            .ok_or(Error::InvalidUdf("VTSI: C_ADT sector overflow"))?;
        let cadt_buf = buf
            .get(cadt_off..)
            .ok_or(Error::InvalidUdf("VTSI: C_ADT sector past end"))?;
        let cell_adt = VtsCAdt::parse(cadt_buf)?;

        // Materialise the chapter list. The PTT entry gives us
        // (PGCN, PGN). To recover (start_cell, end_cell) we look at
        // the referenced PGC's program_map — entry `pgn-1` is the
        // start cell, entry `pgn` (if it exists) is the next chapter's
        // start cell minus one. The last chapter runs through the
        // PGC's last cell.
        let title_count_u8 = u8::try_from(ptt_srpt.title_count.min(255))
            .map_err(|_| Error::InvalidUdf("VTSI: title count > 255"))?;
        let mut titles = Vec::with_capacity(usize::from(title_count_u8));
        for (i, ptt_title) in ptt_srpt.titles.iter().enumerate() {
            let title_number = (i as u8).saturating_add(1);
            let mut chapters = Vec::with_capacity(ptt_title.chapters.len());
            for (ch_i, ptt) in ptt_title.chapters.iter().enumerate() {
                let pgc = pgci
                    .pgcs
                    .get(usize::from(ptt.pgcn.saturating_sub(1)))
                    .ok_or(Error::InvalidUdf(
                        "VTSI: PTT references PGCN past end of PGCI",
                    ))?;
                let pgn_idx = usize::from(ptt.pgn.saturating_sub(1));
                let start_cell = *pgc
                    .program_map
                    .get(pgn_idx)
                    .ok_or(Error::InvalidUdf("VTSI: PTT PGN past program_map"))?;
                // Determine end_cell: next chapter in the same PGC
                // gives us its start_cell - 1; otherwise the PGC's
                // last cell.
                let next_in_same_pgc = ptt_title.chapters.get(ch_i + 1).and_then(|next_ptt| {
                    if next_ptt.pgcn == ptt.pgcn {
                        pgc.program_map
                            .get(usize::from(next_ptt.pgn.saturating_sub(1)))
                            .copied()
                            .map(|next_start| next_start.saturating_sub(1))
                    } else {
                        None
                    }
                });
                let end_cell = next_in_same_pgc.unwrap_or(pgc.number_of_cells);
                chapters.push(DvdChapter {
                    number: (ch_i as u16).saturating_add(1),
                    pgcn: ptt.pgcn,
                    pgn: ptt.pgn,
                    start_cell,
                    end_cell,
                    playback_time: pgc.playback_time,
                });
            }
            let chapter_count = chapters.len() as u16;
            titles.push(DvdTitle {
                number: title_number,
                angle_count: 1,
                chapter_count,
                chapters,
            });
        }

        Ok(Self {
            vts_number,
            title_count: title_count_u8,
            titles,
            pgcs: pgci.pgcs,
            cell_adt,
            mat,
        })
    }
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------
    // PgcTime decode
    // -------------------------------------------------------------

    #[test]
    fn pgc_time_decode_ntsc_30() {
        // 01:23:45.20 @ 30 fps → bytes 0x01 0x23 0x45 (0b11_10_0000 = 0xE0).
        // Frame field: bits 7+6 = 11 (30 fps), bits 5+4 = 0b10 = 2 in BCD
        // hi-nibble (frame tens), bits 3+0 = 0b0000 = 0 in BCD lo-nibble.
        // So frames = 2 * 10 + 0 = 20.
        let t = PgcTime::from_bytes([0x01, 0x23, 0x45, 0xE0]);
        assert_eq!(t.hours, 1);
        assert_eq!(t.minutes, 23);
        assert_eq!(t.seconds, 45);
        assert_eq!(t.frames, 20);
        assert_eq!(t.frame_rate, FrameRate::Ntsc30);
        assert_eq!(t.total_seconds(), 3600 + 23 * 60 + 45);
    }

    #[test]
    fn pgc_time_decode_pal_25() {
        // 00:00:01.00 @ 25 fps → bytes 0x00 0x00 0x01 (0b01_00_0000 = 0x40).
        let t = PgcTime::from_bytes([0x00, 0x00, 0x01, 0x40]);
        assert_eq!(t.frame_rate, FrameRate::Pal25);
        assert_eq!(t.frames, 0);
        assert_eq!(t.total_seconds(), 1);
    }

    // -------------------------------------------------------------
    // VMGI MAT parse
    // -------------------------------------------------------------

    fn build_vmg_mat() -> Vec<u8> {
        let mut b = vec![0u8; 0x200];
        b[0..12].copy_from_slice(VMG_MAGIC);
        // 0x000C: last sector of VMG set = 1000
        b[0x000C..0x0010].copy_from_slice(&1000u32.to_be_bytes());
        // 0x001C: last sector of IFO = 4
        b[0x001C..0x0020].copy_from_slice(&4u32.to_be_bytes());
        // 0x0020: version 0x0011 (major 1, minor 1)
        b[0x0020..0x0022].copy_from_slice(&0x0011u16.to_be_bytes());
        // 0x0022: VMG category (region mask byte 1 = 0xFF "no region")
        b[0x0022..0x0026].copy_from_slice(&0x00FF_0000u32.to_be_bytes());
        // 0x0026: number of volumes = 1
        b[0x0026..0x0028].copy_from_slice(&1u16.to_be_bytes());
        // 0x0028: volume number = 1
        b[0x0028..0x002A].copy_from_slice(&1u16.to_be_bytes());
        // 0x002A: side ID = 0
        b[0x002A] = 0;
        // 0x003E: number of title sets = 2
        b[0x003E..0x0040].copy_from_slice(&2u16.to_be_bytes());
        // 0x0040: provider ID "OXIDEAV-TEST"
        let pid = b"OXIDEAV-TEST";
        b[0x0040..0x0040 + pid.len()].copy_from_slice(pid);
        // 0x0080: VMGI_MAT end
        b[0x0080..0x0084].copy_from_slice(&0x01FFu32.to_be_bytes());
        // 0x0084: FP_PGC start address = 0
        b[0x0084..0x0088].copy_from_slice(&0u32.to_be_bytes());
        // 0x00C0: menu VOB sector = 0 (no menu)
        // 0x00C4: TT_SRPT sector = 1
        b[0x00C4..0x00C8].copy_from_slice(&1u32.to_be_bytes());
        // 0x00C8: VMGM_PGCI_UT sector = 0
        // 0x00CC: VMG_PTL_MAIT sector = 0
        // 0x00D0: VMG_VTS_ATRT sector = 2
        b[0x00D0..0x00D4].copy_from_slice(&2u32.to_be_bytes());
        // 0x00D4: TXTDT_MG sector = 0
        // 0x00D8: VMGM_C_ADT sector = 0
        // 0x00DC: VMGM_VOBU_ADMAP sector = 0
        b
    }

    #[test]
    fn vmgi_mat_parse_roundtrip() {
        let buf = build_vmg_mat();
        let vmg = VmgIfo::parse(&buf).unwrap();
        assert_eq!(vmg.last_sector_vmg_set, 1000);
        assert_eq!(vmg.last_sector_ifo, 4);
        assert_eq!(vmg.version, 0x0011);
        assert_eq!(vmg.number_of_volumes, 1);
        assert_eq!(vmg.volume_number, 1);
        assert_eq!(vmg.side_id, 0);
        assert_eq!(vmg.number_of_title_sets, 2);
        assert_eq!(vmg.provider_id, "OXIDEAV-TEST");
        assert_eq!(vmg.tt_srpt_sector, 1);
        assert_eq!(vmg.vts_atrt_sector, 2);
        assert_eq!(vmg.menu_vob_sector, 0);
    }

    #[test]
    fn vmgi_mat_rejects_bad_magic() {
        let mut buf = build_vmg_mat();
        buf[0..12].copy_from_slice(b"DVDVIDEO-BAD");
        let err = VmgIfo::parse(&buf).unwrap_err();
        match err {
            Error::InvalidUdf(_) => {}
            other => panic!("expected InvalidUdf, got {other:?}"),
        }
    }

    // -------------------------------------------------------------
    // VTSI MAT parse
    // -------------------------------------------------------------

    fn build_vtsi_mat(
        ptt_srpt_sector: u32,
        pgci_sector: u32,
        c_adt_sector: u32,
        title_vob_sector: u32,
    ) -> Vec<u8> {
        let mut b = vec![0u8; 0x200];
        b[0..12].copy_from_slice(VTS_MAGIC);
        // last_sector_title_set
        b[0x000C..0x0010].copy_from_slice(&100_000u32.to_be_bytes());
        // last_sector_ifo
        b[0x001C..0x0020].copy_from_slice(&15u32.to_be_bytes());
        // version 0x0011
        b[0x0020..0x0022].copy_from_slice(&0x0011u16.to_be_bytes());
        // VTS category = 0
        // VTSI_MAT end
        b[0x0080..0x0084].copy_from_slice(&0x01FFu32.to_be_bytes());
        // menu VOB sector = 0
        // title VOB sector
        b[0x00C4..0x00C8].copy_from_slice(&title_vob_sector.to_be_bytes());
        // PTT_SRPT
        b[0x00C8..0x00CC].copy_from_slice(&ptt_srpt_sector.to_be_bytes());
        // PGCI
        b[0x00CC..0x00D0].copy_from_slice(&pgci_sector.to_be_bytes());
        // VTSM_PGCI_UT = 0
        // VTS_TMAPTI = 0
        // VTSM_C_ADT = 0
        // VTSM_VOBU_ADMAP = 0
        // VTS_C_ADT
        b[0x00E0..0x00E4].copy_from_slice(&c_adt_sector.to_be_bytes());
        // VTS_VOBU_ADMAP = 0
        b
    }

    #[test]
    fn vtsi_mat_parse_roundtrip() {
        let buf = build_vtsi_mat(1, 2, 3, 42);
        let mat = VtsiMat::parse(&buf).unwrap();
        assert_eq!(mat.last_sector_title_set, 100_000);
        assert_eq!(mat.last_sector_ifo, 15);
        assert_eq!(mat.version, 0x0011);
        assert_eq!(mat.title_vob_sector, 42);
        assert_eq!(mat.vts_ptt_srpt_sector, 1);
        assert_eq!(mat.vts_pgci_sector, 2);
        assert_eq!(mat.vts_c_adt_sector, 3);
    }

    // -------------------------------------------------------------
    // TT_SRPT
    // -------------------------------------------------------------

    fn build_tt_srpt(entries: &[(u8, u8, u16, u8, u8, u32)]) -> Vec<u8> {
        // 8-byte header + N * 12 entries.
        let n = entries.len();
        let len = 8 + n * 12;
        let mut b = vec![0u8; len];
        b[0..2].copy_from_slice(&(n as u16).to_be_bytes());
        // reserved at 2..4 = 0
        // end_address = last byte of last entry, relative to start
        let end_addr = (len - 1) as u32;
        b[4..8].copy_from_slice(&end_addr.to_be_bytes());
        for (i, e) in entries.iter().enumerate() {
            let base = 8 + i * 12;
            b[base] = e.0; // title_type
            b[base + 1] = e.1; // angle_count
            b[base + 2..base + 4].copy_from_slice(&e.2.to_be_bytes()); // chapter_count
            b[base + 4..base + 6].copy_from_slice(&0u16.to_be_bytes()); // parental_mask
            b[base + 6] = e.3; // vts_number
            b[base + 7] = e.4; // vts_title_number
            b[base + 8..base + 12].copy_from_slice(&e.5.to_be_bytes()); // vts_start_sector
            let _ = e; // suppress unused
        }
        b
    }

    #[test]
    fn tt_srpt_parses_titles() {
        // 3 titles: VTS1-title1 (15 chapters), VTS1-title2 (4 chapters),
        // VTS2-title1 (1 chapter).
        let entries = [
            (0x3F, 1u8, 15u16, 1u8, 1u8, 0x0000_0500u32),
            (0x3F, 1u8, 4u16, 1u8, 2u8, 0x0000_0500u32),
            (0x3F, 2u8, 1u16, 2u8, 1u8, 0x0000_8000u32),
        ];
        let buf = build_tt_srpt(&entries);
        let srpt = TtSrpt::parse(&buf).unwrap();
        assert_eq!(srpt.title_count, 3);
        assert_eq!(srpt.end_address, (8 + 3 * 12 - 1) as u32);
        assert_eq!(srpt.entries[0].chapter_count, 15);
        assert_eq!(srpt.entries[1].vts_title_number, 2);
        assert_eq!(srpt.entries[2].vts_number, 2);
        assert_eq!(srpt.entries[2].angle_count, 2);
        assert_eq!(srpt.entries[2].vts_start_sector, 0x0000_8000);
    }

    // -------------------------------------------------------------
    // VTS_C_ADT
    // -------------------------------------------------------------

    fn build_c_adt(rows: &[(u16, u8, u32, u32)]) -> Vec<u8> {
        let n = rows.len();
        let len = 8 + n * 12;
        let mut b = vec![0u8; len];
        // number_of_vob_ids — let's pick the distinct vob count
        let distinct = {
            let mut v: Vec<u16> = rows.iter().map(|r| r.0).collect();
            v.sort();
            v.dedup();
            v.len() as u16
        };
        b[0..2].copy_from_slice(&distinct.to_be_bytes());
        // end_address = last byte of last entry, relative to start
        let end_addr = (len - 1) as u32;
        b[4..8].copy_from_slice(&end_addr.to_be_bytes());
        for (i, r) in rows.iter().enumerate() {
            let base = 8 + i * 12;
            b[base..base + 2].copy_from_slice(&r.0.to_be_bytes());
            b[base + 2] = r.1;
            // reserved at +3 = 0
            b[base + 4..base + 8].copy_from_slice(&r.2.to_be_bytes());
            b[base + 8..base + 12].copy_from_slice(&r.3.to_be_bytes());
        }
        b
    }

    #[test]
    fn c_adt_parses_four_rows() {
        // 4 cells: VOB 1 → cells 1 + 2 + 3; VOB 2 → cell 1.
        let rows = [
            (1u16, 1u8, 100u32, 199u32),
            (1u16, 2u8, 200u32, 299u32),
            (1u16, 3u8, 300u32, 399u32),
            (2u16, 1u8, 1000u32, 1999u32),
        ];
        let buf = build_c_adt(&rows);
        let adt = VtsCAdt::parse(&buf).unwrap();
        assert_eq!(adt.number_of_vob_ids, 2);
        assert_eq!(adt.entries.len(), 4);
        assert_eq!(adt.lookup(1, 2), Some((200, 299)));
        assert_eq!(adt.lookup(2, 1), Some((1000, 1999)));
        assert_eq!(adt.lookup(3, 1), None);
    }

    // -------------------------------------------------------------
    // PGCI with 1 PGC + 3 cells
    // -------------------------------------------------------------

    fn build_pgc_with_cells(cells: &[CellPlaybackInfo], positions: &[CellPositionInfo]) -> Vec<u8> {
        assert_eq!(cells.len(), positions.len());
        let n = cells.len();
        // PGC header is 0xEC bytes. Then program map (1 program, 1
        // byte each, padded to word boundary). Then C_PBI (24*n). Then
        // C_POS (4*n).
        let header_size = 0xEC;
        let prog_count = 1u8;
        let prog_map_size = (usize::from(prog_count) + 1) & !1; // pad to word
        let pre_n = 1usize; // command table: 1 pre + 1 post + 2 cell = 4 words
        let post_n = 1usize;
        let cmd_cell_n = 2usize;
        let cmd_table_size = 8 + (pre_n + post_n + cmd_cell_n) * 8;
        let cpbi_size = n * 24;
        let cpos_size = n * 4;
        let mut b = vec![0u8; header_size + cmd_table_size + prog_map_size + cpbi_size + cpos_size];

        // number_of_programs at 0x0002
        b[0x0002] = prog_count;
        // number_of_cells at 0x0003
        b[0x0003] = n as u8;
        // playback time
        b[0x0004..0x0008].copy_from_slice(&[0x00, 0x05, 0x00, 0xE0]); // 00:05:00.00 @ 30fps
                                                                      // 0x0008..: prohibited UOPs = 0
                                                                      // next/prev/goup at 0x009C / 0x009E / 0x00A0 = 0
                                                                      // still time, playback mode = 0

        // Palette at 0x00A4: 16 × (0, Y, Cr, Cb). Fill entry i with a
        // deterministic (Y=0x10+i, Cr=0x80, Cb=0x80) so the round-trip
        // can assert the layout decode.
        for i in 0..16usize {
            let base = 0x00A4 + i * 4;
            b[base] = 0x00; // reserved
            b[base + 1] = 0x10 + i as u8; // Y
            b[base + 2] = 0x80; // Cr
            b[base + 3] = 0x80; // Cb
        }

        let off_cmd = header_size as u16; // command table right after header
        let off_pmap = (header_size + cmd_table_size) as u16;
        let off_cpbi = (header_size + cmd_table_size + prog_map_size) as u16;
        let off_cpos = (header_size + cmd_table_size + prog_map_size + cpbi_size) as u16;
        b[0x00E4..0x00E6].copy_from_slice(&off_cmd.to_be_bytes());
        b[0x00E6..0x00E8].copy_from_slice(&off_pmap.to_be_bytes());
        b[0x00E8..0x00EA].copy_from_slice(&off_cpbi.to_be_bytes());
        b[0x00EA..0x00EC].copy_from_slice(&off_cpos.to_be_bytes());

        // Command table header + words. Each word is tagged in byte 0
        // so the round-trip can tell pre/post/cell apart.
        let ct = header_size;
        b[ct..ct + 2].copy_from_slice(&(pre_n as u16).to_be_bytes());
        b[ct + 2..ct + 4].copy_from_slice(&(post_n as u16).to_be_bytes());
        b[ct + 4..ct + 6].copy_from_slice(&(cmd_cell_n as u16).to_be_bytes());
        b[ct + 6..ct + 8].copy_from_slice(&((cmd_table_size - 1) as u16).to_be_bytes());
        let mut w = ct + 8;
        b[w] = 0xA0; // pre[0]: command_type = 0b101
        b[w + 7] = 0x01;
        w += 8;
        b[w] = 0xB0; // post[0]
        b[w + 7] = 0x02;
        w += 8;
        b[w] = 0xC0; // cell[0]
        b[w + 7] = 0x03;
        w += 8;
        b[w] = 0xC1; // cell[1]
        b[w + 7] = 0x04;

        // program map: 1 program starting at cell 1
        b[off_pmap as usize] = 1;
        // (no padding needed since prog_map_size already includes pad)

        // C_PBI
        for (i, c) in cells.iter().enumerate() {
            let base = off_cpbi as usize + i * 24;
            b[base] = c.category_byte0;
            b[base + 1] = if c.restricted { 0x80 } else { 0 };
            b[base + 2] = c.still_time;
            b[base + 3] = c.cell_command;
            // playback time — synthesise a deterministic field
            b[base + 4] = 0x00;
            b[base + 5] = 0x01;
            b[base + 6] = 0x00;
            b[base + 7] = 0xE0;
            b[base + 8..base + 12].copy_from_slice(&c.first_vobu_start_sector.to_be_bytes());
            b[base + 12..base + 16].copy_from_slice(&c.first_ilvu_end_sector.to_be_bytes());
            b[base + 16..base + 20].copy_from_slice(&c.last_vobu_start_sector.to_be_bytes());
            b[base + 20..base + 24].copy_from_slice(&c.last_vobu_end_sector.to_be_bytes());
        }

        // C_POS
        for (i, p) in positions.iter().enumerate() {
            let base = off_cpos as usize + i * 4;
            b[base..base + 2].copy_from_slice(&p.vob_id.to_be_bytes());
            b[base + 3] = p.cell_id;
        }
        b
    }

    fn make_cell(start: u32, end: u32) -> CellPlaybackInfo {
        CellPlaybackInfo {
            category_byte0: 0,
            restricted: false,
            still_time: 0,
            cell_command: 0,
            playback_time: PgcTime::from_bytes([0, 1, 0, 0xE0]),
            first_vobu_start_sector: start,
            first_ilvu_end_sector: start + 5,
            last_vobu_start_sector: end - 5,
            last_vobu_end_sector: end,
        }
    }

    #[test]
    fn pgci_parses_one_pgc_with_three_cells() {
        let cells = [
            make_cell(1000, 1999),
            make_cell(2000, 2999),
            make_cell(3000, 3999),
        ];
        let positions = [
            CellPositionInfo {
                vob_id: 1,
                cell_id: 1,
            },
            CellPositionInfo {
                vob_id: 1,
                cell_id: 2,
            },
            CellPositionInfo {
                vob_id: 1,
                cell_id: 3,
            },
        ];
        let pgc_blob = build_pgc_with_cells(&cells, &positions);

        // Wrap that single PGC into a PGCI: 8-byte header + 1 SRP
        // entry (8 bytes) + the PGC body.
        let srp_size = 8usize;
        let body_off = 8 + srp_size; // PGC starts here
        let total = body_off + pgc_blob.len();
        let mut b = vec![0u8; total];
        // number_of_pgcs = 1
        b[0..2].copy_from_slice(&1u16.to_be_bytes());
        // reserved at 2..4 = 0
        // end_address = total - 1
        b[4..8].copy_from_slice(&((total - 1) as u32).to_be_bytes());
        // SRP[0]: category 0, offset = body_off
        b[8..12].copy_from_slice(&0u32.to_be_bytes());
        b[12..16].copy_from_slice(&(body_off as u32).to_be_bytes());
        // Copy PGC body
        b[body_off..body_off + pgc_blob.len()].copy_from_slice(&pgc_blob);

        let pgci = Pgci::parse(&b).unwrap();
        assert_eq!(pgci.number_of_pgcs, 1);
        assert_eq!(pgci.pgcs.len(), 1);
        let pgc = &pgci.pgcs[0];
        assert_eq!(pgc.number_of_programs, 1);
        assert_eq!(pgc.number_of_cells, 3);
        assert_eq!(pgc.cells.len(), 3);
        assert_eq!(pgc.cell_positions.len(), 3);
        assert_eq!(pgc.cells[0].first_vobu_start_sector, 1000);
        assert_eq!(pgc.cells[2].last_vobu_end_sector, 3999);
        assert_eq!(pgc.cell_positions[1].cell_id, 2);
        assert_eq!(pgc.playback_time.frame_rate, FrameRate::Ntsc30);

        // Palette decode: entry i carries (Y=0x10+i, Cr=0x80, Cb=0x80).
        assert_eq!(
            pgc.palette[0],
            PaletteEntry {
                y: 0x10,
                cr: 0x80,
                cb: 0x80
            }
        );
        assert_eq!(
            pgc.palette[15],
            PaletteEntry {
                y: 0x1F,
                cr: 0x80,
                cb: 0x80
            }
        );

        // Command table: 1 pre + 1 post + 2 cell, tagged in byte 0.
        let cmds = pgc.commands.as_ref().expect("command table present");
        assert_eq!(cmds.pre.len(), 1);
        assert_eq!(cmds.post.len(), 1);
        assert_eq!(cmds.cell.len(), 2);
        assert_eq!(cmds.pre[0].bytes[0], 0xA0);
        assert_eq!(cmds.pre[0].bytes[7], 0x01);
        assert_eq!(cmds.pre[0].command_type(), 0b101);
        assert_eq!(cmds.post[0].bytes[0], 0xB0);
        assert_eq!(cmds.post[0].bytes[7], 0x02);
        assert_eq!(cmds.cell[0].bytes[7], 0x03);
        assert_eq!(cmds.cell[1].bytes[7], 0x04);
    }

    // -------------------------------------------------------------
    // PGC palette + command table — focused unit tests
    // -------------------------------------------------------------

    #[test]
    fn palette_entry_skips_reserved_byte() {
        // (reserved, Y, Cr, Cb)
        let e = PaletteEntry::parse(&[0xFF, 0x42, 0x10, 0xC0]).unwrap();
        assert_eq!(
            e,
            PaletteEntry {
                y: 0x42,
                cr: 0x10,
                cb: 0xC0
            }
        );
        // Too short → error.
        assert!(PaletteEntry::parse(&[0x00, 0x01, 0x02]).is_err());
    }

    #[test]
    fn command_table_carves_three_lists() {
        // 2 pre + 1 post + 1 cell = 4 words.
        let pre = 2u16;
        let post = 1u16;
        let cell = 1u16;
        let total = (pre + post + cell) as usize;
        let size = 8 + total * 8;
        let mut b = vec![0u8; size];
        b[0..2].copy_from_slice(&pre.to_be_bytes());
        b[2..4].copy_from_slice(&post.to_be_bytes());
        b[4..6].copy_from_slice(&cell.to_be_bytes());
        b[6..8].copy_from_slice(&((size - 1) as u16).to_be_bytes());
        // Tag each word's last byte with its 1-based index.
        for i in 0..total {
            b[8 + i * 8 + 7] = (i + 1) as u8;
        }
        let t = PgcCommandTable::parse(&b).unwrap();
        assert_eq!(t.pre.len(), 2);
        assert_eq!(t.post.len(), 1);
        assert_eq!(t.cell.len(), 1);
        assert_eq!(t.end_address, (size - 1) as u16);
        // pre = words 1,2; post = word 3; cell = word 4.
        assert_eq!(t.pre[0].bytes[7], 1);
        assert_eq!(t.pre[1].bytes[7], 2);
        assert_eq!(t.post[0].bytes[7], 3);
        assert_eq!(t.cell[0].bytes[7], 4);
    }

    #[test]
    fn command_table_rejects_overlong_count() {
        // pre alone claims 129 words → > 128 invariant violated.
        let mut b = vec![0u8; 8];
        b[0..2].copy_from_slice(&129u16.to_be_bytes());
        assert!(PgcCommandTable::parse(&b).is_err());
    }

    #[test]
    fn command_table_rejects_truncated_list() {
        // Header claims 2 words but the buffer only holds one.
        let mut b = vec![0u8; 8 + 8];
        b[0..2].copy_from_slice(&2u16.to_be_bytes());
        assert!(PgcCommandTable::parse(&b).is_err());
    }

    #[test]
    fn pgc_without_command_table_yields_none() {
        // build_pgc_with_cells always emits a command table; build a
        // minimal header-only PGC with offset_commands == 0.
        let mut b = vec![0u8; 0xEC];
        b[0x0002] = 0; // 0 programs
        b[0x0003] = 0; // 0 cells
        b[0x0004..0x0008].copy_from_slice(&[0x00, 0x00, 0x00, 0xC0]); // PAL 25 fps
                                                                      // all four table offsets stay 0
        let pgc = Pgc::parse(&b).unwrap();
        assert!(pgc.commands.is_none());
        // Palette defaults to all-zero when bytes are zero.
        assert_eq!(pgc.palette[7], PaletteEntry::default());
    }

    // -------------------------------------------------------------
    // VTS_PTT_SRPT walking 2 titles × 5 chapters
    // -------------------------------------------------------------

    #[test]
    fn ptt_srpt_walks_two_titles_five_chapters() {
        // 2 titles × 5 chapters each = 10 PTT entries × 4 bytes = 40 bytes
        // of chapter data. Plus 8-byte header + 2 × 4-byte offsets = 16
        // bytes of header. Total = 56 bytes.
        let n_titles = 2usize;
        let n_chaps = 5usize;
        let offsets_size = n_titles * 4;
        let header_size = 8 + offsets_size;
        let title_body_size = n_chaps * 4;
        let total = header_size + n_titles * title_body_size;
        let mut b = vec![0u8; total];
        // number_of_titles
        b[0..2].copy_from_slice(&(n_titles as u16).to_be_bytes());
        // end_address = total - 1
        b[4..8].copy_from_slice(&((total - 1) as u32).to_be_bytes());
        // offset_to_PTT[1] = header_size
        // offset_to_PTT[2] = header_size + title_body_size
        for ti in 0..n_titles {
            let off = (header_size + ti * title_body_size) as u32;
            b[8 + ti * 4..8 + ti * 4 + 4].copy_from_slice(&off.to_be_bytes());
        }
        // Fill chapter entries: PGCN = title_number, PGN = chapter_number
        for ti in 0..n_titles {
            for ci in 0..n_chaps {
                let base = header_size + ti * title_body_size + ci * 4;
                let pgcn = (ti + 1) as u16;
                let pgn = (ci + 1) as u16;
                b[base..base + 2].copy_from_slice(&pgcn.to_be_bytes());
                b[base + 2..base + 4].copy_from_slice(&pgn.to_be_bytes());
            }
        }

        let srpt = VtsPttSrpt::parse(&b).unwrap();
        assert_eq!(srpt.title_count, 2);
        assert_eq!(srpt.titles.len(), 2);
        for ti in 0..n_titles {
            assert_eq!(srpt.titles[ti].chapters.len(), 5);
            assert_eq!(srpt.titles[ti].chapters[0].pgcn, (ti + 1) as u16);
            assert_eq!(srpt.titles[ti].chapters[0].pgn, 1);
            assert_eq!(srpt.titles[ti].chapters[4].pgn, 5);
        }
    }

    // -------------------------------------------------------------
    // Round-trip composite: VTSI_MAT + PTT_SRPT + PGCI + C_ADT
    // -------------------------------------------------------------

    fn make_composite_vts() -> Vec<u8> {
        // We lay out a minimal IFO image:
        //   sector 0: VTSI_MAT
        //   sector 1: VTS_PTT_SRPT (1 title × 3 chapters)
        //   sector 2: VTS_PGCI (1 PGC with 5 cells; 3 programs)
        //   sector 3: VTS_C_ADT (5 cell entries)
        let mut img = vec![0u8; DVD_SECTOR * 4];

        // ---------- Sector 0: VTSI_MAT ----------
        let mat = build_vtsi_mat(1, 2, 3, 100);
        img[0..mat.len()].copy_from_slice(&mat);

        // ---------- Sector 2: VTS_PGCI ----------
        // 1 PGC with 5 cells (and 3 programs: cells 1, 3, 5 are
        // program entry points). Cells: 1, 2, 3, 4, 5 with disjoint
        // sector ranges.
        let cells: Vec<CellPlaybackInfo> = (0..5)
            .map(|i| make_cell(1000 + i * 1000, 1999 + i * 1000))
            .collect();
        let positions: Vec<CellPositionInfo> = (0..5)
            .map(|i| CellPositionInfo {
                vob_id: 1,
                cell_id: (i + 1) as u8,
            })
            .collect();
        // build_pgc_with_cells assumes 1 program; we extend manually
        // to 3 programs whose program_map = [1, 3, 5].
        let header_size = 0xEC;
        let prog_count = 3u8;
        let prog_map_size = (usize::from(prog_count) + 1) & !1; // 4
        let cpbi_size = 5 * 24;
        let cpos_size = 5 * 4;
        let pgc_blob_len = header_size + prog_map_size + cpbi_size + cpos_size;
        let mut pgc_blob = vec![0u8; pgc_blob_len];
        pgc_blob[0x0002] = prog_count;
        pgc_blob[0x0003] = 5; // number_of_cells
        pgc_blob[0x0004..0x0008].copy_from_slice(&[0x00, 0x15, 0x00, 0xE0]);
        let off_pmap = header_size as u16;
        let off_cpbi = (header_size + prog_map_size) as u16;
        let off_cpos = (header_size + prog_map_size + cpbi_size) as u16;
        pgc_blob[0x00E6..0x00E8].copy_from_slice(&off_pmap.to_be_bytes());
        pgc_blob[0x00E8..0x00EA].copy_from_slice(&off_cpbi.to_be_bytes());
        pgc_blob[0x00EA..0x00EC].copy_from_slice(&off_cpos.to_be_bytes());
        pgc_blob[header_size] = 1; // program 1 starts at cell 1
        pgc_blob[header_size + 1] = 3; // program 2 starts at cell 3
        pgc_blob[header_size + 2] = 5; // program 3 starts at cell 5
        for (i, c) in cells.iter().enumerate() {
            let base = header_size + prog_map_size + i * 24;
            pgc_blob[base + 4..base + 8].copy_from_slice(&[0, 1, 0, 0xE0]);
            pgc_blob[base + 8..base + 12].copy_from_slice(&c.first_vobu_start_sector.to_be_bytes());
            pgc_blob[base + 12..base + 16].copy_from_slice(&c.first_ilvu_end_sector.to_be_bytes());
            pgc_blob[base + 16..base + 20].copy_from_slice(&c.last_vobu_start_sector.to_be_bytes());
            pgc_blob[base + 20..base + 24].copy_from_slice(&c.last_vobu_end_sector.to_be_bytes());
        }
        for (i, p) in positions.iter().enumerate() {
            let base = header_size + prog_map_size + cpbi_size + i * 4;
            pgc_blob[base..base + 2].copy_from_slice(&p.vob_id.to_be_bytes());
            pgc_blob[base + 3] = p.cell_id;
        }
        // Wrap into PGCI
        let srp_size = 8usize;
        let body_off = 8 + srp_size;
        let pgci_total = body_off + pgc_blob.len();
        let mut pgci = vec![0u8; pgci_total];
        pgci[0..2].copy_from_slice(&1u16.to_be_bytes());
        pgci[4..8].copy_from_slice(&((pgci_total - 1) as u32).to_be_bytes());
        pgci[12..16].copy_from_slice(&(body_off as u32).to_be_bytes());
        pgci[body_off..body_off + pgc_blob.len()].copy_from_slice(&pgc_blob);
        img[2 * DVD_SECTOR..2 * DVD_SECTOR + pgci.len()].copy_from_slice(&pgci);

        // ---------- Sector 1: VTS_PTT_SRPT ----------
        // 1 title with 3 chapters at programs 1, 2, 3.
        let n_titles = 1usize;
        let n_chaps = 3usize;
        let header_sz = 8 + n_titles * 4;
        let title_body = n_chaps * 4;
        let total = header_sz + title_body;
        let mut ptt = vec![0u8; total];
        ptt[0..2].copy_from_slice(&(n_titles as u16).to_be_bytes());
        ptt[4..8].copy_from_slice(&((total - 1) as u32).to_be_bytes());
        ptt[8..12].copy_from_slice(&(header_sz as u32).to_be_bytes());
        for ci in 0..n_chaps {
            let base = header_sz + ci * 4;
            ptt[base..base + 2].copy_from_slice(&1u16.to_be_bytes()); // PGCN
            ptt[base + 2..base + 4].copy_from_slice(&((ci + 1) as u16).to_be_bytes());
            // PGN
        }
        img[DVD_SECTOR..DVD_SECTOR + ptt.len()].copy_from_slice(&ptt);

        // ---------- Sector 3: VTS_C_ADT ----------
        let cadt_rows: Vec<(u16, u8, u32, u32)> = (0..5)
            .map(|i| {
                (
                    1u16,
                    (i + 1) as u8,
                    1000 + i as u32 * 1000,
                    1999 + i as u32 * 1000,
                )
            })
            .collect();
        let cadt = build_c_adt(&cadt_rows);
        img[3 * DVD_SECTOR..3 * DVD_SECTOR + cadt.len()].copy_from_slice(&cadt);

        img
    }

    #[test]
    fn composite_vts_roundtrip() {
        let img = make_composite_vts();
        let vts = VtsIfo::parse(&img, 1).unwrap();
        assert_eq!(vts.vts_number, 1);
        assert_eq!(vts.title_count, 1);
        assert_eq!(vts.pgcs.len(), 1);
        // The PGC has 5 cells and 3 programs.
        assert_eq!(vts.pgcs[0].number_of_cells, 5);
        assert_eq!(vts.pgcs[0].number_of_programs, 3);
        // Chapter materialisation:
        let t = &vts.titles[0];
        assert_eq!(t.chapter_count, 3);
        // program_map = [1, 3, 5]; PTT[1] = (PGCN=1, PGN=1) →
        // start_cell=1, end_cell=2 (next program's start_cell - 1 = 3-1).
        assert_eq!(t.chapters[0].start_cell, 1);
        assert_eq!(t.chapters[0].end_cell, 2);
        // PTT[2] = (PGCN=1, PGN=2) → start_cell=3, end_cell=4.
        assert_eq!(t.chapters[1].start_cell, 3);
        assert_eq!(t.chapters[1].end_cell, 4);
        // PTT[3] = (PGCN=1, PGN=3) → start_cell=5, end_cell=5 (last PGC cell).
        assert_eq!(t.chapters[2].start_cell, 5);
        assert_eq!(t.chapters[2].end_cell, 5);
        // C_ADT must give us first cell's sector range.
        assert_eq!(vts.cell_adt.lookup(1, 1), Some((1000, 1999)));
        assert_eq!(vts.cell_adt.lookup(1, 5), Some((5000, 5999)));
    }
}
