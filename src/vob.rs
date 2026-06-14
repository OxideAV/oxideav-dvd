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
//! Layouts derive from the `docs/container/dvd/application/`
//! references listed above.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::disc::{DvdDisc, DvdFileKind};
use crate::error::{Error, Result};
use crate::ifo::{PgcTime, DVD_SECTOR};

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
    /// `btn_md` — the raw button-grouping word (describes up to 3
    /// button groups and their display-mode types). Use
    /// [`HighlightInfo::button_mode`] for the decoded
    /// [`ButtonMode`] view.
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

impl HighlightInfo {
    /// Typed view over [`Self::btn_md`].
    ///
    /// Decodes the `btn_md` word's button-group count and the three
    /// per-group display-mode type codes per the `btn_md word`
    /// sub-table of
    /// `docs/container/dvd/application/mpucoder-pci_pkt.html`. See
    /// [`ButtonMode`] for the bit layout and the spec caveat on the
    /// type-code naming.
    #[inline]
    pub const fn button_mode(&self) -> ButtonMode {
        ButtonMode::from_btn_md(self.btn_md)
    }
}

/// Highlight-status code carried in the lower two bits of
/// `PCI_GI.hli_ss` (offset `HLI_GI 00`).
///
/// Per `docs/container/dvd/application/mpucoder-pci_pkt.html`, the
/// field encodes how a player should treat the menu-button overlay
/// for the VOBU:
///
/// | bits | meaning                                                          |
/// |------|------------------------------------------------------------------|
/// | `00` | No highlight information for this VOBU                           |
/// | `01` | All-new highlight information for this VOBU                      |
/// | `10` | Re-use the highlight information from the previous VOBU          |
/// | `11` | Re-use the previous VOBU's highlight but take commands from here |
///
/// The remaining 14 bits of the raw `u16` are reserved; the typed
/// view exposes the four documented states and preserves any
/// reserved-bit content through [`HighlightStatus::raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightStatus {
    /// `00` — no highlight information for this VOBU.
    None,
    /// `01` — all-new highlight information for this VOBU.
    AllNew,
    /// `10` — re-use the highlight from the previous VOBU.
    UsePrevious,
    /// `11` — re-use the previous VOBU's highlight but take its
    /// per-button commands from this VOBU's `BTN_IT` instead.
    UsePreviousExceptCommands,
}

impl HighlightStatus {
    /// Decode the lower two bits of a raw `hli_ss` word.
    ///
    /// Per `mpucoder-pci_pkt.html` the upper 14 bits are reserved;
    /// they are ignored here. The mapping is exhaustive — every
    /// 2-bit value maps to a named variant, so the result is
    /// infallible.
    #[inline]
    pub const fn from_hli_ss(hli_ss: u16) -> Self {
        match hli_ss & 0b11 {
            0b00 => Self::None,
            0b01 => Self::AllNew,
            0b10 => Self::UsePrevious,
            0b11 => Self::UsePreviousExceptCommands,
            _ => unreachable!(),
        }
    }

    /// `true` when the VOBU carries no highlight information at all.
    #[inline]
    pub const fn is_none(self) -> bool {
        matches!(self, Self::None)
    }

    /// `true` when the VOBU declares fresh highlight geometry, i.e.
    /// `BTN_IT` should be parsed from this packet rather than from
    /// the predecessor.
    #[inline]
    pub const fn declares_new_geometry(self) -> bool {
        matches!(self, Self::AllNew)
    }

    /// `true` when the VOBU re-uses the previous highlight's geometry.
    /// Covers both the plain re-use and the
    /// "re-use except for commands" variants.
    #[inline]
    pub const fn reuses_previous_geometry(self) -> bool {
        matches!(self, Self::UsePrevious | Self::UsePreviousExceptCommands)
    }

    /// `true` when this VOBU's `BTN_IT` should supply the per-button
    /// command words even though geometry is inherited.
    ///
    /// Per the spec table, the `UsePreviousExceptCommands` case is
    /// the only one where geometry is inherited but commands are not.
    #[inline]
    pub const fn supplies_own_commands(self) -> bool {
        matches!(self, Self::AllNew | Self::UsePreviousExceptCommands)
    }

    /// Encode back to the lower two bits of `hli_ss`.
    #[inline]
    pub const fn to_bits(self) -> u16 {
        match self {
            Self::None => 0b00,
            Self::AllNew => 0b01,
            Self::UsePrevious => 0b10,
            Self::UsePreviousExceptCommands => 0b11,
        }
    }
}

/// Typed view over the `HLI_GI.btn_md` word (`HLI_GI 0x0E`).
///
/// Per the `btn_md word` sub-table of
/// `docs/container/dvd/application/mpucoder-pci_pkt.html`, the 16-bit
/// field groups the VOBU's buttons into up to three *button groups*,
/// each carrying a display-mode type. A button group lets the author
/// supply alternate button geometry for the three display aspects a
/// 4:3 player can present a 16:9 title in (normal/widescreen,
/// letterbox, pan-and-scan), so the active group is chosen by the
/// player's current display mode.
///
/// Bit layout, with the word read big-endian (high byte first):
///
/// | u16 bits | field       | meaning                              |
/// |----------|-------------|--------------------------------------|
/// | 15..14   | reserved    | —                                    |
/// | 13..12   | `btngr_ns`  | number of button groups (0..=3)      |
/// | 11       | reserved    | —                                    |
/// | 10..8    | `btngr1_ty` | button group 1 type (3-bit code)     |
/// | 7        | reserved    | —                                    |
/// | 6..4     | `btngr2_ty` | button group 2 type (3-bit code)     |
/// | 3        | reserved    | —                                    |
/// | 2..0     | `btngr3_ty` | button group 3 type (3-bit code)     |
///
/// The reference labels the three 3-bit type codes "normal, lb, p/s"
/// (normal / letterbox / pan-scan) but does **not** give the numeric
/// value-to-name mapping, so the group types are surfaced as raw 3-bit
/// codes rather than a named enum. A renderer that needs to pick the
/// group matching the current display mode keys off these codes plus
/// the title's own aspect attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ButtonMode {
    /// `btngr_ns` — number of button groups declared (0..=3).
    pub group_count: u8,
    /// The three group type codes (`btngr1_ty`, `btngr2_ty`,
    /// `btngr3_ty`), each a raw 3-bit value (0..=7). Only the first
    /// [`Self::group_count`] entries are meaningful; the rest are
    /// whatever the authoring tool left in the field.
    pub group_types: [u8; 3],
}

impl ButtonMode {
    /// Decode a raw `btn_md` word into its documented sub-fields.
    ///
    /// The mapping is exhaustive over the documented bit ranges; the
    /// reserved bits (15..14, 11, 7, 3) are ignored.
    #[inline]
    pub const fn from_btn_md(btn_md: u16) -> Self {
        Self {
            group_count: ((btn_md >> 12) & 0x3) as u8,
            group_types: [
                ((btn_md >> 8) & 0x7) as u8,
                ((btn_md >> 4) & 0x7) as u8,
                (btn_md & 0x7) as u8,
            ],
        }
    }

    /// Encode back to a raw `btn_md` word.
    ///
    /// Reserved bits are emitted as zero; the low three bits of each
    /// group type and the low two bits of the count are preserved.
    #[inline]
    pub const fn to_btn_md(self) -> u16 {
        (((self.group_count & 0x3) as u16) << 12)
            | (((self.group_types[0] & 0x7) as u16) << 8)
            | (((self.group_types[1] & 0x7) as u16) << 4)
            | ((self.group_types[2] & 0x7) as u16)
    }
}

/// One angle's non-seamless jump descriptor within [`NsmlAgli`].
///
/// Per `mpucoder-pci_pkt.html` NSML_AGLI sub-table, every angle gets a
/// 4-byte `nsml_agl_cN_dsta` record: the relative offset (in sectors)
/// to the VOBU that begins the *current* ILVU for that angle, so a
/// player switching angles non-seamlessly knows where to jump. Bit 31
/// is the direction (0 = forward, 1 = backward); the remaining 31 bits
/// are the magnitude. `0x0000_0000` flags an absent angle and
/// `0x7FFF_FFFF` flags "no more video for this angle".
///
/// Unlike the DSI [`SmlAngleCell`] (seamless playback), the PCI
/// non-seamless record carries only the jump offset — there is no ILVU
/// size field, because a non-seamless angle change re-seeks rather than
/// stitching the next interleaved unit inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NsmlAngleCell {
    /// Relative VOBU offset (in sectors) to the current ILVU for this
    /// angle. Bit 31 is the direction (0 = forward, 1 = backward); the
    /// low 31 bits are the magnitude. `0x0000_0000` = angle absent;
    /// `0x7FFF_FFFF` = no more video for this angle.
    pub dsta: u32,
}

impl NsmlAngleCell {
    /// Sentinel: this angle does not exist for the current VOBU.
    pub const ABSENT: u32 = 0x0000_0000;
    /// Sentinel: no more video for this angle.
    pub const NO_MORE_VIDEO: u32 = 0x7FFF_FFFF;

    /// `true` when no angle is defined for this slot
    /// (`dsta == 0x0000_0000`).
    #[inline]
    pub fn is_absent(&self) -> bool {
        self.dsta == Self::ABSENT
    }

    /// `true` when the angle exists but has no further video
    /// (`dsta == 0x7FFF_FFFF`).
    #[inline]
    pub fn is_no_more_video(&self) -> bool {
        self.dsta == Self::NO_MORE_VIDEO
    }

    /// `true` when the jump direction is backward (bit 31 set).
    ///
    /// Only meaningful when the cell is neither [`Self::is_absent`] nor
    /// [`Self::is_no_more_video`].
    #[inline]
    pub fn is_backward(&self) -> bool {
        self.dsta & 0x8000_0000 != 0
    }

    /// Jump magnitude in sectors (bit 31 stripped).
    ///
    /// Returns `None` for the [`Self::ABSENT`] / [`Self::NO_MORE_VIDEO`]
    /// sentinels, which do not encode a real offset.
    #[inline]
    pub fn offset_sectors(&self) -> Option<u32> {
        if self.is_absent() || self.is_no_more_video() {
            None
        } else {
            Some(self.dsta & 0x7FFF_FFFF)
        }
    }
}

/// NSML_AGLI — Non-Seamless Angle Information (PCI sub-block).
///
/// 36-byte block at PCI packet offset `0x3C..0x60` carrying the 9
/// `nsml_agl_cN_dsta` jump pointers (1-based angle index 1..=9). When a
/// title is a non-seamless multi-angle block, a player that wants to
/// switch to angle *N* at the next VOBU reads `cells[N - 1].dsta` and
/// re-seeks by that relative sector offset. A title with no multi-angle
/// data leaves every cell at [`NsmlAngleCell::ABSENT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NsmlAgli {
    /// The 9 non-seamless angle jump descriptors (angle 1..=9).
    pub cells: [NsmlAngleCell; 9],
}

impl NsmlAgli {
    /// NSML_AGLI start offset within the PCI packet body
    /// (`PCI_GI` is `0x00..0x3C`).
    pub const PACKET_OFFSET: usize = 0x3C;
    /// Size of the NSML_AGLI block: 9 × 4 bytes.
    pub const SIZE: usize = 9 * 4;

    /// Parse NSML_AGLI from a PCI packet body `buf` (slice starting at
    /// `PCI_GI 00`). Reads the 9 angle cells at
    /// [`Self::PACKET_OFFSET`].
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::PACKET_OFFSET + Self::SIZE {
            return Err(Error::InvalidUdf("NSML_AGLI: short buffer"));
        }
        let mut cells = [NsmlAngleCell::default(); 9];
        for (i, slot) in cells.iter_mut().enumerate() {
            *slot = NsmlAngleCell {
                dsta: read_u32_be(buf, Self::PACKET_OFFSET + i * 4)?,
            };
        }
        Ok(Self { cells })
    }

    /// `true` when no angle slot defines a jump
    /// (every cell is [`NsmlAngleCell::ABSENT`]) — i.e. this VOBU is not
    /// part of a non-seamless multi-angle block.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cells.iter().all(NsmlAngleCell::is_absent)
    }

    /// Number of angles that define a real jump for this VOBU
    /// (neither absent nor "no more video").
    #[inline]
    pub fn active_angle_count(&self) -> usize {
        self.cells
            .iter()
            .filter(|c| !c.is_absent() && !c.is_no_more_video())
            .count()
    }

    /// Jump descriptor for a 1-based angle number (`1..=9`).
    ///
    /// Returns `None` if `angle` is out of range. SPRM 3 holds the
    /// player's current 1-based angle; pass it straight through.
    #[inline]
    pub fn angle(&self, angle: u8) -> Option<NsmlAngleCell> {
        if (1..=9).contains(&angle) {
            Some(self.cells[angle as usize - 1])
        } else {
            None
        }
    }
}

/// PCI packet — Presentation Control Information.
///
/// Layout per mpucoder-pci_pkt.html. Surfaces the `PCI_GI` general
/// information block (timing + UOP mask) plus the NSML_AGLI
/// non-seamless angle jump table and, when the VOBU carries a menu, the
/// decoded [`HighlightInfo`] (HLI_GI + SL_COLI + BTN_IT).
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
    /// `NSML_AGLI 00..24` — non-seamless angle jump table (9 cells).
    /// Every cell is [`NsmlAngleCell::ABSENT`] for a single-angle title.
    pub nsml_agli: NsmlAgli,
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
        // NSML_AGLI (0x3C..0x60) sits entirely below HLI_GI (0x60), so
        // the length check above already guarantees these reads.
        let nsml_agli = NsmlAgli::parse(buf)?;
        let highlight = Self::parse_highlight(buf)?;
        Ok(Self {
            nv_pck_lbn: read_u32_be(buf, 0x00)?,
            vobu_cat: read_u16_be(buf, 0x04)?,
            vobu_uop_ctl: read_u32_be(buf, 0x08)?,
            vobu_s_ptm: read_u32_be(buf, 0x0C)?,
            vobu_e_ptm: read_u32_be(buf, 0x10)?,
            vobu_se_e_ptm: read_u32_be(buf, 0x14)?,
            c_eltm: read_u32_be(buf, 0x18)?,
            nsml_agli,
            hli_ss,
            highlight,
        })
    }

    /// Typed view over [`Self::vobu_uop_ctl`].
    ///
    /// The per-VOBU UOP-prohibition mask follows the same 25-bit
    /// layout as the PGC and TT_SRPT levels; this accessor wraps
    /// the raw word so callers can use named
    /// [`crate::uops::UserOp`] variants. Per
    /// `docs/container/dvd/application/mpucoder-uops.html`, a set
    /// bit inhibits the associated control. The spec table leaves
    /// bits 0/1/2/17 (`TimePlayOrSearch` / `PttPlayOrSearch` /
    /// `TitlePlay` / `ButtonSelectOrActivate`) blank for the VOBU
    /// level — those bits are not expected to be set in a valid
    /// PCI packet.
    #[inline]
    pub fn uop_mask(&self) -> crate::uops::UopMask {
        crate::uops::UopMask::from_bits(self.vobu_uop_ctl)
    }

    /// `true` when `op` is **not** prohibited at the VOBU level.
    /// The full player-visible answer is still subject to the
    /// TT_SRPT and PGC masks per the spec's three-level OR rule;
    /// use [`crate::uops::UopMask::merge_or`] to combine.
    #[inline]
    pub fn is_user_op_allowed(&self, op: crate::uops::UserOp) -> bool {
        self.uop_mask().is_allowed(op)
    }

    /// Typed view over [`Self::hli_ss`].
    ///
    /// The lower two bits of `hli_ss` carry the highlight-status
    /// code documented in
    /// `docs/container/dvd/application/mpucoder-pci_pkt.html`. This
    /// accessor returns the [`HighlightStatus`] variant a player
    /// should branch on; the remaining 14 reserved bits stay in
    /// [`Self::hli_ss`] for callers that need the raw word.
    ///
    /// Note that [`Self::highlight`] is populated only when the VOBU
    /// declares geometry — that is, when `btn_ns > 0`. A VOBU with
    /// status [`HighlightStatus::UsePrevious`] or
    /// [`HighlightStatus::UsePreviousExceptCommands`] inherits
    /// geometry from its predecessor and typically reports
    /// `btn_ns == 0` in its own packet; resolving the inherited
    /// `BTN_IT` table is the playback engine's job.
    #[inline]
    pub fn highlight_status(&self) -> HighlightStatus {
        HighlightStatus::from_hli_ss(self.hli_ss)
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

// ------------------------------------------------------------------
// DSI packet (mpucoder-dsi_pkt.html)
// ------------------------------------------------------------------

/// DSI_GI — DSI General Information block.
///
/// 32-byte preamble of the DSI packet, covering the nav-pack system-
/// clock reference, the disc LBA cross-check field (redundant with
/// the PCI half), the VOBU end-address triplet that fast-play uses,
/// the (VOB, cell) identifier pair, and the per-cell elapsed-time
/// BCD field (whose top two bits also encode the frame rate).
///
/// Layout per `mpucoder-dsi_pkt.html`:
///
/// | Offset | Field            | Size |
/// |--------|------------------|------|
/// | `0x00` | `nv_pck_scr`     | 4    |
/// | `0x04` | `nv_pck_lbn`     | 4    |
/// | `0x08` | `vobu_ea`        | 4    |
/// | `0x0C` | `vobu_1stref_ea` | 4    |
/// | `0x10` | `vobu_2ndref_ea` | 4    |
/// | `0x14` | `vobu_3rdref_ea` | 4    |
/// | `0x18` | `vobu_vob_idn`   | 2    |
/// | `0x1A` | reserved (00)    | 1    |
/// | `0x1B` | `vobu_c_idn`     | 1    |
/// | `0x1C` | `c_eltm`         | 4    |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsiGi {
    /// System clock reference for this nav-pack.
    pub nv_pck_scr: u32,
    /// Logical Block Number (sector) of this nav-pack.
    pub nv_pck_lbn: u32,
    /// VOBU end address — relative offset to the last sector of the VOBU.
    pub vobu_ea: u32,
    /// First-reference-frame end block, relative — used for fast play.
    pub vobu_1stref_ea: u32,
    /// Second-reference-frame end block, relative — used for fast play.
    pub vobu_2ndref_ea: u32,
    /// Third-reference-frame end block, relative — used for fast play.
    pub vobu_3rdref_ea: u32,
    /// VOB number containing this VOBU.
    pub vobu_vob_idn: u16,
    /// Cell number within the VOB.
    pub vobu_c_idn: u8,
    /// Cell elapsed time — BCD `hh:mm:ss:ff`. Bits 7&6 of the frame
    /// (last) byte encode the frame rate: `11` = 30 fps, `01` = 25 fps,
    /// other combinations are spec-illegal.
    pub c_eltm: u32,
}

impl DsiGi {
    /// Size of the DSI_GI block — 32 bytes.
    pub const SIZE: usize = 0x20;

    /// Decode the BCD `c_eltm` cell-elapsed-time field into the typed
    /// [`PgcTime`] representation (the same `hh:mm:ss:ff` + 2-bit
    /// frame-rate layout the `PGC_GI` playback-time field uses).
    ///
    /// Per `mpucoder-dsi_pkt.html`: `c_eltm` is a 4-byte BCD field
    /// `hh:mm:ss:ff` with the top two bits of the frame byte encoding
    /// the rate (`11` = 30 fps, `01` = 25 fps; `00` / `10` are
    /// declared illegal). The raw `u32` is decoded big-endian — the
    /// byte at packet offset `0x1C` is `hours`, `0x1D` is `minutes`,
    /// `0x1E` is `seconds`, and `0x1F` is the combined frames /
    /// frame-rate byte — so `to_be_bytes()` yields the exact input
    /// [`PgcTime::from_bytes`] expects.
    pub fn cell_elapsed_time(&self) -> PgcTime {
        PgcTime::from_bytes(self.c_eltm.to_be_bytes())
    }

    /// Convenience: cell elapsed time converted to absolute
    /// nanoseconds via [`PgcTime::to_nanoseconds`]. Lets a playback
    /// engine forward a per-cell duration without going through the
    /// typed intermediate every time.
    pub fn cell_elapsed_ns(&self) -> u64 {
        self.cell_elapsed_time().to_nanoseconds()
    }

    /// Parse the DSI_GI block. `buf` starts at DSI_GI 0x00 (packet
    /// offset 0x00, sector offset 0x407).
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(Error::InvalidUdf("DSI_GI: short buffer"));
        }
        Ok(Self {
            nv_pck_scr: read_u32_be(buf, 0x00)?,
            nv_pck_lbn: read_u32_be(buf, 0x04)?,
            vobu_ea: read_u32_be(buf, 0x08)?,
            vobu_1stref_ea: read_u32_be(buf, 0x0C)?,
            vobu_2ndref_ea: read_u32_be(buf, 0x10)?,
            vobu_3rdref_ea: read_u32_be(buf, 0x14)?,
            vobu_vob_idn: read_u16_be(buf, 0x18)?,
            // 0x1A is "reserved 00"; ignored.
            vobu_c_idn: buf[0x1B],
            c_eltm: read_u32_be(buf, 0x1C)?,
        })
    }
}

/// One audio stream's seamless-playback gap pair within `SmlPbi`.
///
/// Each VOB allows up to two audio "gaps" (places where playback
/// pauses one audio stream to preserve A/V sync across an
/// interleaved-block boundary). Per `mpucoder-dsi_pkt.html` SML_PBI
/// sub-table, each stream gets the same 16-byte quad of
/// `{ stp_ptm1, stp_ptm2, gap_len1, gap_len2 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SmlAudioGap {
    /// PTM of the first audio-gap stop for this stream (90 kHz).
    pub stp_ptm1: u32,
    /// PTM of the second audio-gap stop for this stream (90 kHz).
    pub stp_ptm2: u32,
    /// Duration of the first audio gap (90 kHz clocks).
    pub gap_len1: u32,
    /// Duration of the second audio gap (90 kHz clocks).
    pub gap_len2: u32,
}

/// SML_PBI — Seamless Playback Information.
///
/// 148-byte block at packet offset `0x20..0xB4` carrying the
/// Interleaved-Unit (ILVU) flags + jump-pointer pair that drives
/// seamless playback across interleaved blocks, the (start, end) PTM
/// pair of the surrounding VOB's video span, and the per-audio-
/// stream A/V-sync gap descriptor table (8 streams × 16 bytes).
///
/// Layout per `mpucoder-dsi_pkt.html` SML_PBI sub-table:
///
/// | Packet | Field          | Size |
/// |--------|----------------|------|
/// | `0x20` | `ilvu`         | 2    |
/// | `0x22` | `ilvu_ea`      | 4    |
/// | `0x26` | `nxt_ilvu_sa`  | 4    |
/// | `0x2A` | `nxt_ilvu_sz`  | 2    |
/// | `0x2C` | `vob_v_s_ptm`  | 4    |
/// | `0x30` | `vob_v_e_ptm`  | 4    |
/// | `0x34..0xB4` | 8 × [`SmlAudioGap`] | 16 each |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmlPbi {
    /// ILVU flag word. Only the top 4 bits of the first byte are
    /// defined; the rest is reserved-zero. Decode via
    /// [`SmlPbi::preu`], [`SmlPbi::is_ilvu`], [`SmlPbi::unit_start`]
    /// and [`SmlPbi::unit_end`].
    pub ilvu: u16,
    /// ILVU end address — relative offset to the last sector of this
    /// ILVU for the current angle/scene. `0x0000_0000` for PREU and
    /// non-interleaved blocks.
    pub ilvu_ea: u32,
    /// Relative offset to the next ILVU block (not VOBU) for the
    /// current angle/scene. `0x0000_0000` for PREU / non-interleaved
    /// blocks; `0xFFFF_FFFF` marks the end of interleaving.
    pub nxt_ilvu_sa: u32,
    /// Size of the next ILVU block (sectors). `0x0000` for PREU /
    /// non-interleaved blocks; `0xFFFF` marks the end of interleaving.
    pub nxt_ilvu_sz: u16,
    /// PTM of the first video frame in the first GOP of this VOB.
    pub vob_v_s_ptm: u32,
    /// PTM of the last video frame in the last GOP of this VOB.
    pub vob_v_e_ptm: u32,
    /// Per-audio-stream A/V-sync gap pairs (8 streams).
    pub audio_gaps: [SmlAudioGap; 8],
}

impl SmlPbi {
    /// Size of the SML_PBI block — 148 bytes.
    pub const SIZE: usize = 0xB4 - 0x20;
    /// SML_PBI start offset within the DSI packet body.
    pub const PACKET_OFFSET: usize = 0x20;

    /// PREU flag (ILVU bit 15) — set during the last 3 VOBUs preceding
    /// an interleaved block.
    pub fn preu(&self) -> bool {
        self.ilvu & 0x8000 != 0
    }
    /// ILVU flag (ILVU bit 14) — set for every VOBU inside an
    /// interleaved block.
    pub fn is_ilvu(&self) -> bool {
        self.ilvu & 0x4000 != 0
    }
    /// Unit_Start flag (ILVU bit 13) — set for the first VOBU of an
    /// angle/scene within an ILVU, or the first VOBU of a PREU run.
    pub fn unit_start(&self) -> bool {
        self.ilvu & 0x2000 != 0
    }
    /// Unit_End flag (ILVU bit 12) — set for the last VOBU of an
    /// angle/scene within an ILVU, or the last VOBU of a PREU run.
    pub fn unit_end(&self) -> bool {
        self.ilvu & 0x1000 != 0
    }

    /// Parse SML_PBI given a slice starting at packet offset
    /// [`Self::PACKET_OFFSET`].
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(Error::InvalidUdf("SML_PBI: short buffer"));
        }
        let mut audio_gaps = [SmlAudioGap::default(); 8];
        for (i, slot) in audio_gaps.iter_mut().enumerate() {
            // First audio stream starts at SML_PBI 0x14 (packet 0x34);
            // each subsequent stream is 16 bytes further on.
            let base = 0x14 + i * 16;
            *slot = SmlAudioGap {
                stp_ptm1: read_u32_be(buf, base)?,
                stp_ptm2: read_u32_be(buf, base + 4)?,
                gap_len1: read_u32_be(buf, base + 8)?,
                gap_len2: read_u32_be(buf, base + 12)?,
            };
        }
        Ok(Self {
            ilvu: read_u16_be(buf, 0x00)?,
            ilvu_ea: read_u32_be(buf, 0x02)?,
            nxt_ilvu_sa: read_u32_be(buf, 0x06)?,
            nxt_ilvu_sz: read_u16_be(buf, 0x0A)?,
            vob_v_s_ptm: read_u32_be(buf, 0x0C)?,
            vob_v_e_ptm: read_u32_be(buf, 0x10)?,
            audio_gaps,
        })
    }
}

/// One angle's seamless-playback descriptor within `SmlAgli`.
///
/// Per `mpucoder-dsi_pkt.html` SML_AGLI sub-table, every angle gets a
/// 6-byte `{ dsta: u32, sz: u16 }` record. `dsta == 0` flags an
/// absent angle; `dsta == 0x7FFF_FFFF` means no more video for the
/// angle; bit 31 toggles forward (0) versus backward (1) direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SmlAngleCell {
    /// Relative offset to the NEXT ILVU for this angle. Bit 31 is the
    /// direction (0 = forward, 1 = backward). `0x0000_0000` = angle
    /// absent; `0x7FFF_FFFF` = no more video for this angle.
    pub dsta: u32,
    /// ILVU size in sectors for this angle.
    pub sz: u16,
}

/// SML_AGLI — Seamless Angle Information.
///
/// 54-byte block at packet offset `0xB4..0xEA` covering the 9 angle
/// cells of the multi-angle interleaved block. Each cell occupies
/// 6 bytes (`dsta` 4 + `sz` 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmlAgli {
    /// SML_AGLI start offset within the DSI packet body.
    /// 9 angle cells per `mpucoder-dsi_pkt.html`.
    pub cells: [SmlAngleCell; 9],
}

impl SmlAgli {
    /// Size of the SML_AGLI block — 54 bytes.
    pub const SIZE: usize = 0xEA - 0xB4;
    /// SML_AGLI start offset within the DSI packet body.
    pub const PACKET_OFFSET: usize = 0xB4;

    /// Parse SML_AGLI given a slice starting at packet offset
    /// [`Self::PACKET_OFFSET`].
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(Error::InvalidUdf("SML_AGLI: short buffer"));
        }
        let mut cells = [SmlAngleCell::default(); 9];
        for (i, slot) in cells.iter_mut().enumerate() {
            // Each cell is 6 bytes wide.
            let base = i * 6;
            *slot = SmlAngleCell {
                dsta: read_u32_be(buf, base)?,
                sz: read_u16_be(buf, base + 4)?,
            };
        }
        Ok(Self { cells })
    }
}

/// VOBU_SRI — VOBU Search Information.
///
/// 168-byte block at packet offset `0xEA..0x192`. The fast-seek
/// table at the heart of DVD playback: 19 forward + 19 backward
/// scaled jump-pointers, plus the four bracket pointers (next/prev
/// VOBU with video, next/prev VOBU with possible video).
///
/// Per `mpucoder-dsi_pkt.html`, every entry encodes:
///
/// - bit 31 = "valid pointer" flag.
/// - bit 30 (forward/backward span entries only) = "one or more
///   VOBUs are present between this reference and the reference
///   closer to the current VOBU".
/// - the remaining 30 bits are the relative VOBU offset.
///
/// Sentinel values:
///
/// - forward span: `0x3FFF_FFFF` = no VOBU within the cell for this span.
/// - backward span: `0x3FFF_FFFF` = no VOBU within the cell for this span.
/// - `sri_nvwv` / `sri_pvwv`: `0xBFFF_FFFF` = no following/preceding
///   VOBU contains video.
/// - `sri_nv` / `sri_pv`: `0x3FFF_FFFF` = no following/preceding VOBU.
///
/// `forward` / `backward` are decoded as raw 19-entry tables; the
/// caller masks bits 30/31 with the helpers below to recover the
/// offset + flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VobuSri {
    /// Offset to the next VOBU **with video** (forward; bit 31 = valid).
    pub sri_nvwv: u32,
    /// Forward span entries — relative VOBU offsets for 19 fixed
    /// fast-forward scrub distances. First entry is `sri_fwdi240`;
    /// the remaining 18 are `sri_fwda*` (decreasing time spans down
    /// to `sri_fwda1`).
    pub forward: [u32; 19],
    /// Offset to the next VOBU with possible video.
    pub sri_nv: u32,
    /// Offset to the previous VOBU with possible video (backward).
    pub sri_pv: u32,
    /// Backward span entries — 19 fixed scrub distances mirroring the
    /// forward table (`sri_bwda1` first through `sri_bwda240` last).
    pub backward: [u32; 19],
    /// Offset to the previous VOBU **with video** (backward; bit 31 = valid).
    pub sri_pvwv: u32,
}

impl VobuSri {
    /// Size of the VOBU_SRI block — 168 bytes (42 × 4-byte entries).
    pub const SIZE: usize = 0x192 - 0xEA;
    /// VOBU_SRI start offset within the DSI packet body.
    pub const PACKET_OFFSET: usize = 0xEA;
    /// Bit-31 "valid pointer" mask shared by every entry.
    pub const VALID_BIT: u32 = 0x8000_0000;
    /// Bit-30 "VOBUs present between this and the closer reference"
    /// mask, shared by the forward / backward span entries.
    pub const INTERMEDIATE_BIT: u32 = 0x4000_0000;
    /// Mask isolating the 30-bit relative-offset payload.
    pub const OFFSET_MASK: u32 = 0x3FFF_FFFF;

    /// Parse VOBU_SRI given a slice starting at packet offset
    /// [`Self::PACKET_OFFSET`].
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(Error::InvalidUdf("VOBU_SRI: short buffer"));
        }
        let sri_nvwv = read_u32_be(buf, 0x00)?;
        let mut forward = [0u32; 19];
        for (i, slot) in forward.iter_mut().enumerate() {
            // Forward entries occupy 0x04..0x50 (relative to VOBU_SRI),
            // matching packet offsets 0xEE..0x13A.
            *slot = read_u32_be(buf, 0x04 + i * 4)?;
        }
        let sri_nv = read_u32_be(buf, 0x50)?; // packet 0x13A
        let sri_pv = read_u32_be(buf, 0x54)?; // packet 0x13E
        let mut backward = [0u32; 19];
        for (i, slot) in backward.iter_mut().enumerate() {
            // Backward entries occupy 0x58..0xA4 (relative to VOBU_SRI),
            // matching packet offsets 0x142..0x18E.
            *slot = read_u32_be(buf, 0x58 + i * 4)?;
        }
        let sri_pvwv = read_u32_be(buf, 0xA4)?; // packet 0x18E
        Ok(Self {
            sri_nvwv,
            forward,
            sri_nv,
            sri_pv,
            backward,
            sri_pvwv,
        })
    }
}

/// SYNCI — Sync Information.
///
/// 144-byte block at packet offset `0x192..0x222` covering the
/// per-substream "where does this VOBU's first audio / subpicture
/// packet live" pointers an A/V-sync renderer uses to align tracks.
///
/// - `a_synca[0..8]`: 8 audio-stream pointers (2 bytes each). Bit 15
///   is the direction (0 = forward, 1 = backward). `0x0000` =
///   stream absent; `0x3FFF` = no more audio for this stream.
/// - `sp_synca[0..32]`: 32 subpicture-stream pointers (4 bytes each).
///   Bit 31 is the direction. `0x0000_0000` = subpicture absent;
///   `0x3FFF_FFFF` = no subpicture data for this VOBU;
///   `0x7FFF_FFFF` = subpicture data is contained inside this VOBU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Synci {
    /// Per-audio-stream "first audio packet" relative offsets.
    pub a_synca: [u16; 8],
    /// Per-subpicture-stream "first subpicture packet" relative offsets.
    pub sp_synca: [u32; 32],
}

impl Synci {
    /// Size of the SYNCI block — 144 bytes (8 × 2 + 32 × 4).
    pub const SIZE: usize = 0x222 - 0x192;
    /// SYNCI start offset within the DSI packet body.
    pub const PACKET_OFFSET: usize = 0x192;
    /// Audio-pointer direction bit (1 = backward).
    pub const AUDIO_DIRECTION_BIT: u16 = 0x8000;
    /// Subpicture-pointer direction bit (1 = backward).
    pub const SP_DIRECTION_BIT: u32 = 0x8000_0000;

    /// Parse SYNCI given a slice starting at packet offset
    /// [`Self::PACKET_OFFSET`].
    fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(Error::InvalidUdf("SYNCI: short buffer"));
        }
        let mut a_synca = [0u16; 8];
        for (i, slot) in a_synca.iter_mut().enumerate() {
            *slot = read_u16_be(buf, i * 2)?;
        }
        let mut sp_synca = [0u32; 32];
        for (i, slot) in sp_synca.iter_mut().enumerate() {
            *slot = read_u32_be(buf, 0x10 + i * 4)?;
        }
        Ok(Self { a_synca, sp_synca })
    }
}

/// DSI packet — Data Search Information (NAV substream `0x01`).
///
/// Layout per `mpucoder-dsi_pkt.html`. The DSI packet body extends
/// from sector offset `0x407` to `0x629` (packet offsets
/// `0x000..0x222`) covering five concatenated sub-sections:
///
/// | Packet | Block        | Size | Field                                  |
/// |--------|--------------|------|----------------------------------------|
/// | `0x00` | [`DsiGi`]    | 32   | general info + (VOB, cell) ID + c_eltm |
/// | `0x20` | [`SmlPbi`]   | 148  | seamless-playback info (ILVU + gaps)   |
/// | `0xB4` | [`SmlAgli`]  | 54   | seamless-angle info (9 angles)         |
/// | `0xEA` | [`VobuSri`]  | 168  | fast-seek pointer table                |
/// | `0x192`| [`Synci`]    | 144  | A/V sync pointer table                 |
///
/// Total decoded payload: 546 bytes. The remainder of the 0x3FA-
/// byte private-stream-2 packet is reserved-zero per spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsiPacket {
    /// DSI_GI — general information block (packet `0x00..0x20`).
    pub general_info: DsiGi,
    /// SML_PBI — seamless-playback information (packet `0x20..0xB4`).
    pub sml_pbi: SmlPbi,
    /// SML_AGLI — seamless-angle information (packet `0xB4..0xEA`).
    pub sml_agli: SmlAgli,
    /// VOBU_SRI — fast-seek pointer table (packet `0xEA..0x192`).
    pub vobu_sri: VobuSri,
    /// SYNCI — A/V sync pointer table (packet `0x192..0x222`).
    pub synci: Synci,
}

impl DsiPacket {
    /// Total decoded DSI body size — 546 bytes (DSI_GI + SML_PBI +
    /// SML_AGLI + VOBU_SRI + SYNCI). The on-wire packet is 0x3FA
    /// bytes; bytes past `BODY_SIZE` are reserved-zero per spec.
    pub const BODY_SIZE: usize = Synci::PACKET_OFFSET + Synci::SIZE;

    // Convenience getters that mirror the previous (flat) DSI_GI API
    // so call-sites that touched the old fields directly keep working.

    /// Shortcut for `general_info.nv_pck_scr`.
    pub fn nv_pck_scr(&self) -> u32 {
        self.general_info.nv_pck_scr
    }
    /// Shortcut for `general_info.nv_pck_lbn`.
    pub fn nv_pck_lbn(&self) -> u32 {
        self.general_info.nv_pck_lbn
    }
    /// Shortcut for `general_info.vobu_ea`.
    pub fn vobu_ea(&self) -> u32 {
        self.general_info.vobu_ea
    }
    /// Shortcut for `general_info.vobu_vob_idn`.
    pub fn vobu_vob_idn(&self) -> u16 {
        self.general_info.vobu_vob_idn
    }
    /// Shortcut for `general_info.vobu_c_idn`.
    pub fn vobu_c_idn(&self) -> u8 {
        self.general_info.vobu_c_idn
    }
    /// Shortcut for `general_info.c_eltm`.
    pub fn c_eltm(&self) -> u32 {
        self.general_info.c_eltm
    }

    /// Shortcut for [`DsiGi::cell_elapsed_time`] — surfaces the typed
    /// BCD `hh:mm:ss:ff` + frame-rate decoding of the `c_eltm` field.
    pub fn cell_elapsed_time(&self) -> PgcTime {
        self.general_info.cell_elapsed_time()
    }

    /// Shortcut for [`DsiGi::cell_elapsed_ns`] — surfaces the
    /// nanosecond-precision cell elapsed time.
    pub fn cell_elapsed_ns(&self) -> u64 {
        self.general_info.cell_elapsed_ns()
    }

    /// Parse a DSI packet body. `buf` starts at `DSI_GI 00` (sector
    /// offset `0x407`).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::BODY_SIZE {
            return Err(Error::InvalidUdf(
                "DSI: shorter than DSI_GI + SML_PBI + SML_AGLI + VOBU_SRI + SYNCI",
            ));
        }
        Ok(Self {
            general_info: DsiGi::parse(&buf[0x00..])?,
            sml_pbi: SmlPbi::parse(&buf[SmlPbi::PACKET_OFFSET..])?,
            sml_agli: SmlAgli::parse(&buf[SmlAgli::PACKET_OFFSET..])?,
            vobu_sri: VobuSri::parse(&buf[VobuSri::PACKET_OFFSET..])?,
            synci: Synci::parse(&buf[Synci::PACKET_OFFSET..])?,
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
        assert_eq!(nav.dsi.general_info.nv_pck_lbn, 0xDEAD_BEEF);
        assert_eq!(nav.dsi.general_info.vobu_ea, 0x0000_07FF);
        assert_eq!(nav.dsi.nv_pck_lbn(), 0xDEAD_BEEF);
        assert_eq!(nav.dsi.vobu_ea(), 0x0000_07FF);
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

    // ----- PCI NSML_AGLI (non-seamless angle table) ----------------

    #[test]
    fn pci_default_nsml_agli_is_empty() {
        // A nav sector with no angle data leaves NSML_AGLI all-zero.
        let sector = build_nav_sector(1, 0, 0);
        let nav = NavPack::parse(&sector).unwrap();
        assert!(nav.pci.nsml_agli.is_empty());
        assert_eq!(nav.pci.nsml_agli.active_angle_count(), 0);
        for c in &nav.pci.nsml_agli.cells {
            assert!(c.is_absent());
            assert!(!c.is_no_more_video());
            assert_eq!(c.offset_sectors(), None);
        }
    }

    #[test]
    fn pci_parses_nsml_agli_block() {
        let mut sector = build_nav_sector(1, 0, 0);
        // NSML_AGLI begins at PCI packet 0x3C; 9 cells × 4 bytes.
        // Angle 1: forward jump of 0x100 sectors.
        sector[pci(0x3C)..pci(0x40)].copy_from_slice(&0x0000_0100u32.to_be_bytes());
        // Angle 2: backward jump of 0x080 sectors (bit 31 set).
        sector[pci(0x40)..pci(0x44)].copy_from_slice(&0x8000_0080u32.to_be_bytes());
        // Angle 3: "no more video" sentinel.
        sector[pci(0x44)..pci(0x48)].copy_from_slice(&NsmlAngleCell::NO_MORE_VIDEO.to_be_bytes());
        // Angles 4..=9 stay absent (0x0000_0000).
        // Angle 9 explicitly written to prove the last cell is reached.
        sector[pci(0x5C)..pci(0x60)].copy_from_slice(&0x0000_0009u32.to_be_bytes());

        let nav = NavPack::parse(&sector).unwrap();
        let agli = &nav.pci.nsml_agli;
        assert!(!agli.is_empty());

        // Angle 1 — forward, magnitude 0x100.
        let a1 = agli.angle(1).unwrap();
        assert_eq!(a1.dsta, 0x0000_0100);
        assert!(!a1.is_backward());
        assert!(!a1.is_absent());
        assert_eq!(a1.offset_sectors(), Some(0x100));

        // Angle 2 — backward, magnitude 0x080.
        let a2 = agli.angle(2).unwrap();
        assert!(a2.is_backward());
        assert_eq!(a2.offset_sectors(), Some(0x080));

        // Angle 3 — no more video (no real offset).
        let a3 = agli.angle(3).unwrap();
        assert!(a3.is_no_more_video());
        assert_eq!(a3.offset_sectors(), None);

        // Angle 4 — absent.
        assert!(agli.angle(4).unwrap().is_absent());

        // Angle 9 — last cell parsed.
        assert_eq!(agli.angle(9).unwrap().dsta, 0x0000_0009);

        // Active = angles that encode a real jump (1, 2, 9).
        assert_eq!(agli.active_angle_count(), 3);

        // Out-of-range angle numbers reject.
        assert_eq!(agli.angle(0), None);
        assert_eq!(agli.angle(10), None);
    }

    #[test]
    fn nsml_agli_parse_rejects_short_buffer() {
        // Truncated before the NSML_AGLI block (0x3C + 36 = 0x60).
        let buf = [0u8; 0x40];
        assert!(NsmlAgli::parse(&buf).is_err());
        // Exactly enough bytes for NSML_AGLI parses.
        let buf = [0u8; 0x60];
        assert!(NsmlAgli::parse(&buf).is_ok());
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
        // btn_md 0x0100: group_count = (0x0100>>12)&3 = 0; btngr1_ty =
        // (0x0100>>8)&7 = 1; remaining group types zero.
        let bm = hli.button_mode();
        assert_eq!(bm.group_count, 0);
        assert_eq!(bm.group_types, [1, 0, 0]);
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
    fn button_mode_decodes_btn_md_subfields() {
        // Construct a btn_md with distinct values in every documented
        // field and reserved bits all set, to confirm the masks isolate
        // the right ranges. Per mpucoder-pci_pkt.html `btn_md word`:
        //   bits 13..12 = btngr_ns, 10..8 = btngr1_ty,
        //   6..4 = btngr2_ty, 2..0 = btngr3_ty; bits 15,14,11,7,3 rsv.
        // Pick: count = 0b10 (2), ty1 = 0b101 (5), ty2 = 0b011 (3),
        // ty3 = 0b110 (6); set all reserved bits.
        let reserved = (1u16 << 15) | (1 << 14) | (1 << 11) | (1 << 7) | (1 << 3);
        let btn_md = reserved | (0b10 << 12) | (0b101 << 8) | (0b011 << 4) | 0b110;
        let bm = ButtonMode::from_btn_md(btn_md);
        assert_eq!(bm.group_count, 2);
        assert_eq!(bm.group_types, [5, 3, 6]);
        // Round-trip drops reserved bits but preserves the meaningful
        // fields.
        let cleaned = (0b10u16 << 12) | (0b101 << 8) | (0b011 << 4) | 0b110;
        assert_eq!(bm.to_btn_md(), cleaned);
        // Decoding the cleaned word yields an identical view.
        assert_eq!(ButtonMode::from_btn_md(cleaned), bm);
    }

    #[test]
    fn button_mode_zero_is_default() {
        // A button-less / single-group VOBU commonly leaves btn_md
        // zero: no groups, all type codes zero.
        let bm = ButtonMode::from_btn_md(0);
        assert_eq!(bm, ButtonMode::default());
        assert_eq!(bm.group_count, 0);
        assert_eq!(bm.group_types, [0, 0, 0]);
        assert_eq!(bm.to_btn_md(), 0);
    }

    #[test]
    fn highlight_status_maps_two_low_bits_to_named_variants() {
        // Per mpucoder-pci_pkt.html the four 2-bit codes are
        // exhaustively assigned; the upper 14 bits are reserved
        // and must not influence the typed decode.
        for reserved in [0x0000u16, 0xFFFCu16, 0xA5A4u16] {
            assert_eq!(
                HighlightStatus::from_hli_ss(reserved),
                HighlightStatus::None
            );
            assert_eq!(
                HighlightStatus::from_hli_ss(reserved | 0b01),
                HighlightStatus::AllNew
            );
            assert_eq!(
                HighlightStatus::from_hli_ss(reserved | 0b10),
                HighlightStatus::UsePrevious
            );
            assert_eq!(
                HighlightStatus::from_hli_ss(reserved | 0b11),
                HighlightStatus::UsePreviousExceptCommands
            );
        }
    }

    #[test]
    fn highlight_status_classifier_predicates_match_spec_table() {
        // `None` is the only variant flagged as carrying no
        // highlight at all.
        assert!(HighlightStatus::None.is_none());
        assert!(!HighlightStatus::AllNew.is_none());
        assert!(!HighlightStatus::UsePrevious.is_none());
        assert!(!HighlightStatus::UsePreviousExceptCommands.is_none());

        // Only `AllNew` declares fresh geometry; the two re-use
        // variants inherit it.
        assert!(!HighlightStatus::None.declares_new_geometry());
        assert!(HighlightStatus::AllNew.declares_new_geometry());
        assert!(!HighlightStatus::UsePrevious.declares_new_geometry());
        assert!(!HighlightStatus::UsePreviousExceptCommands.declares_new_geometry());

        // Both re-use variants are the only ones that inherit
        // predecessor geometry.
        assert!(!HighlightStatus::None.reuses_previous_geometry());
        assert!(!HighlightStatus::AllNew.reuses_previous_geometry());
        assert!(HighlightStatus::UsePrevious.reuses_previous_geometry());
        assert!(HighlightStatus::UsePreviousExceptCommands.reuses_previous_geometry());

        // Per the spec table, `AllNew` and
        // `UsePreviousExceptCommands` are the two variants where the
        // current VOBU's `BTN_IT` supplies the per-button commands.
        assert!(!HighlightStatus::None.supplies_own_commands());
        assert!(HighlightStatus::AllNew.supplies_own_commands());
        assert!(!HighlightStatus::UsePrevious.supplies_own_commands());
        assert!(HighlightStatus::UsePreviousExceptCommands.supplies_own_commands());
    }

    #[test]
    fn highlight_status_to_bits_round_trips_through_from_hli_ss() {
        for status in [
            HighlightStatus::None,
            HighlightStatus::AllNew,
            HighlightStatus::UsePrevious,
            HighlightStatus::UsePreviousExceptCommands,
        ] {
            assert_eq!(HighlightStatus::from_hli_ss(status.to_bits()), status);
        }
        // Reserved upper bits must be ignored on the decode side, so
        // an `hli_ss` word with any pattern in bits 15..2 still
        // round-trips its low-2-bit semantics.
        for raw in 0u16..=0xFFFF {
            let status = HighlightStatus::from_hli_ss(raw);
            assert_eq!(status.to_bits(), raw & 0b11);
        }
    }

    #[test]
    fn pci_highlight_status_accessor_matches_raw_word() {
        // Button-less VOBU — hli_ss == 0 → `None`.
        let sector = build_nav_sector(1, 0, 0);
        let nav = NavPack::parse(&sector).unwrap();
        assert_eq!(nav.pci.hli_ss & 0b11, 0);
        assert_eq!(nav.pci.highlight_status(), HighlightStatus::None);

        // Single-button VOBU — hli_ss == 0x0001 → `AllNew`.
        let mut sector = build_nav_sector(1, 0, 0);
        add_one_button_hli(&mut sector);
        let nav = NavPack::parse(&sector).unwrap();
        assert_eq!(nav.pci.hli_ss, 0x0001);
        assert_eq!(nav.pci.highlight_status(), HighlightStatus::AllNew);
        assert!(nav.pci.highlight_status().declares_new_geometry());
        assert!(nav.pci.highlight_status().supplies_own_commands());

        // Forge a "use previous" status word on a button-less sector
        // by writing 0x0002 into HLI_GI.00 and exercise the typed
        // accessor against the raw word. The decoder still skips
        // `parse_highlight` because btn_ns == 0; the typed status is
        // independent of geometry presence.
        let mut sector = build_nav_sector(1, 0, 0);
        sector[pci(0x60)] = 0x00;
        sector[pci(0x61)] = 0x02;
        let nav = NavPack::parse(&sector).unwrap();
        assert_eq!(nav.pci.hli_ss, 0x0002);
        assert_eq!(nav.pci.highlight_status(), HighlightStatus::UsePrevious);
        assert!(nav.pci.highlight_status().reuses_previous_geometry());
        assert!(!nav.pci.highlight_status().supplies_own_commands());
        assert!(nav.pci.highlight.is_none());

        // And the "previous geometry, fresh commands" word 0x0003.
        let mut sector = build_nav_sector(1, 0, 0);
        sector[pci(0x60)] = 0x00;
        sector[pci(0x61)] = 0x03;
        let nav = NavPack::parse(&sector).unwrap();
        assert_eq!(nav.pci.hli_ss, 0x0003);
        assert_eq!(
            nav.pci.highlight_status(),
            HighlightStatus::UsePreviousExceptCommands
        );
        assert!(nav.pci.highlight_status().reuses_previous_geometry());
        assert!(nav.pci.highlight_status().supplies_own_commands());
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

    // ----- DSI sub-section layout pins (mpucoder-dsi_pkt.html) ------

    /// Compile-time sanity for the DSI section-size + offset map. Every
    /// constant below is the spec-listed width of one sub-section; the
    /// running sum has to match the next section's start offset (or
    /// the total DSI body size).
    #[test]
    fn dsi_section_offsets_match_spec() {
        // DsiGi spans packet 0x00..0x20.
        assert_eq!(DsiGi::SIZE, 0x20);
        // SML_PBI is at packet 0x20..0xB4 (148 bytes).
        assert_eq!(SmlPbi::PACKET_OFFSET, 0x20);
        assert_eq!(SmlPbi::SIZE, 0xB4 - 0x20);
        assert_eq!(SmlPbi::PACKET_OFFSET + SmlPbi::SIZE, SmlAgli::PACKET_OFFSET);
        // SML_AGLI is at packet 0xB4..0xEA (54 bytes).
        assert_eq!(SmlAgli::PACKET_OFFSET, 0xB4);
        assert_eq!(SmlAgli::SIZE, 0xEA - 0xB4);
        assert_eq!(
            SmlAgli::PACKET_OFFSET + SmlAgli::SIZE,
            VobuSri::PACKET_OFFSET
        );
        // VOBU_SRI is at packet 0xEA..0x192 (168 bytes = 42 × 4).
        assert_eq!(VobuSri::PACKET_OFFSET, 0xEA);
        assert_eq!(VobuSri::SIZE, 0x192 - 0xEA);
        assert_eq!(VobuSri::SIZE, 42 * 4);
        assert_eq!(VobuSri::PACKET_OFFSET + VobuSri::SIZE, Synci::PACKET_OFFSET);
        // SYNCI is at packet 0x192..0x222 (144 bytes = 8×2 + 32×4).
        assert_eq!(Synci::PACKET_OFFSET, 0x192);
        assert_eq!(Synci::SIZE, 0x222 - 0x192);
        assert_eq!(Synci::SIZE, 8 * 2 + 32 * 4);
        // Total decoded DSI body = 546 bytes.
        assert_eq!(DsiPacket::BODY_SIZE, 0x222);
    }

    /// Build a DSI packet body (546 bytes, packet offsets `0x00..0x222`)
    /// with every spec-listed field set to a unique sentinel so the
    /// parse-side asserts can pin every offset exactly.
    fn build_dsi_body() -> Vec<u8> {
        let mut buf = vec![0u8; DsiPacket::BODY_SIZE];

        // ---- DSI_GI (packet 0x00..0x20) ----
        buf[0x00..0x04].copy_from_slice(&0x1111_2222u32.to_be_bytes()); // nv_pck_scr
        buf[0x04..0x08].copy_from_slice(&0x3333_4444u32.to_be_bytes()); // nv_pck_lbn
        buf[0x08..0x0C].copy_from_slice(&0x5555_6666u32.to_be_bytes()); // vobu_ea
        buf[0x0C..0x10].copy_from_slice(&0x7777_8888u32.to_be_bytes()); // vobu_1stref_ea
        buf[0x10..0x14].copy_from_slice(&0x9999_AAAAu32.to_be_bytes()); // vobu_2ndref_ea
        buf[0x14..0x18].copy_from_slice(&0xBBBB_CCCCu32.to_be_bytes()); // vobu_3rdref_ea
        buf[0x18..0x1A].copy_from_slice(&0xDEADu16.to_be_bytes()); // vobu_vob_idn
        buf[0x1A] = 0x00; // reserved
        buf[0x1B] = 0x42; // vobu_c_idn
        buf[0x1C..0x20].copy_from_slice(&0xC0DE_F00Du32.to_be_bytes()); // c_eltm

        // ---- SML_PBI (packet 0x20..0xB4) ----
        // ilvu: PREU | ILVU | Unit_Start | Unit_End = 0xF000
        buf[0x20..0x22].copy_from_slice(&0xF000u16.to_be_bytes());
        buf[0x22..0x26].copy_from_slice(&0x0123_4567u32.to_be_bytes()); // ilvu_ea
        buf[0x26..0x2A].copy_from_slice(&0x89AB_CDEFu32.to_be_bytes()); // nxt_ilvu_sa
        buf[0x2A..0x2C].copy_from_slice(&0xFFFFu16.to_be_bytes()); // nxt_ilvu_sz
        buf[0x2C..0x30].copy_from_slice(&0x0000_AAAAu32.to_be_bytes()); // vob_v_s_ptm
        buf[0x30..0x34].copy_from_slice(&0x0000_BBBBu32.to_be_bytes()); // vob_v_e_ptm
                                                                        // 8 audio streams × 16 bytes each (stp_ptm1, stp_ptm2, gap_len1, gap_len2)
        for stream in 0..8u32 {
            let base = 0x34 + stream as usize * 16;
            buf[base..base + 4].copy_from_slice(&(0x1000_0000 + stream).to_be_bytes());
            buf[base + 4..base + 8].copy_from_slice(&(0x2000_0000 + stream).to_be_bytes());
            buf[base + 8..base + 12].copy_from_slice(&(0x3000_0000 + stream).to_be_bytes());
            buf[base + 12..base + 16].copy_from_slice(&(0x4000_0000 + stream).to_be_bytes());
        }

        // ---- SML_AGLI (packet 0xB4..0xEA) ---- 9 cells × 6 bytes
        for cell in 0..9u32 {
            let base = 0xB4 + cell as usize * 6;
            buf[base..base + 4].copy_from_slice(&(0x5000_0000 + cell).to_be_bytes());
            buf[base + 4..base + 6].copy_from_slice(&(0x6000u16 + cell as u16).to_be_bytes());
        }

        // ---- VOBU_SRI (packet 0xEA..0x192) ----
        buf[0xEA..0xEE].copy_from_slice(&0x8000_0001u32.to_be_bytes()); // sri_nvwv (valid, +1)
        for i in 0..19u32 {
            let off = 0xEE + i as usize * 4;
            buf[off..off + 4].copy_from_slice(&(0x9000_0000 + i).to_be_bytes());
            // forward
        }
        buf[0x13A..0x13E].copy_from_slice(&0x8000_0002u32.to_be_bytes()); // sri_nv
        buf[0x13E..0x142].copy_from_slice(&0x8000_0003u32.to_be_bytes()); // sri_pv
        for i in 0..19u32 {
            let off = 0x142 + i as usize * 4;
            buf[off..off + 4].copy_from_slice(&(0xA000_0000 + i).to_be_bytes());
            // backward
        }
        buf[0x18E..0x192].copy_from_slice(&0x8000_0004u32.to_be_bytes()); // sri_pvwv

        // ---- SYNCI (packet 0x192..0x222) ----
        for i in 0..8u16 {
            let off = 0x192 + i as usize * 2;
            buf[off..off + 2].copy_from_slice(&(0x7000u16 + i).to_be_bytes());
        }
        for i in 0..32u32 {
            let off = 0x1A2 + i as usize * 4;
            buf[off..off + 4].copy_from_slice(&(0xB000_0000 + i).to_be_bytes());
        }

        buf
    }

    #[test]
    fn dsi_parses_general_info_block() {
        let buf = build_dsi_body();
        let dsi = DsiPacket::parse(&buf).expect("DSI body parses");
        let gi = dsi.general_info;
        assert_eq!(gi.nv_pck_scr, 0x1111_2222);
        assert_eq!(gi.nv_pck_lbn, 0x3333_4444);
        assert_eq!(gi.vobu_ea, 0x5555_6666);
        assert_eq!(gi.vobu_1stref_ea, 0x7777_8888);
        assert_eq!(gi.vobu_2ndref_ea, 0x9999_AAAA);
        assert_eq!(gi.vobu_3rdref_ea, 0xBBBB_CCCC);
        assert_eq!(gi.vobu_vob_idn, 0xDEAD);
        assert_eq!(gi.vobu_c_idn, 0x42);
        assert_eq!(gi.c_eltm, 0xC0DE_F00D);
        // Convenience getters mirror the field accessors.
        assert_eq!(dsi.nv_pck_scr(), 0x1111_2222);
        assert_eq!(dsi.nv_pck_lbn(), 0x3333_4444);
        assert_eq!(dsi.vobu_ea(), 0x5555_6666);
        assert_eq!(dsi.vobu_vob_idn(), 0xDEAD);
        assert_eq!(dsi.vobu_c_idn(), 0x42);
        assert_eq!(dsi.c_eltm(), 0xC0DE_F00D);
    }

    #[test]
    fn dsi_parses_sml_pbi_block_and_ilvu_flags() {
        let buf = build_dsi_body();
        let dsi = DsiPacket::parse(&buf).expect("DSI body parses");
        let pbi = dsi.sml_pbi;
        assert_eq!(pbi.ilvu, 0xF000);
        // The four ILVU flags occupy the top 4 bits of byte 0.
        assert!(pbi.preu());
        assert!(pbi.is_ilvu());
        assert!(pbi.unit_start());
        assert!(pbi.unit_end());
        assert_eq!(pbi.ilvu_ea, 0x0123_4567);
        assert_eq!(pbi.nxt_ilvu_sa, 0x89AB_CDEF);
        assert_eq!(pbi.nxt_ilvu_sz, 0xFFFF);
        assert_eq!(pbi.vob_v_s_ptm, 0x0000_AAAA);
        assert_eq!(pbi.vob_v_e_ptm, 0x0000_BBBB);
        // Audio gap table — 8 streams, every field tagged with the
        // stream index so an off-by-one in the stride would surface.
        for (i, gap) in pbi.audio_gaps.iter().enumerate() {
            let i = i as u32;
            assert_eq!(gap.stp_ptm1, 0x1000_0000 + i);
            assert_eq!(gap.stp_ptm2, 0x2000_0000 + i);
            assert_eq!(gap.gap_len1, 0x3000_0000 + i);
            assert_eq!(gap.gap_len2, 0x4000_0000 + i);
        }
    }

    #[test]
    fn dsi_pbi_ilvu_flag_decoders_isolate_bits() {
        // Each ILVU flag bit must decode independently — the helper
        // suite from `mpucoder-dsi_pkt.html` enumerates a0 / 80 / 90
        // bytes in the PREU window, so the bit-level decode matters.
        let bits = [
            (0x8000u16, SmlPbi::preu as fn(&SmlPbi) -> bool),
            (0x4000, SmlPbi::is_ilvu),
            (0x2000, SmlPbi::unit_start),
            (0x1000, SmlPbi::unit_end),
        ];
        for (bit, getter) in bits {
            let mut buf = vec![0u8; SmlPbi::SIZE];
            buf[0..2].copy_from_slice(&bit.to_be_bytes());
            let pbi = SmlPbi::parse(&buf).expect("SmlPbi parses");
            // The targeted bit is set, the other three are clear.
            assert!(getter(&pbi), "bit {:#06x} should be set", bit);
            for (other_bit, other_getter) in bits.iter().filter(|(b, _)| *b != bit) {
                assert!(
                    !other_getter(&pbi),
                    "bit {:#06x} leaked into bit {:#06x}",
                    bit,
                    other_bit
                );
            }
        }
    }

    #[test]
    fn dsi_parses_sml_agli_block() {
        let buf = build_dsi_body();
        let dsi = DsiPacket::parse(&buf).expect("DSI body parses");
        // 9 angle cells, 6 bytes each — index 8 is the spec's
        // sml_agl_c9_* row at packet offset 0xE4.
        for (i, cell) in dsi.sml_agli.cells.iter().enumerate() {
            let i = i as u32;
            assert_eq!(cell.dsta, 0x5000_0000 + i);
            assert_eq!(cell.sz, 0x6000 + i as u16);
        }
        // Spec-prescribed 9-cell stride: the 9th cell ends at the
        // SML_AGLI / VOBU_SRI boundary (packet 0xEA).
        assert_eq!(SmlAgli::PACKET_OFFSET + SmlAgli::SIZE, 0xEA);
    }

    #[test]
    fn dsi_parses_vobu_sri_block_and_brackets() {
        let buf = build_dsi_body();
        let dsi = DsiPacket::parse(&buf).expect("DSI body parses");
        let sri = dsi.vobu_sri;
        assert_eq!(sri.sri_nvwv, 0x8000_0001);
        for (i, slot) in sri.forward.iter().enumerate() {
            assert_eq!(*slot, 0x9000_0000 + i as u32);
        }
        assert_eq!(sri.sri_nv, 0x8000_0002);
        assert_eq!(sri.sri_pv, 0x8000_0003);
        for (i, slot) in sri.backward.iter().enumerate() {
            assert_eq!(*slot, 0xA000_0000 + i as u32);
        }
        assert_eq!(sri.sri_pvwv, 0x8000_0004);

        // Bit-31 valid flag + bit-30 intermediate flag + 30-bit offset
        // mask — these are the three patterns every consumer of the
        // table works with.
        assert_eq!(VobuSri::VALID_BIT, 0x8000_0000);
        assert_eq!(VobuSri::INTERMEDIATE_BIT, 0x4000_0000);
        assert_eq!(VobuSri::OFFSET_MASK, 0x3FFF_FFFF);
        assert!(sri.sri_nvwv & VobuSri::VALID_BIT != 0);
        assert_eq!(sri.sri_nvwv & VobuSri::OFFSET_MASK, 1);
    }

    #[test]
    fn dsi_parses_synci_block() {
        let buf = build_dsi_body();
        let dsi = DsiPacket::parse(&buf).expect("DSI body parses");
        let sy = dsi.synci;
        for (i, a) in sy.a_synca.iter().enumerate() {
            assert_eq!(*a, 0x7000 + i as u16);
        }
        for (i, sp) in sy.sp_synca.iter().enumerate() {
            assert_eq!(*sp, 0xB000_0000 + i as u32);
        }
        // Direction-bit constants land where the spec requires.
        assert_eq!(Synci::AUDIO_DIRECTION_BIT, 0x8000);
        assert_eq!(Synci::SP_DIRECTION_BIT, 0x8000_0000);
    }

    #[test]
    fn dsi_rejects_short_buffer() {
        // A buffer one byte short of the full DSI body must error
        // rather than read past the slice.
        let short = vec![0u8; DsiPacket::BODY_SIZE - 1];
        assert!(DsiPacket::parse(&short).is_err());
    }

    #[test]
    fn dsi_gi_cell_elapsed_time_decodes_bcd() {
        use crate::ifo::FrameRate;

        // Build a DSI body whose `c_eltm` carries a realistic BCD field:
        // 00:01:23.10 @ 30 fps.
        //   hh = 0x00, mm = 0x01, ss = 0x23 (= decimal 23),
        //   frame byte = 0b11_01_0000 = 0xD0 (rate=30 fps, BCD frames=10).
        let mut buf = build_dsi_body();
        let c_eltm_be = [0x00u8, 0x01, 0x23, 0xD0];
        buf[0x1C..0x20].copy_from_slice(&c_eltm_be);

        let dsi = DsiPacket::parse(&buf).expect("DSI body parses");

        // Typed accessor decodes the four bytes via PgcTime::from_bytes.
        let t = dsi.general_info.cell_elapsed_time();
        assert_eq!(t.hours, 0);
        assert_eq!(t.minutes, 1);
        assert_eq!(t.seconds, 23);
        assert_eq!(t.frames, 10);
        assert_eq!(t.frame_rate, FrameRate::Ntsc30);

        // Nanosecond shortcut matches `total_seconds + frames/30`.
        // 83 s × 1e9 + 10 × 1e9 / 30 = 83_000_000_000 + 333_333_333.
        assert_eq!(dsi.general_info.cell_elapsed_ns(), 83_333_333_333);

        // DsiPacket shortcut delegates to DsiGi.
        assert_eq!(dsi.cell_elapsed_time(), t);
        assert_eq!(dsi.cell_elapsed_ns(), 83_333_333_333);
    }

    #[test]
    fn dsi_gi_cell_elapsed_time_pal_round_trip() {
        use crate::ifo::FrameRate;

        // 01:00:00.20 @ 25 fps → 3600 s + 20/25 s = 3600.8 s.
        //   frame byte: rate=01, BCD frames hi=2 lo=0 → 0b01_10_0000 = 0x60.
        let mut buf = build_dsi_body();
        buf[0x1C..0x20].copy_from_slice(&[0x01u8, 0x00, 0x00, 0x60]);

        let dsi = DsiPacket::parse(&buf).expect("DSI body parses");
        let t = dsi.cell_elapsed_time();
        assert_eq!(t.frame_rate, FrameRate::Pal25);
        assert_eq!(t.frames, 20);
        assert_eq!(t.total_seconds(), 3600);
        assert_eq!(dsi.cell_elapsed_ns(), 3_600_800_000_000);
    }

    #[test]
    fn dsi_nav_pack_round_trip_through_full_sector() {
        // Glue check: a 2048-byte nav sector with the build_dsi_body
        // payload injected at sector offset 0x407 must round-trip
        // through NavPack::parse and surface every sub-section.
        let body = build_dsi_body();
        let mut sector = build_nav_sector(1, 0, 0);
        // build_nav_sector already wrote DSI_GI[0x04] (nv_pck_lbn) and
        // DSI_GI[0x08] (vobu_ea) — overwrite the whole DSI body with
        // the richer fixture.
        sector[0x407..0x407 + body.len()].copy_from_slice(&body);
        let nav = NavPack::parse(&sector).expect("nav pack parses");
        // Every sub-section comes through unchanged.
        assert_eq!(nav.dsi.general_info.nv_pck_lbn, 0x3333_4444);
        assert_eq!(nav.dsi.sml_pbi.ilvu, 0xF000);
        assert_eq!(nav.dsi.sml_agli.cells[0].dsta, 0x5000_0000);
        assert_eq!(nav.dsi.vobu_sri.sri_nvwv, 0x8000_0001);
        assert_eq!(nav.dsi.synci.a_synca[0], 0x7000);
    }
}
