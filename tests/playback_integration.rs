//! End-to-end synthetic-disc playback integration — navigation engine
//! × VOB demux × LPCM frame unpack × SPU compositing × trick play ×
//! still-time semantics, all through the crate's public API.
//!
//! A two-cell title lives in a six-sector in-memory VOB:
//!
//! | LBN | Content                                             |
//! |-----|-----------------------------------------------------|
//! | 0   | nav pack — VOBU 1 of cell 1 (SRI: next video at +2) |
//! | 1   | video PES `CELL1-VIDEO-A`                           |
//! | 2   | nav pack — VOBU 2 of cell 1 (SRI: no more video)    |
//! | 3   | video PES `CELL1-VIDEO-B` + LPCM PES + SPU PES      |
//! | 4   | nav pack — VOBU 1 of cell 2                         |
//! | 5   | video PES `CELL2-VIDEO`                             |
//!
//! The PGC authors cell 1 with no still, cell 2 with a 2-second
//! still, an infinite PGC still, and a prohibited-UOP word banning
//! "Still off" — so the walk exercises `PgcRunner` events, the
//! stream-selection helpers, per-cell demux, LPCM bytes → PCM
//! frames, SPU palette compositing, SRI-based scanning, and the
//! `StillClock` UOP gate in one pass.
//!
//! Clean-room per the `docs/container/dvd/application/` references
//! cited on each helper in `src/` (mpucoder-pgc / -dsi_pkt / -lpcm /
//! -spu / -sprm / -uops + stnsoft-ass-hdr / -sys_hdr / -vobov).

use std::io::Cursor;

use oxideav_dvd::{
    peel_lpcm_payload, scan_permitted, scan_step, select_audio_stream, select_subpicture_stream,
    AudioSelection, AudioStreamControl, CellPlaybackInfo, PaletteEntry, Pgc, PgcRunner, PgcTime,
    PlaybackEvent, ScanDirection, SriPointer, StillPhase, StillTime, SubPictureUnit,
    SubpictureSelection, SubpictureStreamControl, TrickStep, UopMask, UserOp, Vm, VobDemuxer,
    VobuSri, DVD_SECTOR, SPRM_AUDIO_STREAM, SPRM_SUBPICTURE_STREAM,
};

// ---------------------------------------------------------------------
// Sector builders (MPEG-PS pack / PES / nav-pack wire images).
// ---------------------------------------------------------------------

/// 14-byte MPEG-PS pack header with zero SCR and the DVD-typical
/// program mux rate, no stuffing.
fn pack_header() -> [u8; 14] {
    let mut b = [0u8; 14];
    b[..4].copy_from_slice(&[0x00, 0x00, 0x01, 0xBA]);
    // SCR zero: marker bits only ('01' prefix + three marker 1s).
    b[4] = 0b0100_0100;
    b[6] = 0b0000_0100;
    b[8] = 0b0000_0100;
    b[9] = 0b0000_0001;
    // mux_rate 25200 + trailing '11'.
    let mr: u32 = 25_200;
    b[10] = (mr >> 14) as u8;
    b[11] = (mr >> 6) as u8;
    b[12] = (((mr & 0x3F) as u8) << 2) | 0b11;
    b[13] = 0xF8; // reserved + 0 stuffing bytes
    b
}

/// Spec-shaped DVD system header (rate/audio/video bounds + the four
/// stream_bound entries per stnsoft-sys_hdr.html).
fn system_header() -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x01, 0xBB, 0x00, 0x12];
    let rate_bound: u32 = 25_200;
    v.push(0x80 | ((rate_bound >> 15) as u8 & 0x7F));
    v.push((rate_bound >> 7) as u8);
    v.push((((rate_bound & 0x7F) as u8) << 1) | 1);
    v.push(1 << 2); // audio_bound = 1
    v.push(0b1110_0000 | 1); // locks + video_bound = 1
    v.push(0x7F);
    for &(sid, scale, size) in &[
        (0xB9u8, true, 232u16),
        (0xB8u8, false, 32u16),
        (0xBDu8, true, 58u16),
        (0xBFu8, true, 2u16),
    ] {
        v.push(sid);
        v.push(0b1100_0000 | ((scale as u8) << 5) | ((size >> 8) as u8 & 0x1F));
        v.push((size & 0xFF) as u8);
    }
    v
}

/// Video PES packet (stream id 0xE0), no PTS.
fn pes_video(payload: &[u8]) -> Vec<u8> {
    let pes_len = 3 + payload.len();
    let mut v = vec![0x00, 0x00, 0x01, 0xE0];
    v.push((pes_len >> 8) as u8);
    v.push((pes_len & 0xFF) as u8);
    v.extend_from_slice(&[0b1000_0000, 0, 0]);
    v.extend_from_slice(payload);
    v
}

/// private_stream_1 PES packet carrying `substream` + `payload`.
fn pes_private1(substream: u8, payload: &[u8]) -> Vec<u8> {
    let pes_len = 3 + 1 + payload.len();
    let mut v = vec![0x00, 0x00, 0x01, 0xBD];
    v.push((pes_len >> 8) as u8);
    v.push((pes_len & 0xFF) as u8);
    v.extend_from_slice(&[0b1000_0000, 0, 0]);
    v.push(substream);
    v.extend_from_slice(payload);
    v
}

/// A content sector: pack header + the given PES packets, zero-padded
/// to 2048 bytes.
fn content_sector(pes_packets: &[Vec<u8>]) -> Vec<u8> {
    let mut s = Vec::with_capacity(DVD_SECTOR);
    s.extend_from_slice(&pack_header());
    for p in pes_packets {
        s.extend_from_slice(p);
    }
    assert!(s.len() <= DVD_SECTOR, "sector overflow");
    s.resize(DVD_SECTOR, 0);
    s
}

/// A nav-pack sector: pack header + system header + PCI (0xBF/0x00) +
/// DSI (0xBF/0x01) with the given LBN and VOBU_SRI entry patches of
/// `(sri_relative_offset, raw_word)` shape, everything else zero.
fn nav_sector(lbn: u32, first_ref_ea: u32, sri: &[(usize, u32)]) -> Vec<u8> {
    let mut s = vec![0u8; DVD_SECTOR];
    s[..14].copy_from_slice(&pack_header());
    let sys = system_header();
    s[0x0E..0x0E + sys.len()].copy_from_slice(&sys);
    // PCI: start code + length 0x3D4 + substream 0x00; body at 0x2D.
    s[0x26..0x2D].copy_from_slice(&[0x00, 0x00, 0x01, 0xBF, 0x03, 0xD4, 0x00]);
    s[0x2D..0x31].copy_from_slice(&lbn.to_be_bytes());
    // DSI: start code + length 0x3FA + substream 0x01; body at 0x407.
    s[0x400..0x407].copy_from_slice(&[0x00, 0x00, 0x01, 0xBF, 0x03, 0xFA, 0x01]);
    s[0x40B..0x40F].copy_from_slice(&lbn.to_be_bytes()); // nv_pck_lbn
    s[0x413..0x417].copy_from_slice(&first_ref_ea.to_be_bytes()); // vobu_1stref_ea
    for &(off, word) in sri {
        let at = 0x407 + VobuSri::PACKET_OFFSET + off;
        s[at..at + 4].copy_from_slice(&word.to_be_bytes());
    }
    s
}

/// Minimal SPU: a 2×2 solid rectangle of pixel-code 0 mapped to
/// palette index 5 at full contrast (SET_COLOR + SET_CONTR +
/// SET_DAREA + SET_DSPXA + STA_DSP per mpucoder-spu.html).
fn solid_spu() -> Vec<u8> {
    let mut buf = vec![0u8; 0x30];
    buf[0..2].copy_from_slice(&0x0030u16.to_be_bytes());
    buf[2..4].copy_from_slice(&0x0010u16.to_be_bytes());
    // PXDtf at 0x04 / PXDbf at 0x06: one 16-bit EOL run each.
    // DCSQ at 0x10.
    let d = 0x10;
    buf[d + 2..d + 4].copy_from_slice(&(d as u16).to_be_bytes());
    let mut o = d + 4;
    buf[o..o + 3].copy_from_slice(&[0x03, 0x00, 0x05]); // SET_COLOR: code0 → idx 5
    o += 3;
    buf[o..o + 3].copy_from_slice(&[0x04, 0x00, 0x0F]); // SET_CONTR: code0 opaque
    o += 3;
    // SET_DAREA: x 0..=1, y 0..=1.
    buf[o..o + 7].copy_from_slice(&[0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01]);
    o += 7;
    // SET_DSPXA: top 0x0004, bottom 0x0006.
    buf[o..o + 5].copy_from_slice(&[0x06, 0x00, 0x04, 0x00, 0x06]);
    o += 5;
    buf[o] = 0x01; // STA_DSP
    buf[o + 1] = 0xFF; // CMD_END
    buf
}

// ---------------------------------------------------------------------
// The synthetic title.
// ---------------------------------------------------------------------

/// LPCM audio-pack payload: 7-byte header (track 0, one frame
/// starting at offset 7, 16-bit / 48 kHz / stereo, neutral dynamic
/// range) + one 320-byte audio frame of ramp samples.
fn lpcm_payload() -> Vec<u8> {
    let mut p = vec![0xA0, 0x01, 0x00, 0x04, 0x00, 0x01, 0x80];
    for i in 0..160i16 {
        p.extend_from_slice(&i.to_be_bytes());
    }
    p
}

fn cell(first: u32, last: u32, still: u8) -> CellPlaybackInfo {
    CellPlaybackInfo {
        category_byte0: 0,
        restricted: false,
        still_time: still,
        cell_command: 0,
        playback_time: PgcTime::from_bytes([0, 0, 0, 0]),
        first_vobu_start_sector: first,
        first_ilvu_end_sector: 0,
        last_vobu_start_sector: last,
        last_vobu_end_sector: last,
    }
}

/// Build the two-cell PGC: audio logical 0 → physical 0, sub-picture
/// logical 0 → physical 0 in every display column, palette index 5 =
/// studio-swing white, PGC still infinite, "Still off" prohibited.
fn build_pgc() -> Pgc {
    let mut pgc = Pgc::parse(&[0u8; 0xEC]).expect("empty PGC parses");
    pgc.number_of_programs = 2;
    pgc.number_of_cells = 2;
    pgc.program_map = vec![1, 2];
    pgc.cells = vec![cell(0, 3, 0), cell(4, 5, 2)];
    pgc.still_time = 255;
    pgc.prohibited_user_ops = UopMask::NONE.with(UserOp::StillOff).raw();
    pgc.audio_stream_control[0] = AudioStreamControl {
        available: true,
        stream_number: 0,
    };
    pgc.subpicture_stream_control[0] = SubpictureStreamControl {
        available: true,
        stream_4x3: 0,
        stream_wide: 0,
        stream_letterbox: 0,
        stream_pan_scan: 0,
    };
    pgc.palette[5] = PaletteEntry {
        y: 235,
        cr: 128,
        cb: 128,
    };
    pgc
}

/// Assemble the six-sector VOB image.
fn build_vob() -> Vec<u8> {
    let mut vob = Vec::with_capacity(6 * DVD_SECTOR);
    // LBN 0: cell-1 VOBU 1 — next video VOBU at +2, 1 ref frame.
    vob.extend_from_slice(&nav_sector(0, 1, &[(0x00, VobuSri::VALID_BIT | 2)]));
    // LBN 1: cell-1 video A.
    vob.extend_from_slice(&content_sector(&[pes_video(b"CELL1-VIDEO-A")]));
    // LBN 2: cell-1 VOBU 2 — no more video forward, previous at -2.
    vob.extend_from_slice(&nav_sector(
        2,
        1,
        &[
            (0x00, SriPointer::NO_VIDEO_VOBU),
            (0xA4, VobuSri::VALID_BIT | 2),
        ],
    ));
    // LBN 3: cell-1 video B + LPCM + SPU.
    vob.extend_from_slice(&content_sector(&[
        pes_video(b"CELL1-VIDEO-B"),
        pes_private1(0xA0, &lpcm_payload()[1..]),
        pes_private1(0x20, &solid_spu()),
    ]));
    // LBN 4: cell-2 VOBU 1.
    vob.extend_from_slice(&nav_sector(4, 1, &[]));
    // LBN 5: cell-2 video.
    vob.extend_from_slice(&content_sector(&[pes_video(b"CELL2-VIDEO")]));
    vob
}

// ---------------------------------------------------------------------
// The end-to-end walk.
// ---------------------------------------------------------------------

#[test]
fn synthetic_title_plays_end_to_end() {
    let pgc = build_pgc();
    let vob = build_vob();
    let mut vm = Vm::new();

    // -- Stream selection: explicit SPRM 1 / SPRM 2 picks. ----------
    vm.regs.set_sprm(SPRM_AUDIO_STREAM, 0);
    vm.regs.set_sprm(SPRM_SUBPICTURE_STREAM, 1 << 6); // stream 0, display on
    let audio = select_audio_stream(&vm, &pgc.audio_stream_control, &[]);
    assert_eq!(
        audio,
        AudioSelection::Selected {
            logical: 0,
            physical: 0,
            via_preference: false,
        }
    );
    let subp = select_subpicture_stream(&vm, &pgc.subpicture_stream_control, &[]);
    let SubpictureSelection::Selected {
        physical: sp_physical,
        display: true,
        ..
    } = subp
    else {
        panic!("expected a displayed sub-picture selection, got {subp:?}");
    };

    // -- PgcRunner walk: two cells, cell-2 still, PGC still. ---------
    let mut runner = PgcRunner::new(&pgc, 1);
    let ev1 = runner.next_event(&mut vm);
    let PlaybackEvent::PlayCell {
        cell: 1,
        first_sector: c1_first,
        last_sector: c1_last,
        still: StillTime::None,
        ..
    } = ev1
    else {
        panic!("expected cell 1, got {ev1:?}");
    };
    let ev2 = runner.next_event(&mut vm);
    let PlaybackEvent::PlayCell {
        cell: 2,
        first_sector: c2_first,
        last_sector: c2_last,
        still: StillTime::Seconds(2),
        ..
    } = ev2
    else {
        panic!("expected cell 2 with a 2 s still, got {ev2:?}");
    };

    // -- Demux cell 1 and check every elementary stream. ------------
    let mut cursor = Cursor::new(&vob);
    let mut demux = VobDemuxer::new();
    demux
        .demux_range(&mut cursor, c1_first, c1_last - c1_first + 1)
        .expect("cell 1 demuxes");
    let streams = demux.take();
    assert_eq!(streams.video, b"CELL1-VIDEO-ACELL1-VIDEO-B");
    assert_eq!(streams.nav_packs.len(), 2);

    // LPCM: the audio selection routed physical substream 0 → the
    // demuxer's track-0 buffer. The demuxer strips the substream
    // selector; restore it and run bytes → PCM frames → samples.
    let AudioSelection::Selected { physical, .. } = audio else {
        unreachable!("asserted above");
    };
    let raw = &streams.lpcm[&physical];
    let mut payload = vec![0xA0];
    payload.extend_from_slice(raw);
    let (header, pcm) = peel_lpcm_payload(&payload).expect("LPCM header parses");
    assert_eq!(header.bits_per_sample(), Some(16));
    assert_eq!(header.sample_rate_hz(), Some(48_000));
    assert_eq!(header.channel_count, 2);
    assert_eq!(header.access_unit_offset(), Some(7));
    assert_eq!(header.audio_frame_size_bytes(), Some(320));
    let mut frames = header.split_frames(pcm).expect("frame split");
    let frame = frames.next().expect("one whole audio frame");
    assert_eq!(frame.len(), 320);
    assert!(frames.next().is_none());
    assert!(frames.partial_tail().is_empty());
    let samples = header.unpack_samples_16bit(frame).expect("16-bit unpack");
    assert_eq!(samples.len(), 160);
    assert_eq!(samples[0], 0);
    assert_eq!(samples[159], 159);

    // SPU: parse + composite through the PGC palette.
    let spu_bytes = &streams.subpicture[&sp_physical];
    let spu = SubPictureUnit::parse(spu_bytes).expect("SPU parses");
    let bmp = spu
        .composite(spu_bytes, &pgc.palette)
        .expect("composite ok")
        .expect("bitmap present");
    assert_eq!((bmp.x, bmp.y, bmp.width, bmp.height), (0, 0, 2, 2));
    for px in bmp.rgba.chunks_exact(4) {
        assert!(px[0] >= 254 && px[1] >= 254 && px[2] >= 254, "white pixel");
        assert_eq!(px[3], 0xFF, "opaque pixel");
    }

    // -- Trick play across cell 1's VOBUs. ---------------------------
    let uops = UopMask::from_bits(pgc.prohibited_user_ops);
    assert!(scan_permitted(ScanDirection::Forward, uops, false));
    let dsi0 = &streams.nav_packs[0].dsi;
    assert_eq!(
        scan_step(dsi0, ScanDirection::Forward, 0.5),
        TrickStep::Jump {
            lbn: 2,
            finer_steps_available: false,
        }
    );
    let dsi2 = &streams.nav_packs[1].dsi;
    assert_eq!(
        scan_step(dsi2, ScanDirection::Forward, 0.5),
        TrickStep::NoMoreVideo
    );
    assert_eq!(
        scan_step(dsi2, ScanDirection::Backward, 0.5),
        TrickStep::Jump {
            lbn: 0,
            finer_steps_available: false,
        }
    );
    assert_eq!(
        oxideav_dvd::reference_frame_span(dsi0, 1),
        Some((0, 1)),
        "fast play reads the nav pack + first reference frame sector",
    );

    // -- Demux cell 2. ------------------------------------------------
    let mut demux2 = VobDemuxer::new();
    demux2
        .demux_range(&mut cursor, c2_first, c2_last - c2_first + 1)
        .expect("cell 2 demuxes");
    let streams2 = demux2.take();
    assert_eq!(streams2.video, b"CELL2-VIDEO");

    // Cell 2's authored still holds for exactly 2000 ms.
    let mut cell_still = ev2.still_clock().expect("cell 2 still");
    assert!(!cell_still.advance_ms(1999));
    assert!(cell_still.advance_ms(1));

    // -- PGC still: infinite + "Still off" prohibited. ---------------
    let ev3 = runner.next_event(&mut vm);
    assert_eq!(
        ev3,
        PlaybackEvent::PgcStill {
            still: StillTime::Infinite,
        }
    );
    let mut pgc_still = ev3.still_clock().expect("PGC still");
    assert!(
        !pgc_still.advance_ms(u64::MAX),
        "infinite still never expires"
    );
    assert!(
        !pgc_still.try_user_release(uops),
        "PGC UOP word bans Still off",
    );
    assert_eq!(pgc_still.phase(), StillPhase::Infinite);
    // A menu-button control transfer releases unconditionally.
    pgc_still.release();
    assert_eq!(pgc_still.phase(), StillPhase::Released);

    assert_eq!(runner.next_event(&mut vm), PlaybackEvent::Finished);
}
