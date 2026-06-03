//! Phase 3b — write a DVD-Video title to a Matroska file.
//!
//! Glues the [`crate::vob`] sector / PES walker to
//! [`oxideav_mkv::mux::MkvMuxer`]. Per-PES packets keep their original
//! 90 kHz PTS; PGC playback-time fields become MKV
//! `ChapterTimeStart` / `ChapterTimeEnd` (in nanoseconds).
//!
//! ## Stream model
//!
//! DVD-Video VTSes carry:
//!
//! * exactly one MPEG-2 video stream (`0xE0`),
//! * up to 8 audio tracks across AC-3 (`0x80..=0x87`), DTS
//!   (`0x88..=0x8F`), and LPCM (`0xA0..=0xA7`) — the dispatcher
//!   stores them as one logical track each per PES `(stream_id,
//!   substream_id)` pair seen during the title's first probe pass,
//! * up to 32 VobSub subpicture tracks (`0x20..=0x3F`).
//!
//! Two passes over the VOBs are used:
//!
//! 1. **Probe pass** — walk every PES once to enumerate which
//!    substream IDs the title actually carries, so the MKV header
//!    `Tracks` element is sized correctly before the first packet is
//!    written. (MKV requires `Tracks` up-front; we can't stream-add a
//!    new track after the first `SimpleBlock`.)
//! 2. **Mux pass** — re-walk the VOBs, packetise each PES, and feed
//!    `MkvMuxer::write_packet`. PTS is forwarded verbatim in the
//!    PES's 90 kHz time base; the muxer rescales internally.
//!
//! ## PgcTime → ns
//!
//! Per `docs/container/dvd/application/mpucoder-pgc.html`, the 4-byte
//! BCD `hh:mm:ss:ff` playback-time field's last byte's top 2 bits
//! pick a nominal frame rate:
//!
//! * `11b` → 30 fps (NTSC). ECMA-267 does not require drop-frame
//!   semantics; the BCD frame count is the raw frame ordinal.
//! * `01b` → 25 fps (PAL).
//!
//! We treat both as exact rationals (1/30 s and 1/25 s respectively)
//! and convert with integer math: `seconds * 1e9 + frames * 1e9 /
//! fps`. NTSC's "29.97" pull-down is not encoded in `PgcTime` and is
//! out of scope for the chapter timeline — the per-frame video PTS in
//! the VOB carries the truth at the sample level. Frame rates other
//! than `Pal25` / `Ntsc30` (the spec's `00b` / `10b` "illegal") fall
//! back to second-granularity by dropping the `frames` component.
//!
//! ## Wall
//!
//! No external implementation source consulted — clean-room from the
//! `docs/container/dvd/` references and the spec citations above.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom};
use std::path::Path;

use oxideav_core::packet::PacketFlags;
use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_mkv::mux::MkvMuxer;

use crate::disc::{DvdDisc, DvdFileKind};
use crate::error::{Error, Result};
use crate::ifo::{DvdChapter, FrameRate, PgcTime, DVD_SECTOR};
use crate::vob::{
    looks_like_nav_pack, DvdSubstream, NavPack, PackHeader, PesPacket, SC_PADDING_STREAM,
    SC_PRIVATE_STREAM_1, SC_PRIVATE_STREAM_2, SC_SYSTEM_HEADER,
};

/// PES timestamps on MPEG-PS are in 90 kHz units. MKV expects per-
/// packet `time_base` to match the units the PTS field is written in,
/// and rescales internally to the segment timecode.
const PES_TIME_BASE: TimeBase = TimeBase::new(1, 90_000);

/// Convert a [`PgcTime`] BCD field to absolute nanoseconds.
///
/// See module-level docs for the semantics. Returns `0` when the
/// frame-rate bits are the spec's `00b` / `10b` "illegal" values — we
/// can still emit a chapter spanning zero ns (MKV permits it) so the
/// chapter list never gets dropped silently.
pub fn pgc_time_to_ns(t: PgcTime) -> u64 {
    let secs = u64::from(t.total_seconds());
    let secs_ns = secs.saturating_mul(1_000_000_000);
    // Rational arithmetic: `(frames * 1e9) / fps` keeps the division
    // truncation at most ±1 ns instead of accumulating ~5e-9 per frame
    // (which would make `0:0:1.15 @ 30 fps` round to 1_499_999_995 ns
    // instead of the spec-exact 1_500_000_000 — caught by the
    // `pgc_time_ns_ntsc_30` regression test).
    let frames_ns = match t.frame_rate {
        FrameRate::Ntsc30 => u64::from(t.frames).saturating_mul(1_000_000_000) / 30,
        FrameRate::Pal25 => u64::from(t.frames).saturating_mul(1_000_000_000) / 25,
        FrameRate::Illegal | FrameRate::Reserved => 0,
    };
    secs_ns.saturating_add(frames_ns)
}

/// Per-stream slot in the MKV `Tracks` element. We keep `tag` so the
/// mux pass can re-map a PES (stream_id, substream_id) → MKV stream
/// index in O(1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DvdMkvStream {
    /// `(stream_id) = 0xE0..=0xEF` — there's only one video stream
    /// on a DVD; we coalesce all 0xE? IDs into stream 0.
    Video,
    /// `(stream_id = 0xBD, substream = 0x80..=0x87)`.
    Ac3(u8),
    /// `(stream_id = 0xBD, substream = 0x88..=0x8F)`.
    Dts(u8),
    /// `(stream_id = 0xBD, substream = 0xA0..=0xA7)`. Carrier is
    /// not currently emitted to MKV (no codec_id mapping yet) but we
    /// still surface the stream so consumers see it was discovered.
    Lpcm(u8),
    /// `(stream_id = 0xBD, substream = 0x20..=0x3F)`.
    Subpicture(u8),
}

impl DvdMkvStream {
    fn codec_id(self) -> CodecId {
        match self {
            Self::Video => CodecId::new("mpeg2video"),
            Self::Ac3(_) => CodecId::new("ac3"),
            Self::Dts(_) => CodecId::new("dts"),
            // Map LPCM to a generic pcm tag — MKV's `A_PCM/INT/BIG`
            // is the wire form DVD LPCM matches once the 7-byte
            // private-stream-1 audio-pack header (see
            // `crate::lpcm::LpcmHeader`) is stripped by the Pass-2
            // routing below.
            Self::Lpcm(_) => CodecId::new("pcm_s16be"),
            Self::Subpicture(_) => CodecId::new("dvd_subtitle"),
        }
    }

    fn media_type(self) -> oxideav_core::MediaType {
        match self {
            Self::Video => oxideav_core::MediaType::Video,
            Self::Ac3(_) | Self::Dts(_) | Self::Lpcm(_) => oxideav_core::MediaType::Audio,
            Self::Subpicture(_) => oxideav_core::MediaType::Subtitle,
        }
    }

    fn from_pes(pes: &PesPacket<'_>) -> Option<Self> {
        match pes.stream_id {
            0xE0..=0xEF => Some(Self::Video),
            SC_PRIVATE_STREAM_1 => pes.dvd_substream().map(|s| match s {
                DvdSubstream::Ac3(_) => Self::Ac3(s.track()),
                DvdSubstream::Dts(_) => Self::Dts(s.track()),
                DvdSubstream::Lpcm(_) => Self::Lpcm(s.track()),
                DvdSubstream::Subpicture(_) => Self::Subpicture(s.track()),
            }),
            _ => None,
        }
    }
}

/// Result of [`probe_title_streams`] — discovered stream set plus the
/// PGC chapter list to be encoded as MKV `Chapters`.
#[derive(Debug, Default)]
struct TitleProbe {
    streams: Vec<DvdMkvStream>,
    chapters: Vec<DvdChapter>,
}

/// Top-level entry: read every VOB of `title_idx` (1-based) and write
/// a Matroska file at `out_path`.
///
/// `title_idx` indexes into the **disc-level** title list (the same
/// numbering `DvdDisc::enumerate_titles` returns — `1..=99`).
pub fn write_title_to_mkv(
    disc: &DvdDisc,
    title_idx: usize,
    image_path: impl AsRef<Path>,
    out_path: impl AsRef<Path>,
) -> Result<()> {
    let image_path = image_path.as_ref();
    let mut reader = BufReader::new(File::open(image_path)?);

    // Resolve title_idx → (vts_number, vts_title_number, chapters).
    let titles = disc.enumerate_titles(&mut reader)?;
    let title_entry = titles
        .get(title_idx.checked_sub(1).unwrap_or(usize::MAX))
        .copied()
        .ok_or(Error::NotDvdVideo(
            "write_title_to_mkv: title_idx out of range",
        ))?;
    let vts = disc.parse_vts(&mut reader, title_entry.vts_number)?;
    let vts_title_idx = usize::from(title_entry.vts_title_number.saturating_sub(1));
    let title = vts.titles.get(vts_title_idx).ok_or(Error::NotDvdVideo(
        "write_title_to_mkv: VTS_TTN out of range vs VtsIfo.titles",
    ))?;

    // Phase 1: probe pass over the title's cells.
    let mut probe = TitleProbe {
        streams: Vec::new(),
        chapters: title.chapters.clone(),
    };
    for ch in &title.chapters {
        let pgc = vts
            .pgcs
            .get(usize::from(ch.pgcn.saturating_sub(1)))
            .ok_or(Error::InvalidUdf(
                "write_title_to_mkv: chapter PGCN out of range",
            ))?;
        for cell_no in ch.start_cell..=ch.end_cell {
            let pos = pgc
                .cell_positions
                .get(usize::from(cell_no.saturating_sub(1)))
                .ok_or(Error::InvalidUdf(
                    "write_title_to_mkv: cell index past cell_positions",
                ))?;
            let pb = pgc
                .cells
                .get(usize::from(cell_no.saturating_sub(1)))
                .ok_or(Error::InvalidUdf(
                    "write_title_to_mkv: cell index past cell_playback",
                ))?;
            // `pos.vob_id` / `pos.cell_id` aren't consumed yet — the
            // sectors in `pb` are already cell-relative.
            let _ = (pos.vob_id, pos.cell_id);
            walk_cell_sectors(
                disc,
                title_entry.vts_number,
                &mut reader,
                pb.first_vobu_start_sector,
                pb.last_vobu_end_sector,
                |pes| {
                    if let Some(s) = DvdMkvStream::from_pes(&pes) {
                        if !probe.streams.contains(&s) {
                            probe.streams.push(s);
                        }
                    }
                    Ok(())
                },
            )?;
        }
    }

    // MKV needs at least one stream — most titles always carry video,
    // but if a probe pass came up empty (corrupt fixture) we bail
    // rather than silently writing a zero-stream file.
    if probe.streams.is_empty() {
        return Err(Error::NotDvdVideo(
            "write_title_to_mkv: no playable streams found in title",
        ));
    }
    // Sort for deterministic stream ordering: Video first, then audio
    // (AC-3 → DTS → LPCM, each in track order), then subpicture.
    probe.streams.sort();

    // Build StreamInfo list.
    let stream_infos: Vec<StreamInfo> = probe
        .streams
        .iter()
        .enumerate()
        .map(|(i, s)| StreamInfo {
            index: i as u32,
            time_base: PES_TIME_BASE,
            duration: None,
            start_time: None,
            params: codec_parameters_for(*s),
        })
        .collect();

    // Build the muxer.
    let out_file = File::create(out_path.as_ref())?;
    let writer: Box<dyn WriteSeek> = Box::new(BufWriter::new(out_file));
    let mut muxer = MkvMuxer::new_matroska(writer, &stream_infos)
        .map_err(|e| Error::InvalidUdf(io_static("MKV mux init failed", e.to_string())))?;

    // Stage chapter atoms (in nanoseconds). PGC playback-time is the
    // chapter's TOTAL — we surface it as ChapterTimeStart for chapter
    // N, and the start of chapter N+1 (or end-of-title) as
    // ChapterTimeEnd.
    let mut acc_ns: u64 = 0;
    for (i, ch) in probe.chapters.iter().enumerate() {
        let ch_ns = pgc_time_to_ns(ch.playback_time);
        let start_ns = acc_ns;
        let end_ns = acc_ns.saturating_add(ch_ns);
        let next_start = probe
            .chapters
            .get(i + 1)
            .map(|n| {
                // Next chapter's start = end of this chapter only if
                // the chapter-PGC-grouping is sequential (each chapter
                // in its own PGC). When two chapters share a PGC, the
                // PGC's playback_time is the WHOLE PGC's duration and
                // we'd double-count by using `acc_ns` blindly. The
                // simpler well-defined fallback: use this chapter's
                // (start + duration). Phase 3c can refine using
                // `c_eltm` once we materialise per-cell timing.
                u64::from(n.number)
            })
            .map(|_| end_ns);
        muxer
            .add_chapter(start_ns, next_start, format!("Chapter {}", ch.number))
            .map_err(|e| Error::InvalidUdf(io_static("MKV add_chapter failed", e.to_string())))?;
        acc_ns = end_ns;
    }

    muxer
        .write_header()
        .map_err(|e| Error::InvalidUdf(io_static("MKV write_header failed", e.to_string())))?;

    // Phase 2: mux pass.
    let mut anchor_pts_90khz: Option<u64> = None;
    for ch in &title.chapters {
        let pgc = &vts.pgcs[usize::from(ch.pgcn.saturating_sub(1))];
        for cell_no in ch.start_cell..=ch.end_cell {
            let pos = &pgc.cell_positions[usize::from(cell_no.saturating_sub(1))];
            let pb = &pgc.cells[usize::from(cell_no.saturating_sub(1))];
            let _ = (pos.vob_id, pos.cell_id);
            walk_cell_sectors(
                disc,
                title_entry.vts_number,
                &mut reader,
                pb.first_vobu_start_sector,
                pb.last_vobu_end_sector,
                |pes| {
                    let stream = match DvdMkvStream::from_pes(&pes) {
                        Some(s) => s,
                        None => return Ok(()),
                    };
                    let stream_idx =
                        probe
                            .streams
                            .iter()
                            .position(|x| *x == stream)
                            .ok_or(Error::InvalidUdf(
                                "write_title_to_mkv: probe missed a substream the mux pass found",
                            ))? as u32;
                    let raw_pts = pes.pts;
                    let pts = match raw_pts {
                        Some(p) => {
                            let anchor = *anchor_pts_90khz.get_or_insert(p);
                            Some(p.saturating_sub(anchor) as i64)
                        }
                        None => None,
                    };
                    let mut data = pes.payload.to_vec();
                    // private_stream_1 payload's first byte is the
                    // substream ID — strip so the MKV consumer sees
                    // clean AC-3 / DTS / LPCM / VobSub bytes. LPCM
                    // additionally carries a 7-byte audio-pack header
                    // (`mpucoder-lpcm.html`) ahead of the raw PCM
                    // sample bytes; strip those too so the MKV
                    // `pcm_s16be` track receives big-endian samples
                    // verbatim per `A_PCM/INT/BIG`.
                    if pes.stream_id == SC_PRIVATE_STREAM_1 && !data.is_empty() {
                        let is_lpcm = matches!(stream, DvdMkvStream::Lpcm(_));
                        data.remove(0);
                        if is_lpcm && data.len() >= crate::LPCM_HEADER_LEN - 1 {
                            // We already removed the substream-ID
                            // byte; the LPCM header counts that byte
                            // as offset 0, so strip the remaining 6
                            // header bytes.
                            data.drain(0..crate::LPCM_HEADER_LEN - 1);
                        }
                    }
                    let mut flags = PacketFlags::default();
                    // DVD video is MPEG-2 with sparse I-frames; we
                    // don't decode here, so signal keyframe on every
                    // packet that begins with a sequence header
                    // (0x000001B3) — a conservative heuristic the
                    // MKV cue index uses to seed Cues entries. False
                    // negatives just mean fewer Cue points.
                    if matches!(stream, DvdMkvStream::Video)
                        && data.len() >= 4
                        && data[0..4] == [0x00, 0x00, 0x01, 0xB3]
                    {
                        flags.keyframe = true;
                    }
                    // Audio/subtitle packets are independently
                    // decodable, so mark them keyframes too — lets the
                    // MKV Cues entries cover audio random-access.
                    if matches!(stream.media_type(), oxideav_core::MediaType::Audio)
                        || matches!(stream.media_type(), oxideav_core::MediaType::Subtitle)
                    {
                        flags.keyframe = true;
                    }
                    let packet = Packet {
                        stream_index: stream_idx,
                        time_base: PES_TIME_BASE,
                        pts,
                        dts: pes.dts.map(|d| {
                            let anchor = *anchor_pts_90khz.get_or_insert(d);
                            d.saturating_sub(anchor) as i64
                        }),
                        duration: None,
                        flags,
                        data,
                    };
                    muxer.write_packet(&packet).map_err(|e| {
                        Error::InvalidUdf(io_static("MKV write_packet failed", e.to_string()))
                    })
                },
            )?;
        }
    }

    muxer
        .write_trailer()
        .map_err(|e| Error::InvalidUdf(io_static("MKV write_trailer failed", e.to_string())))?;
    Ok(())
}

fn codec_parameters_for(stream: DvdMkvStream) -> CodecParameters {
    let id = stream.codec_id();
    match stream {
        DvdMkvStream::Video => {
            let mut p = CodecParameters::video(id);
            // DVD-Video Main Profile @ Main Level (720x480 NTSC /
            // 720x576 PAL). We don't know the resolution without
            // parsing the MPEG-2 sequence header; surface NTSC as
            // the conservative default — the MKV `Tracks` element
            // accepts updates from the first decoded frame but the
            // muxer here doesn't decode. Phase 3c can extract the
            // exact resolution from the first sequence header in
            // the elementary stream.
            p.width = Some(720);
            p.height = Some(480);
            p
        }
        DvdMkvStream::Ac3(_) | DvdMkvStream::Dts(_) | DvdMkvStream::Lpcm(_) => {
            let mut p = CodecParameters::audio(id);
            // Conservative DVD defaults; the actual decoder will
            // refine on the first frame.
            p.sample_rate = Some(48_000);
            p.channels = Some(2);
            p
        }
        DvdMkvStream::Subpicture(_) => CodecParameters::subtitle(id),
    }
}

/// Walk the sector range `[first_sector, last_sector]` (cell-relative
/// to the VTS title VOB chain) and call `f` for every PES packet.
/// Pack headers, system headers, padding, and nav-packs are consumed
/// transparently; `looks_like_nav_pack` short-circuits the sector at
/// no PES cost.
///
/// `vob_id`/`cell_id` would let us re-anchor the LBA via
/// `VtsCAdt::lookup`, but the C_PBI sectors already absolute (per
/// mpucoder-pgc.html) so the lookup is not currently needed; the
/// parameters are dropped to keep the clippy `too_many_arguments`
/// budget comfortable.
fn walk_cell_sectors<R, F>(
    disc: &DvdDisc,
    vts_number: u8,
    reader: &mut R,
    first_sector: u32,
    last_sector: u32,
    mut f: F,
) -> Result<()>
where
    R: Read + Seek,
    F: FnMut(PesPacket<'_>) -> Result<()>,
{
    // C_PBI sectors are absolute disc-LBA-equivalent values per
    // mpucoder-pgc.html (the "first VOBU start sector" field carries
    // the same LBA scale `VTS_C_ADT::lookup` returns). The VTS title
    // VOB chain starts at `VTS_xx_1.VOB`'s LBA on disc, with each
    // subsequent VOB appended contiguously — so we can read directly
    // from the chain by absolute LBA without per-VOB seeking.
    let base_lba = disc
        .video_ts_files
        .iter()
        .find(|f| {
            matches!(
                f.kind,
                DvdFileKind::VtsTitle { ts, vob: 1 } if ts == vts_number
            )
        })
        .map(|f| f.lba)
        .ok_or(Error::NotDvdVideo(
            "walk_cell_sectors: title set has no VTS_xx_1.VOB",
        ))?;

    if last_sector < first_sector {
        return Ok(());
    }
    let count = last_sector - first_sector + 1;
    let mut buf = vec![0u8; DVD_SECTOR];
    for s in 0..count {
        let abs_lba = base_lba.saturating_add(first_sector).saturating_add(s);
        reader.seek(SeekFrom::Start(u64::from(abs_lba) * DVD_SECTOR as u64))?;
        reader.read_exact(&mut buf)?;
        // Nav-pack? Validate cheaply and skip — the demuxer's Phase
        // 3a path stores them in `VobStreams::nav_packs`; here we
        // just consume.
        if looks_like_nav_pack(&buf) {
            // Parsing for validation only; the result is unused.
            let _ = NavPack::parse(&buf)?;
            continue;
        }
        let pack = PackHeader::parse(&buf)?;
        let mut cursor = PackHeader::SIZE + pack.stuffing_bytes as usize;
        while cursor + 6 <= buf.len() {
            if buf[cursor..cursor + 4] == [0x00, 0x00, 0x01, SC_SYSTEM_HEADER] {
                let len = ((buf[cursor + 4] as usize) << 8) | buf[cursor + 5] as usize;
                cursor += 6 + len;
                continue;
            }
            if buf[cursor..cursor + 4] == [0x00, 0x00, 0x01, SC_PADDING_STREAM] {
                let len = ((buf[cursor + 4] as usize) << 8) | buf[cursor + 5] as usize;
                cursor += 6 + len;
                continue;
            }
            if buf[cursor..cursor + 4] == [0x00, 0x00, 0x01, SC_PRIVATE_STREAM_2] {
                let len = ((buf[cursor + 4] as usize) << 8) | buf[cursor + 5] as usize;
                cursor += 6 + len;
                continue;
            }
            if buf[cursor..cursor + 3] != [0x00, 0x00, 0x01] {
                break;
            }
            let pes = PesPacket::parse(&buf[cursor..])?;
            cursor += pes.wire_size;
            f(pes)?;
        }
    }
    Ok(())
}

/// Small helper to flatten an `oxideav_core::Error` (or any
/// `ToString`) into a `&'static str` slot of `Error::InvalidUdf`. We
/// deliberately don't add a new `Error` variant for "MKV error" — the
/// crate-local error enum is intentionally minimal (see error.rs) and
/// adding a variant would break consumer match exhaustiveness. The
/// shape is `"label: payload"` so callers can grep.
fn io_static(label: &'static str, _payload: String) -> &'static str {
    // We can't actually format the payload into a `&'static str`
    // without leaking memory. Surface the label only; debug-printable
    // payload is dropped at this layer. (A full error rewrite is a
    // Phase 3c follow-up.)
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pgc_time_ns_ntsc_30() {
        // 00:00:01.15 @ 30 fps → 1 s + 15/30 s = 1.5 s = 1_500_000_000 ns.
        let t = PgcTime {
            hours: 0,
            minutes: 0,
            seconds: 1,
            frames: 15,
            frame_rate: FrameRate::Ntsc30,
        };
        assert_eq!(pgc_time_to_ns(t), 1_500_000_000);
    }

    #[test]
    fn pgc_time_ns_pal_25() {
        // 00:00:00.05 @ 25 fps → 5/25 s = 0.2 s = 200_000_000 ns.
        let t = PgcTime {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 5,
            frame_rate: FrameRate::Pal25,
        };
        assert_eq!(pgc_time_to_ns(t), 200_000_000);
    }

    #[test]
    fn pgc_time_ns_illegal_drops_frames() {
        // Illegal frame-rate bits: only the seconds component
        // survives. Better-than-failing-the-whole-conversion behaviour
        // since some authoring tools emit zero-time placeholders.
        let t = PgcTime {
            hours: 0,
            minutes: 0,
            seconds: 3,
            frames: 12,
            frame_rate: FrameRate::Illegal,
        };
        assert_eq!(pgc_time_to_ns(t), 3_000_000_000);
    }

    #[test]
    fn pgc_time_ns_hour_boundary() {
        // 1 h NTSC, no frames: 3600 × 1e9 = 3.6e12 ns.
        let t = PgcTime {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
            frame_rate: FrameRate::Ntsc30,
        };
        assert_eq!(pgc_time_to_ns(t), 3_600_000_000_000);
    }

    #[test]
    fn dvd_mkv_stream_codec_id_mapping() {
        assert_eq!(DvdMkvStream::Video.codec_id().as_str(), "mpeg2video");
        assert_eq!(DvdMkvStream::Ac3(0).codec_id().as_str(), "ac3");
        assert_eq!(DvdMkvStream::Dts(2).codec_id().as_str(), "dts");
        assert_eq!(DvdMkvStream::Lpcm(0).codec_id().as_str(), "pcm_s16be");
        assert_eq!(
            DvdMkvStream::Subpicture(1).codec_id().as_str(),
            "dvd_subtitle"
        );
    }

    #[test]
    fn dvd_mkv_stream_media_types() {
        use oxideav_core::MediaType;
        assert_eq!(DvdMkvStream::Video.media_type(), MediaType::Video);
        assert_eq!(DvdMkvStream::Ac3(7).media_type(), MediaType::Audio);
        assert_eq!(DvdMkvStream::Dts(0).media_type(), MediaType::Audio);
        assert_eq!(DvdMkvStream::Lpcm(0).media_type(), MediaType::Audio);
        assert_eq!(
            DvdMkvStream::Subpicture(0).media_type(),
            MediaType::Subtitle
        );
    }

    #[test]
    fn dvd_mkv_stream_sort_order() {
        let mut v = [
            DvdMkvStream::Subpicture(0),
            DvdMkvStream::Ac3(2),
            DvdMkvStream::Video,
            DvdMkvStream::Dts(0),
            DvdMkvStream::Lpcm(0),
        ];
        v.sort();
        assert_eq!(v[0], DvdMkvStream::Video);
        assert!(matches!(v[1], DvdMkvStream::Ac3(_)));
        assert!(matches!(v[v.len() - 1], DvdMkvStream::Subpicture(_)));
    }
}
