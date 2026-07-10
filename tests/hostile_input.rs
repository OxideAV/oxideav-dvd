//! Hostile-input hardening for the DVD-Video parsers.
//!
//! Every on-disc structure this crate decodes is attacker-controlled:
//! a malformed or truncated ISO/VOB must never panic the host player,
//! only ever surface a typed `Err`. These tests feed each public parse
//! entry point a firehose of pseudo-random, truncated, and
//! deliberately size-lying buffers and assert the process survives
//! (a panic aborts the test binary, failing the run).
//!
//! No fixtures are read from disk — every buffer is synthesised in
//! process from a small deterministic PRNG so the suite is
//! hermetic and reproducible.

use oxideav_dvd::ifo::{
    Pgc, Pgci, PgciUt, TtSrpt, VmgIfo, VmgPtlMait, VmgVtsAtrt, VobuAdmap, VtsCAdt, VtsIfo,
    VtsPttSrpt, VtsTmapti,
};
use oxideav_dvd::vob::{
    AudioSubstreamHeader, DsiPacket, NavPack, PackHeader, PciPacket, SystemHeader,
};
use oxideav_dvd::{
    scan_video_sequence, Ac3Header, DtsHeader, GopHeader, PictureCodingExtension, PictureHeader,
    SequenceDisplayExtension, SequenceExtension, SequenceHeader, SubPictureUnit,
};

/// A deterministic xorshift64* PRNG — no external crate, fully
/// reproducible so a failing seed can be replayed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 33) as u8
    }
    /// A buffer of a random length in `0..=max`, filled with random
    /// bytes.
    fn buf(&mut self, max: usize) -> Vec<u8> {
        let len = (self.next_u64() as usize) % (max + 1);
        let mut v = vec![0u8; len];
        for b in v.iter_mut() {
            *b = self.byte();
        }
        v
    }
}

/// Run `f` over `iters` random buffers of length `0..=max`. `f` must
/// not panic on any input. The buffer is passed by slice.
fn fuzz_slice(seed: u64, iters: usize, max: usize, f: impl Fn(&[u8])) {
    let mut rng = Rng::new(seed);
    for _ in 0..iters {
        let b = rng.buf(max);
        f(&b);
    }
}

const ITERS: usize = 4000;

#[test]
fn fuzz_vob_leaf_parsers() {
    fuzz_slice(1, ITERS, 64, |b| {
        let _ = PackHeader::parse(b);
    });
    fuzz_slice(2, ITERS, 128, |b| {
        let _ = SystemHeader::parse(b);
    });
    fuzz_slice(3, ITERS, 128, |b| {
        let _ = AudioSubstreamHeader::parse(b);
    });
}

#[test]
fn fuzz_nav_pack_structures() {
    // Full nav-pack sector is 2048 bytes; feed a range that spans
    // undersized, exact, and oversized.
    fuzz_slice(4, ITERS, 2100, |b| {
        let _ = NavPack::parse(b);
    });
    fuzz_slice(5, ITERS, 0x400, |b| {
        let _ = PciPacket::parse(b);
    });
    fuzz_slice(6, ITERS, 0x400, |b| {
        let _ = DsiPacket::parse(b);
    });
}

#[test]
fn fuzz_ifo_top_level() {
    // These reject on the ASCII magic almost immediately, so random
    // data mostly exercises the length gates; the magic-prefixed
    // seeds in the size-lie test drive the deep paths.
    fuzz_slice(7, ITERS, 0x900, |b| {
        let _ = VmgIfo::parse(b);
    });
    fuzz_slice(8, ITERS, 0x900, |b| {
        let _ = VtsIfo::parse(b, 1);
    });
}

#[test]
fn fuzz_ifo_tables() {
    fuzz_slice(9, ITERS, 512, |b| {
        let _ = Pgc::parse(b);
    });
    fuzz_slice(10, ITERS, 512, |b| {
        let _ = Pgci::parse(b);
    });
    fuzz_slice(12, ITERS, 256, |b| {
        let _ = TtSrpt::parse(b);
    });
    fuzz_slice(13, ITERS, 256, |b| {
        let _ = VtsPttSrpt::parse(b);
    });
    fuzz_slice(14, ITERS, 256, |b| {
        let _ = VtsCAdt::parse(b);
    });
    fuzz_slice(15, ITERS, 256, |b| {
        let _ = VobuAdmap::parse(b);
    });
    fuzz_slice(16, ITERS, 256, |b| {
        let _ = VtsTmapti::parse(b);
    });
    fuzz_slice(17, ITERS, 256, |b| {
        let _ = VmgPtlMait::parse(b);
    });
    fuzz_slice(18, ITERS, 256, |b| {
        let _ = VmgVtsAtrt::parse(b);
    });
    fuzz_slice(19, ITERS, 256, |b| {
        let _ = PgciUt::parse(b);
    });
}

#[test]
fn fuzz_spu_decoder() {
    fuzz_slice(20, ITERS, 512, |b| {
        let unit = SubPictureUnit::parse(b);
        // Compositing walks the RLE pixel data with attacker-chosen
        // offsets/dimensions — exercise it when the parse succeeds.
        if let Ok(u) = unit {
            let palette = [oxideav_dvd::ifo::PaletteEntry::default(); 16];
            let _ = u.composite(b, &palette);
        }
    });
}

#[test]
fn fuzz_mpeg_headers() {
    fuzz_slice(21, ITERS, 128, |b| {
        let _ = SequenceHeader::parse(b);
    });
    fuzz_slice(22, ITERS, 128, |b| {
        let _ = SequenceExtension::parse(b);
    });
    fuzz_slice(23, ITERS, 128, |b| {
        let _ = SequenceDisplayExtension::parse(b);
    });
    fuzz_slice(24, ITERS, 128, |b| {
        let _ = GopHeader::parse(b);
    });
    fuzz_slice(25, ITERS, 128, |b| {
        let _ = PictureHeader::parse(b);
    });
    fuzz_slice(26, ITERS, 128, |b| {
        let _ = PictureCodingExtension::parse(b);
    });
    fuzz_slice(27, ITERS, 1024, |b| {
        let _ = scan_video_sequence(b);
    });
}

/// Build a small but well-formed Sub-Picture Unit: SPUH + one DCSQ
/// carrying SET_COLOR / SET_CONTR / SET_DAREA / SET_DSPXA / STA_DSP /
/// CMD_END, so `SubPictureUnit::parse` succeeds and `composite` walks
/// the RLE renderer. Mutating this seed drives attacker-controlled
/// display areas, pixel-data offsets, and control-sequence offsets
/// through the decoder + compositor — random data rarely reaches the
/// renderer because `parse` seldom succeeds on it.
fn build_valid_spu() -> Vec<u8> {
    let mut buf = vec![0u8; 0x28];
    buf[0..2].copy_from_slice(&0x0028u16.to_be_bytes()); // SPDSZ
    buf[2..4].copy_from_slice(&0x0010u16.to_be_bytes()); // SP_DCSQTA
    let dcsq = 0x10;
    buf[dcsq..dcsq + 2].copy_from_slice(&0x0000u16.to_be_bytes()); // start_time
    buf[dcsq + 2..dcsq + 4].copy_from_slice(&(dcsq as u16).to_be_bytes()); // next = self
    let mut o = dcsq + 4;
    buf[o] = 0x03; // SET_COLOR
    buf[o + 1] = 0xFE;
    buf[o + 2] = 0xDC;
    o += 3;
    buf[o] = 0x04; // SET_CONTR
    buf[o + 1] = 0xFF;
    buf[o + 2] = 0x80;
    o += 3;
    buf[o] = 0x05; // SET_DAREA: x 0..3, y 0..3
    buf[o + 1] = 0x00;
    buf[o + 2] = 0x00;
    buf[o + 3] = 0x03;
    buf[o + 4] = 0x00;
    buf[o + 5] = 0x00;
    buf[o + 6] = 0x03;
    o += 7;
    buf[o] = 0x06; // SET_DSPXA: top=0x0004, bottom=0x0004
    buf[o + 1..o + 3].copy_from_slice(&0x0004u16.to_be_bytes());
    buf[o + 3..o + 5].copy_from_slice(&0x0004u16.to_be_bytes());
    o += 5;
    buf[o] = 0x01; // STA_DSP
    o += 1;
    buf[o] = 0xFF; // CMD_END
    buf
}

#[test]
fn valid_spu_seed_parses() {
    let buf = build_valid_spu();
    let unit = SubPictureUnit::parse(&buf).expect("valid SPU seed must parse");
    assert_eq!(unit.control_sequences.len(), 1);
}

#[test]
fn fuzz_spu_seed_mutation() {
    // Byte-flip the valid SPU and run parse → composite. This drives
    // attacker-chosen SET_DAREA dimensions, SET_DSPXA pixel offsets,
    // and DCSQ next-offsets through the RLE decoder + compositor.
    let seed = build_valid_spu();
    let palette = [oxideav_dvd::ifo::PaletteEntry::default(); 16];
    let mut rng = Rng::new(0x59D5);
    for _ in 0..40_000 {
        let extra = (rng.next_u64() as usize) % 48;
        let mut buf = vec![0u8; seed.len() + extra];
        buf[..seed.len()].copy_from_slice(&seed);
        let flips = 1 + (rng.next_u64() as usize % 6);
        for _ in 0..flips {
            let pos = (rng.next_u64() as usize) % buf.len();
            buf[pos] = rng.byte();
        }
        if let Ok(unit) = SubPictureUnit::parse(&buf) {
            let _ = unit.composite(&buf, &palette);
            if let (Some((w, h)), Some((top, bot))) =
                (unit.display_dimensions(), unit.pixel_data_offsets())
            {
                let lines = h.div_ceil(2);
                if let Some(px) = buf.get(top as usize..) {
                    let _ = oxideav_dvd::render_field(px, w, lines);
                }
                if let Some(px) = buf.get(bot as usize..) {
                    let _ = oxideav_dvd::render_field(px, w, lines);
                }
            }
        }
    }
}

const DVD_SECTOR: usize = 2048;

/// Build a small but structurally valid `VTS_xx_0.IFO` image: a
/// VTSI_MAT in sector 0 with sector pointers to a VTS_PTT_SRPT
/// (sector 1), a VTS_PGCI with one 5-cell / 3-program PGC (sector 2),
/// and a VTS_C_ADT (sector 3). Mirrors the layout the crate's own
/// round-trip test exercises, so `VtsIfo::parse` walks every deep
/// path (sector-pointer resolution → PGC body → chapter
/// materialisation) before we start mutating it.
fn build_valid_vtsi() -> Vec<u8> {
    let mut img = vec![0u8; DVD_SECTOR * 4];

    // Sector 0: VTSI_MAT — magic + sector pointers.
    img[0..12].copy_from_slice(b"DVDVIDEO-VTS");
    img[0x000C..0x0010].copy_from_slice(&100_000u32.to_be_bytes()); // last_sector_title_set
    img[0x001C..0x0020].copy_from_slice(&15u32.to_be_bytes()); // last_sector_ifo
    img[0x0020..0x0022].copy_from_slice(&0x0011u16.to_be_bytes()); // version
    img[0x0080..0x0084].copy_from_slice(&0x01FFu32.to_be_bytes()); // VTSI_MAT end
    img[0x00C4..0x00C8].copy_from_slice(&100u32.to_be_bytes()); // title_vob_sector
    img[0x00C8..0x00CC].copy_from_slice(&1u32.to_be_bytes()); // VTS_PTT_SRPT sector
    img[0x00CC..0x00D0].copy_from_slice(&2u32.to_be_bytes()); // VTS_PGCI sector
    img[0x00E0..0x00E4].copy_from_slice(&3u32.to_be_bytes()); // VTS_C_ADT sector

    // Sector 2: VTS_PGCI — 1 PGC, 5 cells, 3 programs (map = 1,3,5).
    let header_size = 0xEC;
    let prog_map_size = 4;
    let cpbi_size = 5 * 24;
    let cpos_size = 5 * 4;
    let pgc_len = header_size + prog_map_size + cpbi_size + cpos_size;
    let mut pgc = vec![0u8; pgc_len];
    pgc[0x0002] = 3; // number_of_programs
    pgc[0x0003] = 5; // number_of_cells
    pgc[0x0004..0x0008].copy_from_slice(&[0x00, 0x15, 0x00, 0xE0]); // playback_time
    pgc[0x00E6..0x00E8].copy_from_slice(&(header_size as u16).to_be_bytes());
    pgc[0x00E8..0x00EA].copy_from_slice(&((header_size + prog_map_size) as u16).to_be_bytes());
    pgc[0x00EA..0x00EC]
        .copy_from_slice(&((header_size + prog_map_size + cpbi_size) as u16).to_be_bytes());
    pgc[header_size] = 1;
    pgc[header_size + 1] = 3;
    pgc[header_size + 2] = 5;
    for i in 0..5u32 {
        let base = header_size + prog_map_size + (i as usize) * 24;
        pgc[base + 4..base + 8].copy_from_slice(&[0, 1, 0, 0xE0]);
        let s0 = 1000 + i * 1000;
        pgc[base + 8..base + 12].copy_from_slice(&s0.to_be_bytes());
        pgc[base + 12..base + 16].copy_from_slice(&(s0 + 999).to_be_bytes());
        pgc[base + 16..base + 20].copy_from_slice(&s0.to_be_bytes());
        pgc[base + 20..base + 24].copy_from_slice(&(s0 + 999).to_be_bytes());
    }
    for i in 0..5u32 {
        let base = header_size + prog_map_size + cpbi_size + (i as usize) * 4;
        pgc[base..base + 2].copy_from_slice(&1u16.to_be_bytes());
        pgc[base + 3] = (i + 1) as u8;
    }
    let body_off = 8 + 8;
    let pgci_total = body_off + pgc.len();
    let mut pgci = vec![0u8; pgci_total];
    pgci[0..2].copy_from_slice(&1u16.to_be_bytes());
    pgci[4..8].copy_from_slice(&((pgci_total - 1) as u32).to_be_bytes());
    pgci[12..16].copy_from_slice(&(body_off as u32).to_be_bytes());
    pgci[body_off..].copy_from_slice(&pgc);
    img[2 * DVD_SECTOR..2 * DVD_SECTOR + pgci.len()].copy_from_slice(&pgci);

    // Sector 1: VTS_PTT_SRPT — 1 title, 3 chapters at programs 1,2,3.
    let hdr = 8 + 4;
    let total = hdr + 3 * 4;
    let mut ptt = vec![0u8; total];
    ptt[0..2].copy_from_slice(&1u16.to_be_bytes());
    ptt[4..8].copy_from_slice(&((total - 1) as u32).to_be_bytes());
    ptt[8..12].copy_from_slice(&(hdr as u32).to_be_bytes());
    for ci in 0..3u16 {
        let base = hdr + (ci as usize) * 4;
        ptt[base..base + 2].copy_from_slice(&1u16.to_be_bytes());
        ptt[base + 2..base + 4].copy_from_slice(&(ci + 1).to_be_bytes());
    }
    img[DVD_SECTOR..DVD_SECTOR + ptt.len()].copy_from_slice(&ptt);

    // Sector 3: VTS_C_ADT — 5 cells for VOB 1.
    let n = 5;
    let clen = 8 + n * 12;
    let mut cadt = vec![0u8; clen];
    cadt[0..2].copy_from_slice(&1u16.to_be_bytes());
    cadt[4..8].copy_from_slice(&((clen - 1) as u32).to_be_bytes());
    for i in 0..n {
        let base = 8 + i * 12;
        cadt[base..base + 2].copy_from_slice(&1u16.to_be_bytes());
        cadt[base + 2] = (i + 1) as u8;
        let s0 = 1000 + (i as u32) * 1000;
        cadt[base + 4..base + 8].copy_from_slice(&s0.to_be_bytes());
        cadt[base + 8..base + 12].copy_from_slice(&(s0 + 999).to_be_bytes());
    }
    img[3 * DVD_SECTOR..3 * DVD_SECTOR + cadt.len()].copy_from_slice(&cadt);

    img
}

#[test]
fn valid_vtsi_seed_parses() {
    // Sanity: the seed must actually reach the deep path, otherwise
    // the mutation fuzz below would only test the reject gates.
    let img = build_valid_vtsi();
    let vts = VtsIfo::parse(&img, 1).expect("valid seed must parse");
    assert_eq!(vts.pgcs.len(), 1);
    assert_eq!(vts.titles[0].chapter_count, 3);
}

#[test]
fn fuzz_vtsi_bit_flips() {
    // Byte-flip mutation fuzz: start from a valid IFO and perturb a
    // handful of bytes each iteration. This drives attacker-chosen
    // sector pointers, table offsets, PGC counts, and PTT PGCN/PGN
    // references through the deep materialisation path — the size-lie
    // vectors random data can't reach past the magic gate.
    let seed = build_valid_vtsi();
    let mut rng = Rng::new(0xF1F0);
    for _ in 0..20_000 {
        let mut img = seed.clone();
        let flips = 1 + (rng.next_u64() as usize % 6);
        for _ in 0..flips {
            let pos = (rng.next_u64() as usize) % img.len();
            img[pos] = rng.byte();
        }
        // Must return Ok or Err — never panic.
        let _ = VtsIfo::parse(&img, 1);
    }
    // Also stress the MAT sector pointers directly with extreme values.
    for &ptr_off in &[0x00C8usize, 0x00CC, 0x00D4, 0x00E0, 0x00E4] {
        for &val in &[
            0u32,
            1,
            3,
            0x7FFF_FFFF,
            0xFFFF_FFFF,
            100_000,
            DVD_SECTOR as u32,
        ] {
            let mut img = seed.clone();
            img[ptr_off..ptr_off + 4].copy_from_slice(&val.to_be_bytes());
            let _ = VtsIfo::parse(&img, 1);
        }
    }
}

#[test]
fn fuzz_audio_headers() {
    fuzz_slice(28, ITERS, 64, |b| {
        let _ = Ac3Header::parse(b);
    });
    fuzz_slice(29, ITERS, 64, |b| {
        let _ = DtsHeader::parse(b);
    });
    fuzz_slice(30, ITERS, 64, |b| {
        let _ = oxideav_dvd::peel_lpcm_payload(b);
    });
}

#[test]
fn fuzz_udf_descriptors() {
    use oxideav_dvd::udf::{
        AnchorVolumeDescriptorPointer, DescriptorTag, ExtAd, FileEntry, FileIdentifierDescriptor,
        FileSetDescriptor, IcbTag, LbAddr, LogicalVolumeDescriptor, LongAd, PartitionDescriptor,
        ShortAd,
    };
    // The disc-mount surface: allocation descriptors, ICB tags, and
    // the variable-length File Entry (allocation-descriptor list is
    // length-driven — a size-lie there is the classic UDF panic).
    fuzz_slice(31, ITERS, 32, |b| {
        let _ = DescriptorTag::parse(b);
    });
    fuzz_slice(32, ITERS, 32, |b| {
        let _ = ShortAd::parse(b);
    });
    fuzz_slice(33, ITERS, 32, |b| {
        let _ = LongAd::parse(b);
    });
    fuzz_slice(34, ITERS, 32, |b| {
        let _ = ExtAd::parse(b);
    });
    fuzz_slice(35, ITERS, 16, |b| {
        let _ = LbAddr::parse(b);
    });
    fuzz_slice(36, ITERS, 64, |b| {
        let _ = IcbTag::parse(b);
    });
    fuzz_slice(37, ITERS, 640, |b| {
        let _ = AnchorVolumeDescriptorPointer::parse(b);
    });
    fuzz_slice(38, ITERS, 640, |b| {
        let _ = PartitionDescriptor::parse(b);
    });
    fuzz_slice(39, ITERS, 640, |b| {
        let _ = LogicalVolumeDescriptor::parse(b);
    });
    fuzz_slice(40, ITERS, 640, |b| {
        let _ = FileSetDescriptor::parse(b);
    });
    fuzz_slice(41, ITERS, 640, |b| {
        let _ = FileIdentifierDescriptor::parse(b);
    });
    // FileEntry is the crown jewel — give it more room and iterations.
    fuzz_slice(42, ITERS * 3, 2048, |b| {
        let _ = FileEntry::parse(b);
    });
}

/// Build a structurally valid UDF File Entry: a checksum-correct
/// descriptor tag (id = FileEntry = 261), an ICB tag selecting the
/// short-AD form, and a 24-byte allocation-descriptor area. This
/// reaches the allocation-descriptor loop — the length-driven
/// (`l_ad`) code random data never gets past the tag-checksum gate.
fn build_valid_file_entry() -> Vec<u8> {
    let l_ad: u32 = 24; // 3 short_ads
    let mut fe = vec![0u8; 176 + l_ad as usize];
    // Descriptor tag: id 261 (FileEntry), little-endian.
    fe[0..2].copy_from_slice(&261u16.to_le_bytes());
    fe[2..4].copy_from_slice(&0x0102u16.to_le_bytes()); // descriptor version
    fe[5] = 0; // reserved must be zero
               // ICB tag occupies FE bytes 16..36; flags at 34..36. flags &
               // 0b111 == 0 selects the short-AD form.
    fe[34..36].copy_from_slice(&0u16.to_le_bytes());
    // l_ea = 0, l_ad = 24.
    fe[168..172].copy_from_slice(&0u32.to_le_bytes());
    fe[172..176].copy_from_slice(&l_ad.to_le_bytes());
    // One non-zero short_ad so the extent-push path runs.
    fe[176..180].copy_from_slice(&2048u32.to_le_bytes()); // length
    fe[180..184].copy_from_slice(&5u32.to_le_bytes()); // block location
                                                       // Checksum: sum of tag bytes 0..16 except byte 4, & 0xFF.
    let sum: u32 = fe[0..16]
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .map(|(_, b)| *b as u32)
        .sum();
    fe[4] = (sum & 0xFF) as u8;
    fe
}

#[test]
fn valid_file_entry_seed_parses() {
    use oxideav_dvd::udf::FileEntry;
    let fe = build_valid_file_entry();
    FileEntry::parse(&fe).expect("valid FE seed must parse");
}

#[test]
fn fuzz_file_entry_alloc_descriptors() {
    use oxideav_dvd::udf::FileEntry;
    // Keep the checksum-correct tag (bytes 0..16) intact and perturb
    // everything after it — ICB flags (ad_type selector), l_ea, l_ad,
    // and the allocation area. This drives every ad_type branch and
    // every `l_ea`/`l_ad` size-lie through the extent loops.
    let seed = build_valid_file_entry();
    let mut rng = Rng::new(0xAD10C);
    for _ in 0..30_000 {
        // Random buffer length: sometimes shorter than the seed
        // (truncation), sometimes longer (over-run headroom).
        let extra = (rng.next_u64() as usize) % 64;
        let mut fe = vec![0u8; seed.len() + extra];
        let copy = seed.len().min(fe.len());
        fe[..copy].copy_from_slice(&seed[..copy]);
        // Perturb bytes 16.. so the tag stays valid but the ICB /
        // length fields / allocation area are attacker-controlled.
        let flips = 1 + (rng.next_u64() as usize % 8);
        for _ in 0..flips {
            if fe.len() > 16 {
                let pos = 16 + (rng.next_u64() as usize % (fe.len() - 16));
                fe[pos] = rng.byte();
            }
        }
        // Occasionally truncate hard to exercise the prefix gate.
        if rng.next_u64() % 5 == 0 {
            let n = (rng.next_u64() as usize) % (seed.len() + 1);
            fe.truncate(n);
        }
        let _ = FileEntry::parse(&fe);
    }
}

#[test]
fn fuzz_iso9660() {
    use oxideav_dvd::iso9660::{parse_l_path_table, DirectoryRecord, PrimaryVolumeDescriptor};
    fuzz_slice(43, ITERS, 2100, |b| {
        let _ = PrimaryVolumeDescriptor::parse(b);
    });
    fuzz_slice(44, ITERS, 128, |b| {
        let _ = DirectoryRecord::parse(b);
    });
    fuzz_slice(45, ITERS, 512, |b| {
        let _ = parse_l_path_table(b);
    });
}

#[test]
fn fuzz_vm_interpreter() {
    use oxideav_dvd::ifo::NavCommand;
    use oxideav_dvd::Vm;
    // A malicious command list must always terminate (runaway-loop
    // budget) and never panic. Feed random 8-byte command words —
    // including Goto-heavy lists that could loop forever without the
    // step bound.
    let mut rng = Rng::new(0x5150);
    for _ in 0..8000 {
        let n = (rng.next_u64() as usize) % 40;
        let list: Vec<NavCommand> = (0..n)
            .map(|_| {
                let w = rng.next_u64();
                NavCommand {
                    bytes: w.to_be_bytes(),
                }
            })
            .collect();
        let mut vm = Vm::new();
        // run_list is the whole-list driver; must return, not hang.
        let _ = vm.run_list(&list);
        // Also exercise the single-step decoder over each word.
        for nc in &list {
            let mut vm2 = Vm::new();
            let _ = vm2.step(nc.decode());
        }
    }
}

#[test]
fn fuzz_copy_control() {
    use oxideav_dvd::copyctl::{CopyControlInfo, CprMai};
    // CPR_MAI: undersized, exact, oversized.
    fuzz_slice(46, ITERS, 16, |b| {
        let _ = CprMai::parse(b);
    });
    fuzz_slice(47, ITERS / 4, 2100, |b| {
        let _ = CprMai::from_data_frame(b);
    });
    // The analog-output field decoder is total over u8 — sweep the
    // whole domain and require encode -> decode stability.
    for f in 0..=255u8 {
        let cci = CopyControlInfo::from_analog_field(f);
        assert_eq!(
            CopyControlInfo::from_analog_field(cci.to_analog_field()),
            cci
        );
    }
}

/// A wire-valid 14-byte pack header (SCR 0, mux_rate 25200, no
/// stuffing) — the gate random data almost never passes, so seeding
/// it lets the fuzz reach the PES walker behind it.
const VALID_PACK_HEADER: [u8; 14] = [
    0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xC3, 0x00,
];

#[test]
fn fuzz_vob_demuxer_push_sector() {
    use oxideav_dvd::vob::VobDemuxer;

    // Phase 1: pure random sectors — virtually all rejected at the
    // pack-header gate; must never panic.
    let mut rng = Rng::new(0xD3D);
    let mut demuxer = VobDemuxer::new();
    let mut sec = vec![0u8; 2048];
    for _ in 0..1500 {
        for b in sec.iter_mut() {
            *b = rng.byte();
        }
        let _ = demuxer.push_sector(&sec);
    }

    // Phase 2: valid pack header + attacker-controlled PES area, so
    // the in-sector PES walker, the substream router, and the census
    // all run on hostile payloads. Splice in 0x000001BD start codes
    // with lying lengths and arbitrary substream-ID bytes.
    for _ in 0..1500 {
        sec[..14].copy_from_slice(&VALID_PACK_HEADER);
        for b in sec[14..].iter_mut() {
            *b = rng.byte();
        }
        let splices = rng.next_u64() % 4;
        for _ in 0..splices {
            let pos = 14 + (rng.next_u64() as usize % (2048 - 14 - 6));
            sec[pos..pos + 3].copy_from_slice(&[0x00, 0x00, 0x01]);
            // Weighted towards private_stream_1 / padding / system.
            sec[pos + 3] = match rng.next_u64() % 4 {
                0 => 0xBD,
                1 => 0xBE,
                2 => 0xBB,
                _ => rng.byte(),
            };
        }
        let _ = demuxer.push_sector(&sec);
    }

    // Phase 3: byte-flip mutation of a fully valid audio sector
    // (SDDS track 0, generic FrmCnt + FirstAccUnit prefix) so the
    // routing/census paths see near-valid input too.
    let mut valid = vec![0u8; 2048];
    valid[..14].copy_from_slice(&VALID_PACK_HEADER);
    let payload_len = 32usize;
    let pes_len = 3 + 1 + payload_len; // ext header + substream + body
    valid[14..20].copy_from_slice(&[0x00, 0x00, 0x01, 0xBD, (pes_len >> 8) as u8, pes_len as u8]);
    valid[20] = 0b1000_0000;
    valid[21] = 0x00;
    valid[22] = 0x00; // header_data_len
    valid[23] = 0x90; // SDDS substream 0
    valid[24] = 0x01; // FrmCnt
    valid[25] = 0x00;
    valid[26] = 0x01; // FirstAccUnit
    {
        let mut d = VobDemuxer::new();
        d.push_sector(&valid).expect("valid SDDS seed must demux");
        let s = d.take();
        // Routed body = everything after the substream-ID byte
        // (FrmCnt + FirstAccUnit + codec bytes) = payload_len.
        assert_eq!(s.sdds.get(&0).map(Vec::len), Some(payload_len));
    }
    for _ in 0..2000 {
        let mut mutated = valid.clone();
        let flips = 1 + (rng.next_u64() as usize % 6);
        for _ in 0..flips {
            let pos = (rng.next_u64() as usize) % mutated.len();
            mutated[pos] = rng.byte();
        }
        let _ = demuxer.push_sector(&mutated);
    }

    // The accumulated state and taxonomy accessors must stay
    // well-formed after the firehose.
    let streams = demuxer.take();
    for sub in streams.substreams_seen() {
        assert!(sub.track() < sub.kind().capacity());
    }
    for (id, stat) in streams.unallocated_substreams() {
        assert!(oxideav_dvd::DvdSubstreamKind::classify(id).is_none());
        assert!(stat.packets > 0);
    }
}
