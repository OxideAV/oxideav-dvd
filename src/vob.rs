//! VOB (Video OBject) demuxer — MPEG-2 Program Stream pack walker
//! + DVD nav-pack (PCI + DSI) decoder + per-substream elementary
//!   stream router.
//!
//! ## Scope (Phase 3a)
//!
//! Given a `VTS_xx_N.VOB` file (the Phase 1 enumeration surfaces
//! these as [`crate::disc::DvdFile`] entries) and the cell ranges
//! recovered from `VTS_C_ADT` in Phase 2, walk the file sector by
//! sector, classify each pack, and route its payload bytes into
//! per-elementary-stream buffers.
//!
//! Phase 3a stops at "elementary streams in `Vec<u8>` buffers". The
//! Phase 3b dispatch will wire those buffers into oxideav-mkv +
//! oxideav-cli-convert + the chapter-encoding path; this module's
//! deliverable is the typed demux surface only.
//!
//! ## Clean-room references
//!
//! All structure layouts come from the docs mirrored under
//! `docs/container/dvd/application/`:
//!
//! * `mpucoder-packhdr.html`  — MPEG-PS Pack Header (14 B, 0xBA)
//! * `mpucoder-pes-hdr.html`  — MPEG-PS PES header + DVD substream
//!   conventions for private_stream_1 (0xBD)
//! * `mpucoder-mpeghdrs.html` — MPEG-PS stream-ID table
//! * `mpucoder-pci_pkt.html`  — PCI packet (substream 0x00) incl.
//!   the HLI_GI / SL_COLI / BTN_IT highlight sub-structure
//! * `mpucoder-dsi_pkt.html`  — DSI packet (substream 0x01)
//! * `mpucoder-dvdmpeg.html`  — DVD substream allocations
//! * `stnsoft-vobov.html`     — pack/sector/VOBU/cell semantics
//! * `stnsoft-sys_hdr.html`   — Program Stream System Header (0xBB)
//!
//! No libdvdread, libdvdnav, libdvdcss, FFmpeg, VLC, mpv, or xine
//! source was consulted.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::disc::{DvdDisc, DvdFileKind};
use crate::error::{Error, Result};
use crate::ifo::DVD_SECTOR;

// ------------------------------------------------------------------
// MPEG-PS start codes (per mpucoder-mpeghdrs.html stream-ID table)
// ------------------------------------------------------------------

/// `0x000001BA` — MPEG-PS Pack Header.
pub const SC_PACK_HEADER: u8 = 0xBA;
/// `0x000001BB` — MPEG-PS System Header.
pub const SC_SYSTEM_HEADER: u8 = 0xBB;
/// `0x000001BC` — Program Stream Map (unused on DVD).
pub const SC_PROGRAM_STREAM_MAP: u8 = 0xBC;
/// `0x000001BD` — Private Stream 1 (AC-3 / DTS / LPCM / subpicture).
pub const SC_PRIVATE_STREAM_1: u8 = 0xBD;
/// `0x000001BE` — Padding Stream.
pub const SC_PADDING_STREAM: u8 = 0xBE;
/// `0x000001BF` — Private Stream 2 (NAV packets: PCI + DSI).
pub const SC_PRIVATE_STREAM_2: u8 = 0xBF;

// ------------------------------------------------------------------
// Pack Header (mpucoder-packhdr.html, ISO 13818-1 §2.5.3.4)
// ------------------------------------------------------------------

/// 14-byte MPEG-2 Program Stream Pack Header — the fixed prefix of
/// every DVD sector.
///
/// Layout per mpucoder-packhdr.html:
/// - bytes 0..4   `00 00 01 BA`
/// - bytes 4..10  `01 SCR[32..30] 1 SCR[29..15] 1 SCR[14..0] 1
///                 SCR_ext[8..0] 1`
/// - bytes 10..13 `program_mux_rate[21..0] 11`
/// - byte  13     `reserved[4..0] pack_stuffing_length[2..0]`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackHeader {
    /// 33-bit System Clock Reference base (90 kHz units; SCR / 300).
    pub scr_base: u64,
    /// 9-bit SCR extension (27 MHz unit; SCR mod 300).
    pub scr_ext: u16,
    /// 22-bit `program_mux_rate` in 50-byte/s units. The spec
    /// forbids the value `0`.
    pub mux_rate: u32,
    /// 3-bit `pack_stuffing_length` — number of `0xFF` stuffing
    /// bytes that follow this 14-byte header before the next
    /// start-code prefix.
    pub stuffing_bytes: u8,
}

impl PackHeader {
    /// Fixed pack-header length on the wire (sans stuffing).
    pub const SIZE: usize = 14;

    /// Parse a pack header from `buf` (which must be at least
    /// `SIZE` bytes; only the first `SIZE` are consumed).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(Error::InvalidUdf("VOB pack: shorter than 14-byte header"));
        }
        // Start code prefix + PACK identifier (0xBA).
        if buf[0..4] != [0x00, 0x00, 0x01, SC_PACK_HEADER] {
            return Err(Error::InvalidUdf("VOB pack: missing 0x000001BA start code"));
        }
        // Bytes 4..10 carry the SCR + SCR_ext + marker bits. The
        // MPEG-2 layout is documented in mpucoder-packhdr.html as:
        //
        //   bit pos:  47    44 43       28 27       12 11      3 2  0
        //   field  : 01 SCR(32..30) 1 SCR(29..15) 1 SCR(14..0) 1 ext  1
        //
        // We deserialise bit-by-bit to avoid endian or alignment
        // surprises.
        let b4 = buf[4] as u64;
        let b5 = buf[5] as u64;
        let b6 = buf[6] as u64;
        let b7 = buf[7] as u64;
        let b8 = buf[8] as u64;
        let b9 = buf[9] as u64;

        // Top bits of b4 must be `01`.
        if (b4 >> 6) != 0b01 {
            return Err(Error::InvalidUdf(
                "VOB pack: byte 4 top bits != 01 (not MPEG-2)",
            ));
        }
        // 3-bit SCR[32..30] at b4 bits 5..3.
        let scr_high = (b4 >> 3) & 0b111;
        // Marker bit at b4 bit 2 must be 1.
        if (b4 >> 2) & 0b1 != 1 {
            return Err(Error::InvalidUdf("VOB pack: SCR marker bit 1 missing"));
        }
        // 15-bit SCR[29..15] = b4 bits 1..0 (2) | b5 (8) | b6 bits 7..3 (5).
        let scr_mid = ((b4 & 0b11) << 13) | (b5 << 5) | ((b6 >> 3) & 0b1_1111);
        // Marker bit at b6 bit 2.
        if (b6 >> 2) & 0b1 != 1 {
            return Err(Error::InvalidUdf("VOB pack: SCR marker bit 2 missing"));
        }
        // 15-bit SCR[14..0] = b6 bits 1..0 (2) | b7 (8) | b8 bits 7..3 (5).
        let scr_low = ((b6 & 0b11) << 13) | (b7 << 5) | ((b8 >> 3) & 0b1_1111);
        // Marker bit at b8 bit 2.
        if (b8 >> 2) & 0b1 != 1 {
            return Err(Error::InvalidUdf("VOB pack: SCR marker bit 3 missing"));
        }
        // 9-bit SCR_ext = b8 bits 1..0 (2) | b9 bits 7..1 (7).
        let scr_ext_v = ((b8 & 0b11) << 7) | (b9 >> 1);
        // Final marker bit at b9 bit 0.
        if b9 & 0b1 != 1 {
            return Err(Error::InvalidUdf("VOB pack: SCR marker bit 4 missing"));
        }

        let scr_base = (scr_high << 30) | (scr_mid << 15) | scr_low;
        let scr_ext = scr_ext_v as u16;

        // Bytes 10..13: 22-bit program_mux_rate followed by `11`.
        let mux_rate = ((buf[10] as u32) << 14) | ((buf[11] as u32) << 6) | ((buf[12] as u32) >> 2);
        if buf[12] & 0b11 != 0b11 {
            return Err(Error::InvalidUdf(
                "VOB pack: mux_rate trailing marker bits != 11",
            ));
        }
        if mux_rate == 0 {
            return Err(Error::InvalidUdf("VOB pack: program_mux_rate is 0"));
        }

        // Byte 13: 5 reserved bits + 3-bit pack_stuffing_length.
        let stuffing_bytes = buf[13] & 0b111;

        Ok(Self {
            scr_base,
            scr_ext,
            mux_rate,
            stuffing_bytes,
        })
    }
}

// ------------------------------------------------------------------
// PES Packet (mpucoder-pes-hdr.html, ISO 13818-1 §2.4.3.6)
// ------------------------------------------------------------------

/// One DVD substream identifier carried inside a private_stream_1
/// (0xBD) PES payload's first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DvdSubstream {
    /// `0x20..=0x3F` — subpicture track (1 of 32).
    Subpicture(u8),
    /// `0x80..=0x87` — AC-3 audio track (1 of 8).
    Ac3(u8),
    /// `0x88..=0x8F` — DTS audio track (1 of 8).
    Dts(u8),
    /// `0xA0..=0xA7` — LPCM audio track (1 of 8).
    Lpcm(u8),
}

impl DvdSubstream {
    /// Classify the leading payload byte of a 0xBD packet per
    /// mpucoder-dvdmpeg.html.
    pub fn from_first_byte(b: u8) -> Option<Self> {
        match b {
            0x20..=0x3F => Some(Self::Subpicture(b)),
            0x80..=0x87 => Some(Self::Ac3(b)),
            0x88..=0x8F => Some(Self::Dts(b)),
            0xA0..=0xA7 => Some(Self::Lpcm(b)),
            _ => None,
        }
    }

    /// Storage track ID `(0..=7)` for audio/subpicture, normalised
    /// to the substream's local range.
    pub fn track(self) -> u8 {
        match self {
            Self::Subpicture(b) => b - 0x20,
            Self::Ac3(b) => b - 0x80,
            Self::Dts(b) => b - 0x88,
            Self::Lpcm(b) => b - 0xA0,
        }
    }
}

/// Parsed PES packet — header + raw payload slice (zero-copy into
/// the caller's buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PesPacket<'a> {
    /// MPEG-PS stream identifier (byte 3 of the start code).
    pub stream_id: u8,
    /// 33-bit Presentation Time Stamp in 90 kHz units (if present).
    pub pts: Option<u64>,
    /// 33-bit Decode Time Stamp in 90 kHz units (if present).
    pub dts: Option<u64>,
    /// Payload bytes (after the PES header). For private_stream_1
    /// (`stream_id == 0xBD`) the first byte of `payload` is the DVD
    /// substream identifier.
    pub payload: &'a [u8],
    /// On-wire size of the full PES packet (header + payload).
    /// Always `6 + PES_packet_length`.
    pub wire_size: usize,
}

impl<'a> PesPacket<'a> {
    /// Parse a PES packet starting at `buf[0]`. `buf` must contain
    /// the entire packet (6-byte fixed header + `PES_packet_length`
    /// payload bytes).
    pub fn parse(buf: &'a [u8]) -> Result<Self> {
        if buf.len() < 6 {
            return Err(Error::InvalidUdf("PES: shorter than 6-byte header"));
        }
        if buf[0..3] != [0x00, 0x00, 0x01] {
            return Err(Error::InvalidUdf("PES: missing 0x000001 start code"));
        }
        let stream_id = buf[3];
        let pkt_len = ((buf[4] as usize) << 8) | (buf[5] as usize);
        let wire_size = 6 + pkt_len;
        if buf.len() < wire_size {
            return Err(Error::InvalidUdf(
                "PES: PES_packet_length exceeds available buffer",
            ));
        }

        // Streams that DON'T carry the 9-byte MPEG-2 extension
        // (padding stream + private_stream_2 — both per
        // mpucoder-pes-hdr.html "extension present? No").
        if stream_id == SC_PADDING_STREAM || stream_id == SC_PRIVATE_STREAM_2 {
            return Ok(Self {
                stream_id,
                pts: None,
                dts: None,
                payload: &buf[6..wire_size],
                wire_size,
            });
        }

        // All other DVD-relevant streams (0xBD private_stream_1,
        // 0xC0..=0xDF audio, 0xE0..=0xEF video) carry the MPEG-2
        // extension starting at byte 6.
        if buf.len() < 9 {
            return Err(Error::InvalidUdf(
                "PES: extension stream shorter than 9-byte header",
            ));
        }
        // Byte 6 top two bits must be `10` (marks an MPEG-2 PES
        // header — MPEG-1 used different framing here).
        if (buf[6] >> 6) != 0b10 {
            return Err(Error::InvalidUdf(
                "PES: byte-6 top bits != 10 (MPEG-1 framing not supported)",
            ));
        }
        let pts_dts_flags = (buf[7] >> 6) & 0b11;
        let header_data_len = buf[8] as usize;
        let payload_start = 9 + header_data_len;
        if payload_start > wire_size {
            return Err(Error::InvalidUdf(
                "PES: PES_header_data_length exceeds packet length",
            ));
        }

        let (pts, dts) = match pts_dts_flags {
            0b00 => (None, None),
            0b01 => return Err(Error::InvalidUdf("PES: PTS_DTS_flags == 01 is forbidden")),
            0b10 => {
                // PTS only — 5-byte field starting at byte 9.
                if header_data_len < 5 {
                    return Err(Error::InvalidUdf("PES: PTS flagged but data_len < 5"));
                }
                let pts_v = parse_timestamp(&buf[9..14], 0b0010)?;
                (Some(pts_v), None)
            }
            0b11 => {
                // PTS + DTS — 10 bytes starting at byte 9.
                if header_data_len < 10 {
                    return Err(Error::InvalidUdf("PES: PTS+DTS flagged but data_len < 10"));
                }
                let pts_v = parse_timestamp(&buf[9..14], 0b0011)?;
                let dts_v = parse_timestamp(&buf[14..19], 0b0001)?;
                (Some(pts_v), Some(dts_v))
            }
            _ => unreachable!(),
        };

        Ok(Self {
            stream_id,
            pts,
            dts,
            payload: &buf[payload_start..wire_size],
            wire_size,
        })
    }

    /// If this is a private_stream_1 packet, classify the DVD
    /// substream from its first payload byte.
    pub fn dvd_substream(&self) -> Option<DvdSubstream> {
        if self.stream_id != SC_PRIVATE_STREAM_1 {
            return None;
        }
        self.payload
            .first()
            .and_then(|b| DvdSubstream::from_first_byte(*b))
    }
}

/// Decode a 5-byte MPEG-2 timestamp field.
///
/// Layout (mpucoder-pes-hdr.html): `<tag:4> <TS[32..30]:3> 1
/// <TS[29..15]:15> 1 <TS[14..0]:15> 1`. The 4-bit tag is `0010` for
/// PTS-only PTS, `0011` for the PTS in a PTS+DTS pair, and `0001`
/// for the DTS in a PTS+DTS pair.
fn parse_timestamp(buf: &[u8], expected_tag: u8) -> Result<u64> {
    if buf.len() < 5 {
        return Err(Error::InvalidUdf("PTS/DTS field shorter than 5 bytes"));
    }
    let tag = buf[0] >> 4;
    if tag != expected_tag {
        return Err(Error::InvalidUdf(
            "PTS/DTS leading tag does not match flags",
        ));
    }
    // Marker bits at buf[0] bit 0, buf[2] bit 0, buf[4] bit 0.
    if buf[0] & 1 != 1 || buf[2] & 1 != 1 || buf[4] & 1 != 1 {
        return Err(Error::InvalidUdf("PTS/DTS marker bit missing"));
    }
    let ts_high = ((buf[0] >> 1) & 0b111) as u64;
    let ts_mid = (((buf[1] as u64) << 7) | ((buf[2] as u64) >> 1)) & 0x7FFF;
    let ts_low = (((buf[3] as u64) << 7) | ((buf[4] as u64) >> 1)) & 0x7FFF;
    Ok((ts_high << 30) | (ts_mid << 15) | ts_low)
}

// ------------------------------------------------------------------
// Nav Pack — PCI + DSI (mpucoder-{pci,dsi}_pkt.html)
// ------------------------------------------------------------------

/// A single (color, contrast) scheme cell of an [`SlColi`] table.
///
/// Both `color` and `contrast` are 4-bit fields: `color` indexes the
/// PGC subpicture colour-LUT (`crate::ifo::PaletteEntry`) and
/// `contrast` is a 0..=15 blend weight (0 = transparent, 15 = opaque)
/// per `mpucoder-pci_pkt.html`'s SL_COLI table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlColiCell {
    /// 4-bit colour code (LUT index).
    pub color: u8,
    /// 4-bit contrast (transparency) value.
    pub contrast: u8,
}

/// One of the three selection/action colour-and-contrast schemes a
/// highlight can assign to its buttons (`SL_COLI_1..3`).
///
/// Each scheme carries four emphasis levels — `background` (code 0),
/// `pattern` (code 1), `emphasis1` (code 2), `emphasis2` (code 3) —
/// for both the *selection* and *action* highlight states, exactly
/// as laid out in the SL_COLI sub-table of `mpucoder-pci_pkt.html`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlColi {
    /// Selection-state cells, indexed by emphasis code 0..=3
    /// (`[background, pattern, emphasis1, emphasis2]`).
    pub selection: [SlColiCell; 4],
    /// Action-state cells, indexed by emphasis code 0..=3.
    pub action: [SlColiCell; 4],
}

impl SlColi {
    /// Decode one 8-byte SL_COLI scheme.
    ///
    /// Byte layout (`mpucoder-pci_pkt.html`):
    /// `0,1` = selection colour (e2 e1 / pat bg nibbles),
    /// `2,3` = selection contrast, `4,5` = action colour,
    /// `6,7` = action contrast. Each byte packs the higher emphasis
    /// code in the high nibble and the lower code in the low nibble.
    fn parse(b: &[u8; 8]) -> Self {
        // selection colour: byte0 = [e2 | e1], byte1 = [pat | bg]
        // selection contr:  byte2 = [e2 | e1], byte3 = [pat | bg]
        // action colour:    byte4 = [e2 | e1], byte5 = [pat | bg]
        // action contr:     byte6 = [e2 | e1], byte7 = [pat | bg]
        let mut selection = [SlColiCell::default(); 4];
        let mut action = [SlColiCell::default(); 4];
        // emphasis codes: 3 = emphasis2, 2 = emphasis1, 1 = pattern, 0 = bg
        selection[3].color = b[0] >> 4;
        selection[2].color = b[0] & 0x0F;
        selection[1].color = b[1] >> 4;
        selection[0].color = b[1] & 0x0F;
        selection[3].contrast = b[2] >> 4;
        selection[2].contrast = b[2] & 0x0F;
        selection[1].contrast = b[3] >> 4;
        selection[0].contrast = b[3] & 0x0F;
        action[3].color = b[4] >> 4;
        action[2].color = b[4] & 0x0F;
        action[1].color = b[5] >> 4;
        action[0].color = b[5] & 0x0F;
        action[3].contrast = b[6] >> 4;
        action[2].contrast = b[6] & 0x0F;
        action[1].contrast = b[7] >> 4;
        action[0].contrast = b[7] & 0x0F;
        Self { selection, action }
    }
}

/// One button's entry in the Button Information Table (`BTN_IT`).
///
/// 18 bytes per `mpucoder-pci_pkt.html`: a colour-scheme selector, a
/// rectangular pixel region (10-bit X/Y coordinates), an auto-action
/// flag, four adjacent-button selectors for D-pad navigation, and an
/// 8-byte VM command executed when the button is actioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonInfo {
    /// `btn_coln` — colour-scheme selector (0 = none, 1..=3 pick
    /// `SL_COLI_1..3`). 2-bit field.
    pub btn_coln: u8,
    /// Starting X pixel (10-bit, inclusive).
    pub start_x: u16,
    /// Ending X pixel (10-bit, inclusive).
    pub end_x: u16,
    /// Starting Y pixel (10-bit, inclusive).
    pub start_y: u16,
    /// Ending Y pixel (10-bit, inclusive).
    pub end_y: u16,
    /// Auto-action flag — if set, selecting the button immediately
    /// executes its command (no separate "enter" press).
    pub auto_action: bool,
    /// Button number to move to on "Up" (`AJBTN_POSI_UP`, 6-bit).
    pub up: u8,
    /// Button number to move to on "Down" (`AJBTN_POSI_DN`, 6-bit).
    pub down: u8,
    /// Button number to move to on "Left" (`AJBTN_POSI_LT`, 6-bit).
    pub left: u8,
    /// Button number to move to on "Right" (`AJBTN_POSI_RT`, 6-bit).
    pub right: u8,
    /// The 8-byte VM command run on action. Surfaced raw; executing
    /// it is Phase 3c VM work (`mpucoder-vmi.html`).
    pub command: [u8; 8],
}

impl ButtonInfo {
    /// Decode one 18-byte `BTN_IT` entry.
    fn parse(b: &[u8; 18]) -> Self {
        // 00: [btn_coln:2][start_x_hi:6]
        // 01: [start_x_lo:4][rsv:2][end_x_hi:2]
        // 02: [end_x_lo:8]
        // 03: [auto_action:2][start_y_hi:6]
        // 04: [start_y_lo:4][rsv:2][end_y_hi:2]
        // 05: [end_y_lo:8]
        // 06..09: [rsv:2][adj button:6]
        // 0a..11: 8-byte vm command
        let start_x = (((b[0] & 0x3F) as u16) << 4) | ((b[1] >> 4) as u16);
        let end_x = (((b[1] & 0x03) as u16) << 8) | (b[2] as u16);
        let start_y = (((b[3] & 0x3F) as u16) << 4) | ((b[4] >> 4) as u16);
        let end_y = (((b[4] & 0x03) as u16) << 8) | (b[5] as u16);
        let mut command = [0u8; 8];
        command.copy_from_slice(&b[0x0A..0x12]);
        Self {
            btn_coln: b[0] >> 6,
            start_x,
            end_x,
            start_y,
            end_y,
            auto_action: (b[3] >> 6) != 0,
            up: b[6] & 0x3F,
            down: b[7] & 0x3F,
            left: b[8] & 0x3F,
            right: b[9] & 0x3F,
            command,
        }
    }
}

/// Highlight Information (`HLI`) — the menu-button overlay a VOBU
/// carries in its PCI NAV-pack half.
///
/// Decoded from `HLI_GI` + `SL_COLI` + `BTN_IT` per
/// `mpucoder-pci_pkt.html`. Only present when `hli_ss & 0b11` selects
/// "all new" or "use previous" highlight info; [`PciPacket::parse`]
/// only materialises it when `btn_ns > 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightInfo {
    /// `hli_s_ptm` — highlight start presentation time (90 kHz).
    pub hli_s_ptm: u32,
    /// `hli_e_ptm` — highlight end presentation time (90 kHz).
    pub hli_e_ptm: u32,
    /// `btn_sl_e_ptm` — button-selection end time (user input after
    /// this is ignored).
    pub btn_sl_e_ptm: u32,
    /// `btn_md` — the raw button-grouping word (4 nibbles describing
    /// up to 3 button groups). Surfaced raw; the group-type decode is
    /// left to a renderer.
    pub btn_md: u16,
    /// `btn_sn` — starting button number (1-based).
    pub btn_sn: u8,
    /// `btn_ns` — number of buttons in this highlight (0..=36).
    pub btn_ns: u8,
    /// `nsl_btn_ns` — number of numerically selectable buttons.
    pub nsl_btn_ns: u8,
    /// `fosl_btnn` — force-select button number (0 = none).
    pub fosl_btnn: u8,
    /// `foac_btnn` — force-action button number (0 = none).
    pub foac_btnn: u8,
    /// The three `SL_COLI` colour-and-contrast schemes buttons pick
    /// from via [`ButtonInfo::btn_coln`].
    pub sl_coli: [SlColi; 3],
    /// The button table — exactly `btn_ns` entries (a VOBU declares
    /// up to 36 buttons).
    pub buttons: Vec<ButtonInfo>,
}

/// PCI packet — Presentation Control Information.
///
/// Layout per mpucoder-pci_pkt.html. Surfaces the `PCI_GI` general
/// information block (timing + UOP mask) plus, when the VOBU carries
/// a menu, the decoded [`HighlightInfo`] (HLI_GI + SL_COLI + BTN_IT).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciPacket {
    /// `PCI_GI 00` — `nv_pck_lbn`: disc-LBA of this NAV pack.
    pub nv_pck_lbn: u32,
    /// `PCI_GI 04` — `vobu_cat` (APS + reserved flags).
    pub vobu_cat: u16,
    /// `PCI_GI 08` — `vobu_uop_ctl`: prohibited-UOP bitmask.
    pub vobu_uop_ctl: u32,
    /// `PCI_GI 0C` — `vobu_s_ptm`: VOBU start presentation time (90 kHz).
    pub vobu_s_ptm: u32,
    /// `PCI_GI 10` — `vobu_e_ptm`: VOBU end presentation time (90 kHz).
    pub vobu_e_ptm: u32,
    /// `PCI_GI 14` — `vobu_se_e_ptm`: end PTM if Sequence_End_Code.
    pub vobu_se_e_ptm: u32,
    /// `PCI_GI 18` — `c_eltm`: cell elapsed time (BCD + frame-rate bits).
    pub c_eltm: u32,
    /// `HLI_GI 00` — `hli_ss`: highlight status (lower 2 bits).
    pub hli_ss: u16,
    /// Decoded highlight / button overlay for this VOBU. `None` when
    /// the VOBU declares no buttons (`btn_ns == 0`).
    pub highlight: Option<HighlightInfo>,
}

impl PciPacket {
    /// Packet-relative offset of `HLI_GI 00` (`hli_ss`).
    const HLI_GI: usize = 0x60;
    /// Packet-relative offset of the first `SL_COLI` scheme.
    const SL_COLI: usize = 0x76;
    /// Packet-relative offset of the first `BTN_IT` entry.
    const BTN_IT: usize = 0x8E;
    /// Bytes per `BTN_IT` entry.
    const BTN_IT_SIZE: usize = 18;
    /// Maximum buttons a VOBU may declare.
    const MAX_BUTTONS: usize = 36;

    /// Parse a PCI packet body. `buf` is the payload that follows
    /// the `0x000001BF` + length + substream-ID prefix — that is,
    /// it starts at `PCI_GI 00` (sector offset 0x2D).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        // The PCI_GI block alone occupies 0x1C bytes; HLI_GI starts
        // at 0x60 (packet-relative) so we need at least 0x62 bytes
        // for a meaningful read.
        if buf.len() < Self::HLI_GI + 2 {
            return Err(Error::InvalidUdf(
                "PCI: shorter than PCI_GI + HLI_GI prefix",
            ));
        }
        let hli_ss = read_u16_be(buf, Self::HLI_GI)?;
        let highlight = Self::parse_highlight(buf)?;
        Ok(Self {
            nv_pck_lbn: read_u32_be(buf, 0x00)?,
            vobu_cat: read_u16_be(buf, 0x04)?,
            vobu_uop_ctl: read_u32_be(buf, 0x08)?,
            vobu_s_ptm: read_u32_be(buf, 0x0C)?,
            vobu_e_ptm: read_u32_be(buf, 0x10)?,
            vobu_se_e_ptm: read_u32_be(buf, 0x14)?,
            c_eltm: read_u32_be(buf, 0x18)?,
            hli_ss,
            highlight,
        })
    }

    /// Decode the `HLI_GI` + `SL_COLI` + `BTN_IT` sub-structure.
    ///
    /// Returns `Ok(None)` when the VOBU declares no buttons
    /// (`btn_ns == 0`) or when the PCI body is too short to carry the
    /// `HLI_GI` header — a button-less VOBU is the common case and is
    /// not an error. When `btn_ns > 0` the buffer must hold the full
    /// `SL_COLI` block plus `btn_ns` `BTN_IT` entries, else
    /// `Error::InvalidUdf`.
    fn parse_highlight(buf: &[u8]) -> Result<Option<HighlightInfo>> {
        // HLI_GI runs HLI_GI+0x00..0x16 (btn_ns lives at HLI_GI+0x11).
        let btn_ns_off = Self::HLI_GI + 0x11;
        if buf.len() <= btn_ns_off {
            return Ok(None);
        }
        let btn_ns = buf[btn_ns_off];
        if btn_ns == 0 {
            return Ok(None);
        }
        if btn_ns as usize > Self::MAX_BUTTONS {
            return Err(Error::InvalidUdf("PCI HLI: btn_ns exceeds 36"));
        }
        // The SL_COLI block (3 × 8 B) starts at SL_COLI; BTN_IT
        // entries follow at BTN_IT. Require the whole declared button
        // table to be present.
        let need = Self::BTN_IT + (btn_ns as usize) * Self::BTN_IT_SIZE;
        if buf.len() < need {
            return Err(Error::InvalidUdf(
                "PCI HLI: body shorter than declared BTN_IT table",
            ));
        }

        let mut sl_coli = [SlColi::default(); 3];
        for (i, scheme) in sl_coli.iter_mut().enumerate() {
            let off = Self::SL_COLI + i * 8;
            let cell: [u8; 8] = buf[off..off + 8].try_into().unwrap();
            *scheme = SlColi::parse(&cell);
        }

        let mut buttons = Vec::with_capacity(btn_ns as usize);
        for i in 0..btn_ns as usize {
            let off = Self::BTN_IT + i * Self::BTN_IT_SIZE;
            let entry: [u8; 18] = buf[off..off + Self::BTN_IT_SIZE].try_into().unwrap();
            buttons.push(ButtonInfo::parse(&entry));
        }

        Ok(Some(HighlightInfo {
            hli_s_ptm: read_u32_be(buf, Self::HLI_GI + 0x02)?,
            hli_e_ptm: read_u32_be(buf, Self::HLI_GI + 0x06)?,
            btn_sl_e_ptm: read_u32_be(buf, Self::HLI_GI + 0x0A)?,
            btn_md: read_u16_be(buf, Self::HLI_GI + 0x0E)?,
            btn_sn: buf[Self::HLI_GI + 0x10],
            btn_ns,
            nsl_btn_ns: buf[Self::HLI_GI + 0x12],
            fosl_btnn: buf[Self::HLI_GI + 0x14],
            foac_btnn: buf[Self::HLI_GI + 0x15],
            sl_coli,
            buttons,
        }))
    }
}

/// DSI packet — Data Search Information.
///
/// Layout per mpucoder-dsi_pkt.html. We expose DSI_GI + the
/// VOBU_SRI seek-pointer table because that's the structure
/// chapter-accurate seeking needs. SML_PBI / SML_AGLI / SYNCI are
/// available in `raw` for future angle / sync-audio work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsiPacket {
    /// `DSI_GI 00` — `nv_pck_scr`.
    pub nv_pck_scr: u32,
    /// `DSI_GI 04` — `nv_pck_lbn`: disc-LBA of this NAV pack
    /// (redundant with PCI but kept for cross-check).
    pub nv_pck_lbn: u32,
    /// `DSI_GI 08` — `vobu_ea`: VOBU end address (relative).
    pub vobu_ea: u32,
    /// `DSI_GI 0C` — `vobu_1stref_ea`: first reference frame end.
    pub vobu_1stref_ea: u32,
    /// `DSI_GI 10` — `vobu_2ndref_ea`.
    pub vobu_2ndref_ea: u32,
    /// `DSI_GI 14` — `vobu_3rdref_ea`.
    pub vobu_3rdref_ea: u32,
    /// `DSI_GI 18` — `vobu_vob_idn`: VOB number.
    pub vobu_vob_idn: u16,
    /// `DSI_GI 1B` — `vobu_c_idn`: cell number within VOB.
    pub vobu_c_idn: u8,
    /// `DSI_GI 1C` — `c_eltm` (BCD + frame-rate bits).
    pub c_eltm: u32,
    /// VOBU_SRI search-information table — 21 forward + 21 backward
    /// pointers, decoded raw from packet offsets 0xEA..0x192. The
    /// pointer at index 0 is `sri_nvwv`; index 1..=20 are the
    /// forward `sri_fwda*` table; index 21..=41 are the backward
    /// counterparts; index 42 is `sri_pvwv` (the previous-VOBU-
    /// with-video pointer). The layout mirrors the DSI spec literally.
    pub vobu_sri: Box<[u32; 43]>,
}

impl DsiPacket {
    /// Parse a DSI packet body. `buf` starts at `DSI_GI 00`
    /// (sector offset 0x407).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        // VOBU_SRI extends to packet offset 0x192 + 4 = 0x196.
        if buf.len() < 0x196 {
            return Err(Error::InvalidUdf("DSI: shorter than DSI_GI + VOBU_SRI"));
        }
        let mut sri = [0u32; 43];
        // `sri_nvwv` at 0xEA, then 21 forward + 21 backward + pvwv.
        // The table is contiguous: 43 × 4 bytes = 172 bytes from
        // 0xEA..0x196.
        for (i, slot) in sri.iter_mut().enumerate() {
            *slot = read_u32_be(buf, 0xEA + i * 4)?;
        }
        Ok(Self {
            nv_pck_scr: read_u32_be(buf, 0x00)?,
            nv_pck_lbn: read_u32_be(buf, 0x04)?,
            vobu_ea: read_u32_be(buf, 0x08)?,
            vobu_1stref_ea: read_u32_be(buf, 0x0C)?,
            vobu_2ndref_ea: read_u32_be(buf, 0x10)?,
            vobu_3rdref_ea: read_u32_be(buf, 0x14)?,
            vobu_vob_idn: read_u16_be(buf, 0x18)?,
            vobu_c_idn: buf[0x1B],
            c_eltm: read_u32_be(buf, 0x1C)?,
            vobu_sri: Box::new(sri),
        })
    }
}

/// A Nav-Pack — the 2048-byte sector that opens every VOBU.
///
/// Sector layout per stnsoft-vobov.html:
///   [pack hdr 14B][sys hdr ~24B][PCI: 0xBF substream 0x00, 0x3D4 B]
///   [DSI: 0xBF substream 0x01, 0x3FA B]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavPack {
    pub pci: PciPacket,
    pub dsi: DsiPacket,
}

impl NavPack {
    /// Parse a full 2048-byte nav-pack sector. The sector must
    /// start with the pack header (offset 0x00) followed by a
    /// system header, a 0x3D4-byte PCI packet, and a 0x3FA-byte DSI
    /// packet at exactly the offsets given in mpucoder-pci_pkt.html
    /// and mpucoder-dsi_pkt.html.
    pub fn parse(sector: &[u8]) -> Result<Self> {
        if sector.len() < DVD_SECTOR {
            return Err(Error::InvalidUdf("nav-pack: shorter than one sector"));
        }
        // 1) Sector starts with pack header — validate by parsing.
        let _pack = PackHeader::parse(sector)?;
        // 2) System header at offset 0x0E (right after the 14-byte
        //    pack header, with `pack_stuffing_length == 0` for the
        //    DVD-Video profile per stnsoft-sys_hdr.html "the length
        //    and content are fixed").
        if sector[0x0E..0x12] != [0x00, 0x00, 0x01, SC_SYSTEM_HEADER] {
            return Err(Error::InvalidUdf(
                "nav-pack: 0x000001BB system header missing at offset 0x0E",
            ));
        }
        // 3) PCI packet at sector offset 0x26 per mpucoder-pci_pkt.html.
        if sector[0x26..0x2A] != [0x00, 0x00, 0x01, SC_PRIVATE_STREAM_2] {
            return Err(Error::InvalidUdf(
                "nav-pack: 0x000001BF (PCI) missing at offset 0x26",
            ));
        }
        if sector[0x2C] != 0x00 {
            return Err(Error::InvalidUdf(
                "nav-pack: PCI substream-ID != 0x00 at offset 0x2C",
            ));
        }
        let pci = PciPacket::parse(&sector[0x2D..])?;

        // 4) DSI packet at sector offset 0x400 per mpucoder-dsi_pkt.html.
        if sector[0x400..0x404] != [0x00, 0x00, 0x01, SC_PRIVATE_STREAM_2] {
            return Err(Error::InvalidUdf(
                "nav-pack: 0x000001BF (DSI) missing at offset 0x400",
            ));
        }
        if sector[0x406] != 0x01 {
            return Err(Error::InvalidUdf(
                "nav-pack: DSI substream-ID != 0x01 at offset 0x406",
            ));
        }
        let dsi = DsiPacket::parse(&sector[0x407..])?;

        Ok(Self { pci, dsi })
    }
}

/// Probe whether a sector looks like a Nav-Pack — cheaper than a
/// full `NavPack::parse` and used by `VobDemuxer` to route sectors.
pub fn looks_like_nav_pack(sector: &[u8]) -> bool {
    sector.len() >= 0x407
        && sector[0..4] == [0x00, 0x00, 0x01, SC_PACK_HEADER]
        && sector[0x0E..0x12] == [0x00, 0x00, 0x01, SC_SYSTEM_HEADER]
        && sector[0x26..0x2A] == [0x00, 0x00, 0x01, SC_PRIVATE_STREAM_2]
        && sector[0x2C] == 0x00
        && sector[0x400..0x404] == [0x00, 0x00, 0x01, SC_PRIVATE_STREAM_2]
        && sector[0x406] == 0x01
}

// ------------------------------------------------------------------
// Elementary stream routing
// ------------------------------------------------------------------

/// Logical destination for a PES payload routed by [`VobDemuxer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementaryStream {
    /// MPEG-2 video — `stream_id` in `0xE0..=0xEF` (DVD allows at
    /// most one video stream).
    Video,
    /// AC-3 audio track `0..=7`.
    Ac3(u8),
    /// DTS audio track `0..=7`.
    Dts(u8),
    /// LPCM audio track `0..=7`.
    Lpcm(u8),
    /// Subpicture track `0..=31`.
    Subpicture(u8),
}

/// Per-elementary-stream byte buffers extracted by [`VobDemuxer`].
#[derive(Debug, Default, Clone)]
pub struct VobStreams {
    /// Raw MPEG-2 video elementary stream bytes (DVD permits only
    /// one video stream, so this is a single `Vec<u8>`).
    pub video: Vec<u8>,
    /// AC-3 elementary streams, keyed by track ID (0..=7).
    pub ac3: BTreeMap<u8, Vec<u8>>,
    /// DTS elementary streams, keyed by track ID (0..=7).
    pub dts: BTreeMap<u8, Vec<u8>>,
    /// LPCM elementary streams, keyed by track ID (0..=7).
    pub lpcm: BTreeMap<u8, Vec<u8>>,
    /// Subpicture elementary streams, keyed by track ID (0..=31).
    pub subpicture: BTreeMap<u8, Vec<u8>>,
    /// Nav-packs consumed during demux — preserved so the Phase 3b
    /// chapter / seek logic can re-use the SRI tables.
    pub nav_packs: Vec<NavPack>,
}

// ------------------------------------------------------------------
// Demuxer
// ------------------------------------------------------------------

/// Stateful walker over a single VOB file. Holds no buffers itself
/// — callers feed sectors via [`Self::push_sector`] or pull from a
/// `Read + Seek` via [`Self::demux_range`].
#[derive(Debug, Default, Clone)]
pub struct VobDemuxer {
    out: VobStreams,
}

impl VobDemuxer {
    /// Build an empty demuxer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the accumulated elementary streams, replacing `self`'s
    /// buffer with an empty one.
    pub fn take(&mut self) -> VobStreams {
        std::mem::take(&mut self.out)
    }

    /// Push a 2048-byte sector and route its PES payload(s).
    ///
    /// A sector is an MPEG-PS pack: it always opens with a 14-byte
    /// pack header. The first VOBU sector additionally embeds the
    /// program-stream system header (0xBB) and is also a nav-pack
    /// (PCI + DSI under stream-id 0xBF). Subsequent sectors carry
    /// one or more video / audio / subpicture PES packets.
    pub fn push_sector(&mut self, sector: &[u8]) -> Result<()> {
        if sector.len() < DVD_SECTOR {
            return Err(Error::InvalidUdf("VOB sector: shorter than 2048 bytes"));
        }
        if looks_like_nav_pack(sector) {
            let nav = NavPack::parse(sector)?;
            self.out.nav_packs.push(nav);
            return Ok(());
        }
        let pack = PackHeader::parse(sector)?;
        let mut cursor = PackHeader::SIZE + pack.stuffing_bytes as usize;

        while cursor + 6 <= sector.len() {
            // Optional system header (0xBB) — can appear standalone
            // at the head of any pack other than a nav-pack, though
            // DVD spec only places it on the very first VOBU sector.
            if sector[cursor..cursor + 4] == [0x00, 0x00, 0x01, SC_SYSTEM_HEADER] {
                let len = ((sector[cursor + 4] as usize) << 8) | sector[cursor + 5] as usize;
                cursor += 6 + len;
                continue;
            }
            // Padding stream — skip without routing.
            if sector[cursor..cursor + 4] == [0x00, 0x00, 0x01, SC_PADDING_STREAM] {
                let len = ((sector[cursor + 4] as usize) << 8) | sector[cursor + 5] as usize;
                cursor += 6 + len;
                continue;
            }
            if sector[cursor..cursor + 3] != [0x00, 0x00, 0x01] {
                // No more start codes in this sector — done.
                break;
            }
            let pes = PesPacket::parse(&sector[cursor..])?;
            self.route(&pes);
            cursor += pes.wire_size;
        }
        Ok(())
    }

    /// Route a parsed PES packet to the appropriate elementary
    /// stream buffer.
    fn route(&mut self, pes: &PesPacket<'_>) {
        match pes.stream_id {
            // Video stream — DVD allows only 0xE0 in practice, but
            // the spec permits 0xE0..=0xEF.
            0xE0..=0xEF => {
                self.out.video.extend_from_slice(pes.payload);
            }
            // Private stream 1 — DVD audio / subpictures. First
            // payload byte is the substream ID; we strip it before
            // appending.
            SC_PRIVATE_STREAM_1 => {
                if let Some(sub) = pes.dvd_substream() {
                    // LPCM and AC-3 carry additional substream-
                    // specific framing bytes (frame counter +
                    // first-access pointer, etc.) that downstream
                    // codec decoders handle. Phase 3a writes the
                    // substream payload verbatim so a Phase 3b
                    // codec wrapper can interpret it.
                    let body = &pes.payload[1..];
                    match sub {
                        DvdSubstream::Ac3(_) => {
                            self.out
                                .ac3
                                .entry(sub.track())
                                .or_default()
                                .extend_from_slice(body);
                        }
                        DvdSubstream::Dts(_) => {
                            self.out
                                .dts
                                .entry(sub.track())
                                .or_default()
                                .extend_from_slice(body);
                        }
                        DvdSubstream::Lpcm(_) => {
                            self.out
                                .lpcm
                                .entry(sub.track())
                                .or_default()
                                .extend_from_slice(body);
                        }
                        DvdSubstream::Subpicture(_) => {
                            self.out
                                .subpicture
                                .entry(sub.track())
                                .or_default()
                                .extend_from_slice(body);
                        }
                    }
                }
                // Unknown private-stream-1 substreams (e.g. SDDS at
                // 0x88..) are dropped silently — they don't map to
                // any Phase 3b decoder.
            }
            // Private stream 2 (NAV) inside a non-nav-pack would
            // be ill-formed; ignore silently.
            SC_PRIVATE_STREAM_2 => {}
            // MPEG audio (DVD permits 0xC0..=0xC7 — eight streams)
            // is rare in the wild but the spec allows it. We pool
            // it into the AC-3 map under the same track index so a
            // single map can serve "audio track N" regardless of
            // codec. Callers needing to distinguish MPEG-1 audio
            // from AC-3 should probe the first frame.
            0xC0..=0xC7 => {
                let track = pes.stream_id - 0xC0;
                self.out
                    .ac3
                    .entry(track)
                    .or_default()
                    .extend_from_slice(pes.payload);
            }
            // All other stream IDs are not part of the DVD-Video
            // subset — drop without erroring (a fault here would
            // break authoring-tool-edge-case fixtures).
            _ => {}
        }
    }

    /// Read `count` sectors starting at disc-LBA `start` from
    /// `reader` and push each through [`Self::push_sector`].
    pub fn demux_range<R: Read + Seek>(
        &mut self,
        reader: &mut R,
        start: u32,
        count: u32,
    ) -> Result<()> {
        let sector_sz = DVD_SECTOR as u64;
        reader.seek(SeekFrom::Start(u64::from(start) * sector_sz))?;
        let mut buf = vec![0u8; DVD_SECTOR];
        for _ in 0..count {
            reader.read_exact(&mut buf)?;
            self.push_sector(&buf)?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------
// High-level entry point
// ------------------------------------------------------------------

/// `(VobId, CellId)` pair identifying one cell within a VTS.
pub type VobId = u16;
/// CellId is the 1-based cell number within the VOB.
pub type CellId = u8;

/// Demux the requested cells from a DVD title-set's VOB files into
/// per-elementary-stream byte buffers.
///
/// The cells are addressed via `(vob_id, cell_id)` pairs as
/// recovered from the title-set's `VTS_C_ADT` — see
/// [`crate::ifo::VtsCAdt::lookup`] which returns the sector range
/// for each `(vob_id, cell_id)`.
///
/// Sector ranges are read directly from the underlying `reader` —
/// the reader must be positioned at the start of the title set's
/// first VOB content sector (i.e. the cell-address-table values
/// are interpreted as disc-LBA, matching how Phase 1's `DvdFile`
/// surfaces them).
///
/// Title-set `0` (VMG / menus) is not supported; the caller must
/// pass `1..=99`.
pub fn demux_vobs<R: Read + Seek>(
    reader: &mut R,
    disc: &DvdDisc,
    title_set: u8,
    cells: &[(VobId, CellId)],
) -> Result<VobStreams> {
    if title_set == 0 {
        return Err(Error::NotDvdVideo(
            "demux_vobs: title_set 0 (VMG) not supported",
        ));
    }
    // Sanity-check the title set exists.
    if disc.vtsi(title_set).is_none() {
        return Err(Error::NotDvdVideo(
            "demux_vobs: title_set has no VTS_xx_0.IFO",
        ));
    }
    // The VTS_C_ADT entries are sector positions relative to the
    // title-set's first VOB's first sector. Look up that LBA so we
    // can translate.
    let first_title_vob = disc
        .video_ts_files
        .iter()
        .find(|f| matches!(f.kind, DvdFileKind::VtsTitle { ts, vob: 1 } if ts == title_set))
        .ok_or(Error::NotDvdVideo(
            "demux_vobs: title_set has no VTS_xx_1.VOB",
        ))?;
    let base_lba = first_title_vob.lba;

    let mut demuxer = VobDemuxer::new();
    for &(vob_id, cell_id) in cells {
        let (start_rel, end_rel) = lookup_cell(disc, title_set, vob_id, cell_id, reader)?;
        let start_abs = base_lba.saturating_add(start_rel).saturating_sub(1);
        // C_ADT sectors are 1-based relative to the VOB's first
        // sector per stnsoft-vmindx.html convention; subtracting 1
        // and adding back the file's LBA gives the absolute LBA.
        let count = end_rel.saturating_sub(start_rel).saturating_add(1);
        demuxer.demux_range(reader, start_abs, count)?;
    }
    Ok(demuxer.take())
}

/// Look up the (start_sector, end_sector) range for a
/// `(vob_id, cell_id)` pair from the title-set's parsed C_ADT.
///
/// This re-parses VTS_xx_0.IFO each call — Phase 3b will cache the
/// parsed VtsIfo once per title set.
fn lookup_cell<R: Read + Seek>(
    disc: &DvdDisc,
    title_set: u8,
    vob_id: VobId,
    cell_id: CellId,
    reader: &mut R,
) -> Result<(u32, u32)> {
    let vts = disc.parse_vts(reader, title_set)?;
    vts.cell_adt
        .lookup(vob_id, cell_id)
        .ok_or(Error::NotDvdVideo(
            "demux_vobs: (vob_id, cell_id) not in C_ADT",
        ))
}

/// Convenience wrapper around [`demux_vobs`] for callers that want
/// to point at the file on disk rather than carry a `Read + Seek`
/// themselves. The disc image is re-opened independently of any
/// reader previously used to parse the disc.
pub fn demux_vobs_path(
    disc: &DvdDisc,
    title_set: u8,
    cells: &[(VobId, CellId)],
    image_path: impl AsRef<Path>,
) -> Result<VobStreams> {
    let mut f = File::open(image_path.as_ref())?;
    demux_vobs(&mut f, disc, title_set, cells)
}

// ------------------------------------------------------------------
// Little helpers — local big-endian readers
// ------------------------------------------------------------------

fn read_u32_be(buf: &[u8], off: usize) -> Result<u32> {
    buf.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(Error::InvalidUdf("VOB: u32 read out of range"))
}

fn read_u16_be(buf: &[u8], off: usize) -> Result<u16> {
    buf.get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or(Error::InvalidUdf("VOB: u16 read out of range"))
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ----- helpers --------------------------------------------------

    /// Build a 14-byte pack header with the supplied SCR / mux_rate
    /// / stuffing fields, packed exactly as mpucoder-packhdr.html
    /// describes.
    fn build_pack_header(scr_base: u64, scr_ext: u16, mux_rate: u32, stuffing: u8) -> [u8; 14] {
        let mut b = [0u8; 14];
        b[0] = 0x00;
        b[1] = 0x00;
        b[2] = 0x01;
        b[3] = SC_PACK_HEADER;

        // Byte 4: 01 + SCR[32..30] + 1
        let scr_high = ((scr_base >> 30) & 0b111) as u8;
        b[4] = 0b0100_0000 | (scr_high << 3) | 0b0000_0100;
        // SCR[29..15] is 15 bits — split: 2 in b4 bottom, 8 in b5, 5 in b6 top
        let scr_mid = ((scr_base >> 15) & 0x7FFF) as u32;
        b[4] |= ((scr_mid >> 13) & 0b11) as u8;
        b[5] = ((scr_mid >> 5) & 0xFF) as u8;
        // Byte 6: top 5 bits of scr_mid[4..0], marker 1, bottom 2 of scr_low[14..13]
        b[6] = (((scr_mid & 0b1_1111) as u8) << 3) | 0b0000_0100;
        let scr_low = (scr_base & 0x7FFF) as u32;
        b[6] |= ((scr_low >> 13) & 0b11) as u8;
        // Byte 7: 8 bits of scr_low[12..5]
        b[7] = ((scr_low >> 5) & 0xFF) as u8;
        // Byte 8: top 5 bits of scr_low[4..0], marker 1, top 2 of scr_ext[8..7]
        b[8] = (((scr_low & 0b1_1111) as u8) << 3) | 0b0000_0100;
        b[8] |= ((scr_ext >> 7) & 0b11) as u8;
        // Byte 9: bottom 7 bits of scr_ext + marker 1
        b[9] = (((scr_ext as u8) & 0x7F) << 1) | 0b0000_0001;

        // Bytes 10..13: 22-bit mux_rate + "11"
        let mr = mux_rate & 0x3F_FFFF;
        b[10] = ((mr >> 14) & 0xFF) as u8;
        b[11] = ((mr >> 6) & 0xFF) as u8;
        b[12] = (((mr & 0x3F) as u8) << 2) | 0b0000_0011;

        // Byte 13: reserved (5 bits, 0xF8 mask) + stuffing length (3 bits)
        b[13] = 0xF8 | (stuffing & 0x07);

        b
    }

    fn build_system_header() -> Vec<u8> {
        // 12 fixed bytes + 12 stream_bound bytes = 24 byte payload;
        // start code (4) + length field (2) puts the on-wire size
        // at 24 bytes for the header part (length = 18).
        // Easier: pack a length-of-18 header per stnsoft-sys_hdr.html.
        let mut v = vec![0x00, 0x00, 0x01, SC_SYSTEM_HEADER, 0x00, 0x12];
        // 18 bytes of body — actual content irrelevant to the
        // demuxer, just preserve the length byte counts.
        v.extend(std::iter::repeat(0u8).take(18));
        v
    }

    fn build_pes_video(payload: &[u8], pts: Option<u64>) -> Vec<u8> {
        let pts_flag = if pts.is_some() { 0b10 } else { 0b00 };
        let pts_extra = if pts.is_some() { 5 } else { 0 };
        let header_data_len = pts_extra;
        let pes_len = 3 + header_data_len + payload.len();
        let mut v = Vec::with_capacity(6 + pes_len);
        v.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
        v.push((pes_len >> 8) as u8);
        v.push((pes_len & 0xFF) as u8);
        v.push(0b1000_0000); // marker bits, no scrambling, no flags
        v.push(pts_flag << 6);
        v.push(header_data_len as u8);
        if let Some(p) = pts {
            v.push(0x20 | (((p >> 29) & 0xE) as u8) | 1);
            v.push(((p >> 22) & 0xFF) as u8);
            v.push((((p >> 14) & 0xFE) as u8) | 1);
            v.push(((p >> 7) & 0xFF) as u8);
            v.push((((p << 1) & 0xFE) as u8) | 1);
        }
        v.extend_from_slice(payload);
        v
    }

    fn build_pes_private1(substream: u8, payload: &[u8]) -> Vec<u8> {
        let header_data_len = 0;
        let mut body = vec![substream];
        body.extend_from_slice(payload);
        let pes_len = 3 + header_data_len + body.len();
        let mut v = Vec::with_capacity(6 + pes_len);
        v.extend_from_slice(&[0x00, 0x00, 0x01, SC_PRIVATE_STREAM_1]);
        v.push((pes_len >> 8) as u8);
        v.push((pes_len & 0xFF) as u8);
        v.push(0b1000_0000);
        v.push(0b0000_0000); // no PTS/DTS
        v.push(header_data_len as u8);
        v.extend_from_slice(&body);
        v
    }

    // ----- pack header ---------------------------------------------

    #[test]
    fn pack_header_parse_roundtrip() {
        let hdr = build_pack_header(0x1_2345_6789, 42, 25200, 4);
        let p = PackHeader::parse(&hdr).unwrap();
        assert_eq!(p.scr_base, 0x1_2345_6789);
        assert_eq!(p.scr_ext, 42);
        assert_eq!(p.mux_rate, 25200);
        assert_eq!(p.stuffing_bytes, 4);
    }

    #[test]
    fn pack_header_rejects_bad_sync() {
        let mut hdr = build_pack_header(0, 0, 1, 0);
        hdr[3] = 0xBB; // not a pack
        assert!(PackHeader::parse(&hdr).is_err());
    }

    #[test]
    fn pack_header_rejects_zero_mux_rate() {
        // mux_rate = 0 is explicitly forbidden by mpucoder-packhdr.html
        let hdr = build_pack_header(0, 0, 0, 0);
        let err = PackHeader::parse(&hdr).unwrap_err();
        matches!(err, Error::InvalidUdf(_));
    }

    // ----- PES ------------------------------------------------------

    #[test]
    fn pes_no_pts() {
        let bytes = build_pes_video(&[0xAA, 0xBB, 0xCC], None);
        let pes = PesPacket::parse(&bytes).unwrap();
        assert_eq!(pes.stream_id, 0xE0);
        assert_eq!(pes.pts, None);
        assert_eq!(pes.dts, None);
        assert_eq!(pes.payload, &[0xAA, 0xBB, 0xCC]);
        assert_eq!(pes.wire_size, bytes.len());
    }

    #[test]
    fn pes_with_pts() {
        let pts = 0x1_2345_6789;
        let bytes = build_pes_video(&[0x10, 0x20], Some(pts));
        let pes = PesPacket::parse(&bytes).unwrap();
        assert_eq!(pes.pts, Some(pts));
        assert_eq!(pes.dts, None);
        assert_eq!(pes.payload, &[0x10, 0x20]);
    }

    #[test]
    fn pes_rejects_bad_start_code() {
        let mut bytes = build_pes_video(&[0xAA], None);
        bytes[2] = 0x02;
        assert!(PesPacket::parse(&bytes).is_err());
    }

    // ----- DVD substream classification -----------------------------

    #[test]
    fn substream_classifies_ac3_track() {
        let pes_bytes = build_pes_private1(0x82, &[0x0B, 0x77]); // AC-3 sync prefix
        let pes = PesPacket::parse(&pes_bytes).unwrap();
        let sub = pes.dvd_substream().unwrap();
        assert_eq!(sub, DvdSubstream::Ac3(0x82));
        assert_eq!(sub.track(), 2);
    }

    #[test]
    fn substream_classifies_subpicture() {
        let pes_bytes = build_pes_private1(0x21, &[0x00]);
        let sub = PesPacket::parse(&pes_bytes)
            .unwrap()
            .dvd_substream()
            .unwrap();
        assert_eq!(sub, DvdSubstream::Subpicture(0x21));
        assert_eq!(sub.track(), 1);
    }

    #[test]
    fn substream_classifies_lpcm_and_dts() {
        let lpcm = build_pes_private1(0xA0, &[0]);
        let dts = build_pes_private1(0x88, &[0x7F, 0xFE, 0x80, 0x01]);
        let lpcm_sub = PesPacket::parse(&lpcm).unwrap().dvd_substream().unwrap();
        let dts_sub = PesPacket::parse(&dts).unwrap().dvd_substream().unwrap();
        assert_eq!(lpcm_sub, DvdSubstream::Lpcm(0xA0));
        assert_eq!(dts_sub, DvdSubstream::Dts(0x88));
        assert_eq!(lpcm_sub.track(), 0);
        assert_eq!(dts_sub.track(), 0);
    }

    // ----- Nav-pack -------------------------------------------------

    fn build_nav_sector(nv_lbn: u32, vobu_s_ptm: u32, vobu_ea: u32) -> Vec<u8> {
        let mut sector = vec![0u8; DVD_SECTOR];
        // Pack header
        let pack = build_pack_header(0, 0, 25200, 0);
        sector[..14].copy_from_slice(&pack);
        // System header at 0x0E
        let sys = build_system_header();
        sector[0x0E..0x0E + sys.len()].copy_from_slice(&sys);

        // PCI prefix at 0x26: 00 00 01 BF 03 D4 00
        sector[0x26] = 0x00;
        sector[0x27] = 0x00;
        sector[0x28] = 0x01;
        sector[0x29] = SC_PRIVATE_STREAM_2;
        sector[0x2A] = 0x03;
        sector[0x2B] = 0xD4;
        sector[0x2C] = 0x00; // PCI substream
                             // PCI_GI 00..1C
        sector[0x2D..0x31].copy_from_slice(&nv_lbn.to_be_bytes());
        sector[0x39..0x3D].copy_from_slice(&vobu_s_ptm.to_be_bytes()); // vobu_s_ptm at 0x2D+0x0C = 0x39
                                                                       // By default no buttons: PCI HLI_GI btn_ns (packet 0x71 ->
                                                                       // sector 0x2D + 0x71 = 0x9E) stays 0 so highlight == None.

        // DSI prefix at 0x400: 00 00 01 BF 03 FA 01
        sector[0x400] = 0x00;
        sector[0x401] = 0x00;
        sector[0x402] = 0x01;
        sector[0x403] = SC_PRIVATE_STREAM_2;
        sector[0x404] = 0x03;
        sector[0x405] = 0xFA;
        sector[0x406] = 0x01; // DSI substream
                              // DSI_GI 00..1C — pour vobu_ea into DSI_GI offset 0x08
                              // DSI body starts at 0x407.
        sector[0x40B..0x40F].copy_from_slice(&nv_lbn.to_be_bytes()); // nv_pck_lbn @0x407+0x04
        sector[0x40F..0x413].copy_from_slice(&vobu_ea.to_be_bytes()); // vobu_ea @0x407+0x08
        sector
    }

    #[test]
    fn nav_pack_parse() {
        let sector = build_nav_sector(0xDEAD_BEEF, 0x0001_2345, 0x0000_07FF);
        assert!(looks_like_nav_pack(&sector));
        let nav = NavPack::parse(&sector).unwrap();
        assert_eq!(nav.pci.nv_pck_lbn, 0xDEAD_BEEF);
        assert_eq!(nav.pci.vobu_s_ptm, 0x0001_2345);
        assert_eq!(nav.dsi.nv_pck_lbn, 0xDEAD_BEEF);
        assert_eq!(nav.dsi.vobu_ea, 0x0000_07FF);
    }

    #[test]
    fn nav_pack_rejects_missing_dsi() {
        let mut sector = build_nav_sector(1, 0, 0);
        sector[0x403] = 0xCC; // corrupt DSI start code
        assert!(NavPack::parse(&sector).is_err());
        assert!(!looks_like_nav_pack(&sector));
    }

    // ----- PCI highlight (HLI / SL_COLI / BTN_IT) -------------------

    /// Sector offset of packet-relative PCI byte `p` (PCI body begins
    /// at sector 0x2D per mpucoder-pci_pkt.html).
    fn pci(p: usize) -> usize {
        0x2D + p
    }

    /// Inject a single-button HLI block into a nav sector built by
    /// `build_nav_sector`. Encodes one button with a known geometry +
    /// colour scheme + command so the decode can be asserted exactly.
    fn add_one_button_hli(sector: &mut [u8]) {
        // HLI_GI 00 (hli_ss) — "all new" status.
        sector[pci(0x60)] = 0x00;
        sector[pci(0x61)] = 0x01;
        // hli_s_ptm @0x62, hli_e_ptm @0x66, btn_sl_e_ptm @0x6a.
        sector[pci(0x62)..pci(0x66)].copy_from_slice(&0x0000_1111u32.to_be_bytes());
        sector[pci(0x66)..pci(0x6A)].copy_from_slice(&0x0000_2222u32.to_be_bytes());
        sector[pci(0x6A)..pci(0x6E)].copy_from_slice(&0x0000_3333u32.to_be_bytes());
        // btn_md @0x6e (raw), btn_sn @0x70, btn_ns @0x71.
        sector[pci(0x6E)] = 0x01;
        sector[pci(0x6F)] = 0x00;
        sector[pci(0x70)] = 1; // btn_sn
        sector[pci(0x71)] = 1; // btn_ns
        sector[pci(0x72)] = 1; // nsl_btn_ns
        sector[pci(0x74)] = 1; // fosl_btnn
        sector[pci(0x75)] = 1; // foac_btnn

        // SL_COLI_1 @0x76: selection colour [e2|e1]=0x21, [pat|bg]=0x43,
        // selection contr [e2|e1]=0x65, [pat|bg]=0x87; action colour
        // [e2|e1]=0xA9, [pat|bg]=0xCB, action contr [e2|e1]=0xED,
        // [pat|bg]=0x0F.
        sector[pci(0x76)..pci(0x7E)]
            .copy_from_slice(&[0x21, 0x43, 0x65, 0x87, 0xA9, 0xCB, 0xED, 0x0F]);

        // BTN_IT[0] @0x8e — 18 bytes.
        //   00: btn_coln=1 (bits7-6), start_x_hi=0x05 -> 0x45
        //   01: start_x_lo=0x6, end_x_hi=0x1 -> 0x61
        //   02: end_x_lo=0x23 -> end_x = 0x123
        //   03: auto_action=1 (bits7-6), start_y_hi=0x07 -> 0x47
        //   04: start_y_lo=0x8, end_y_hi=0x2 -> 0x82
        //   05: end_y_lo=0x9A -> end_y = 0x29A
        //   06: up=5, 07: down=6, 08: left=7, 09: right=8
        //   0a..11: command bytes 0xC0..0xC7
        let entry: [u8; 18] = [
            0x45, 0x61, 0x23, 0x47, 0x82, 0x9A, 0x05, 0x06, 0x07, 0x08, 0xC0, 0xC1, 0xC2, 0xC3,
            0xC4, 0xC5, 0xC6, 0xC7,
        ];
        let off = pci(0x8E);
        sector[off..off + 18].copy_from_slice(&entry);
    }

    #[test]
    fn pci_without_buttons_yields_no_highlight() {
        let sector = build_nav_sector(1, 0, 0);
        let nav = NavPack::parse(&sector).unwrap();
        assert_eq!(nav.pci.hli_ss, 0);
        assert!(nav.pci.highlight.is_none());
    }

    #[test]
    fn pci_decodes_single_button_highlight() {
        let mut sector = build_nav_sector(1, 0, 0);
        add_one_button_hli(&mut sector);
        let nav = NavPack::parse(&sector).unwrap();
        let hli = nav.pci.highlight.expect("highlight present");

        assert_eq!(hli.hli_s_ptm, 0x0000_1111);
        assert_eq!(hli.hli_e_ptm, 0x0000_2222);
        assert_eq!(hli.btn_sl_e_ptm, 0x0000_3333);
        assert_eq!(hli.btn_md, 0x0100);
        assert_eq!(hli.btn_sn, 1);
        assert_eq!(hli.btn_ns, 1);
        assert_eq!(hli.nsl_btn_ns, 1);
        assert_eq!(hli.fosl_btnn, 1);
        assert_eq!(hli.foac_btnn, 1);

        // SL_COLI_1 selection colour: bg=byte1.lo=3, pat=byte1.hi=4,
        // e1=byte0.lo=1, e2=byte0.hi=2.
        let sel = hli.sl_coli[0].selection;
        assert_eq!(sel[0].color, 3); // background
        assert_eq!(sel[1].color, 4); // pattern
        assert_eq!(sel[2].color, 1); // emphasis1
        assert_eq!(sel[3].color, 2); // emphasis2
                                     // selection contrast byte2=0x65,byte3=0x87: bg=7,pat=8,e1=5,e2=6.
        assert_eq!(sel[0].contrast, 7);
        assert_eq!(sel[1].contrast, 8);
        assert_eq!(sel[2].contrast, 5);
        assert_eq!(sel[3].contrast, 6);
        // action colour byte4=0xA9,byte5=0xCB: bg=B,pat=C,e1=9,e2=A.
        let act = hli.sl_coli[0].action;
        assert_eq!(act[0].color, 0xB);
        assert_eq!(act[1].color, 0xC);
        assert_eq!(act[2].color, 0x9);
        assert_eq!(act[3].color, 0xA);

        assert_eq!(hli.buttons.len(), 1);
        let b = &hli.buttons[0];
        assert_eq!(b.btn_coln, 1);
        assert_eq!(b.start_x, 0x56);
        assert_eq!(b.end_x, 0x123);
        assert_eq!(b.start_y, 0x78);
        assert_eq!(b.end_y, 0x29A);
        assert!(b.auto_action);
        assert_eq!(b.up, 5);
        assert_eq!(b.down, 6);
        assert_eq!(b.left, 7);
        assert_eq!(b.right, 8);
        assert_eq!(b.command, [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7]);
    }

    #[test]
    fn pci_rejects_overlong_btn_ns() {
        let mut sector = build_nav_sector(1, 0, 0);
        add_one_button_hli(&mut sector);
        sector[pci(0x71)] = 37; // btn_ns > 36
        assert!(NavPack::parse(&sector).is_err());
    }

    #[test]
    fn pci_rejects_truncated_button_table() {
        // A PCI body that declares 36 buttons but is only long enough
        // to reach HLI_GI must error rather than read past the slice.
        let mut buf = vec![0u8; PciPacket::BTN_IT + 18]; // room for one button
        buf[0x71] = 36; // btn_ns = 36 but only 1 entry of room
        assert!(PciPacket::parse(&buf).is_err());
    }

    // ----- End-to-end synthetic VOBU --------------------------------

    #[test]
    fn vobu_demux_routes_video_and_audio() {
        let mut demuxer = VobDemuxer::new();

        // Sector 0: nav pack.
        let nav = build_nav_sector(100, 0x12345, 0x000003);
        demuxer.push_sector(&nav).unwrap();
        assert_eq!(demuxer.out.nav_packs.len(), 1);

        // Sector 1: pack + one video PES with payload [0x01..0x10].
        let mut sec1 = vec![0u8; DVD_SECTOR];
        let pack = build_pack_header(0, 0, 25200, 0);
        sec1[..14].copy_from_slice(&pack);
        let video_payload: Vec<u8> = (1..=16).collect();
        let video_pes = build_pes_video(&video_payload, Some(0x1000));
        sec1[14..14 + video_pes.len()].copy_from_slice(&video_pes);
        demuxer.push_sector(&sec1).unwrap();

        // Sector 2: pack + one AC-3 PES on track 1.
        let mut sec2 = vec![0u8; DVD_SECTOR];
        sec2[..14].copy_from_slice(&pack);
        let ac3_payload: Vec<u8> = vec![0xAA; 32];
        let ac3_pes = build_pes_private1(0x81, &ac3_payload);
        sec2[14..14 + ac3_pes.len()].copy_from_slice(&ac3_pes);
        demuxer.push_sector(&sec2).unwrap();

        let streams = demuxer.take();
        assert_eq!(streams.video, video_payload);
        assert_eq!(
            streams.ac3.get(&1).map(Vec::as_slice),
            Some(ac3_payload.as_slice())
        );
        assert_eq!(streams.nav_packs.len(), 1);
        assert_eq!(streams.nav_packs[0].pci.nv_pck_lbn, 100);
    }
}
