# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- NAV-pack PCI **highlight information** (menu buttons): the
  `PciPacket` decoder previously read only `hli_ss`; it now
  materialises the full HLI_GI / SL_COLI / BTN_IT sub-structure when
  a VOBU declares buttons, clean-room per `mpucoder-pci_pkt.html` (no
  libdvdread / libdvdnav / libdvdcss / FFmpeg / VLC / mpv / xine
  source consulted).
  - **`HighlightInfo` + `PciPacket::highlight: Option<HighlightInfo>`**
    — the HLI_GI general-information block (`hli_s_ptm`, `hli_e_ptm`,
    `btn_sl_e_ptm`, raw `btn_md` grouping word, `btn_sn`, `btn_ns`,
    `nsl_btn_ns`, `fosl_btnn`, `foac_btnn`). `None` when the VOBU
    declares no buttons (`btn_ns == 0`) — the common case, not an
    error.
  - **`SlColi` + `SlColiCell`** — the three `SL_COLI_1..3`
    selection/action colour-and-contrast schemes. Each 8-byte scheme
    is decoded into selection + action arrays of four
    `{ color, contrast }` cells, indexed by emphasis code
    (`0` = background, `1` = pattern, `2` = emphasis1, `3` =
    emphasis2). `color` is a 4-bit PGC colour-LUT index; `contrast`
    is the 4-bit blend weight a subpicture/menu renderer applies.
  - **`ButtonInfo`** — one 18-byte `BTN_IT` entry: `btn_coln`
    colour-scheme selector, the 10-bit X/Y rectangular region
    (`start_x`/`end_x`/`start_y`/`end_y`), the auto-action flag, the
    four `up`/`down`/`left`/`right` D-pad adjacency selectors, and the
    raw 8-byte VM `command` (executing it is Phase 3c VM work per
    `mpucoder-vmi.html`). The button table holds exactly `btn_ns`
    entries; an over-long count (`> 36`) or a body too short to carry
    the declared table raises `Error::InvalidUdf`.
  - 4 new unit tests (`pci_without_buttons_yields_no_highlight`,
    `pci_decodes_single_button_highlight`,
    `pci_rejects_overlong_btn_ns`,
    `pci_rejects_truncated_button_table`); a new `add_one_button_hli`
    test helper injects a known single-button HLI block into the
    synthetic nav sector so every decoded field is asserted exactly.
  - **Note:** `PciPacket` and `NavPack` no longer derive `Copy`
    (`HighlightInfo` owns a `Vec<ButtonInfo>`); they remain `Clone`.

- PGC palette + command-table parse (richer Phase 2 IFO body): the
  `Pgc` materialiser now decodes the two PGC-header tables it
  previously skipped, both clean-room per `mpucoder-pgc.html` (no
  libdvdread / libdvdnav / libdvdcss / FFmpeg / VLC / mpv / xine
  source consulted).
  - **`PaletteEntry` + `Pgc::palette: [PaletteEntry; 16]`** — the
    subpicture/highlight colour-LUT at PGC offset `0x00A4`, sixteen
    `(0, Y, Cr, Cb)` cells (leading reserved byte dropped) surfaced
    as `{ y, cr, cb }`. This is the table an SPU display-control
    sequence indexes into via its 4-bit colour codes
    (`mpucoder-spu.html`), so a subtitle/menu renderer needs it to
    resolve a pixel to an actual YCrCb value.
  - **`NavCommand` + `PgcCommandTable` + `Pgc::commands:
    Option<PgcCommandTable>`** — the command table at
    `offset_commands` (previously only the *offset* was read). The
    8-byte header (pre/post/cell counts + `end_address`) is decoded
    and each list is carved into fixed 8-byte `NavCommand` words.
    The `pre + post + cell <= 128` spec invariant is enforced;
    truncated lists and over-long counts raise `Error::InvalidUdf`.
    Executing the words is deferred to the Phase 3c VM
    (`mpucoder-vmi.html`); at the container layer we expose the raw
    words plus a `NavCommand::command_type()` convenience (top three
    bits of byte 0 — the VMI command-group selector) so a downstream
    interpreter has what it needs without a full opcode model here.
  - 6 new unit tests (`palette_entry_skips_reserved_byte`,
    `command_table_carves_three_lists`,
    `command_table_rejects_overlong_count`,
    `command_table_rejects_truncated_list`,
    `pgc_without_command_table_yields_none`, plus extended
    palette/command assertions in
    `pgci_parses_one_pgc_with_three_cells`). The
    `build_pgc_with_cells` test helper now emits a real palette +
    4-word command table so the existing round-trip exercises the
    new fields.

- Phase 3b (VOB → MKV mux): clean-room glue between the Phase 3a VOB
  demuxer and `oxideav-mkv`'s `MkvMuxer::{add_chapter,write_packet,
  write_trailer}`. Gated behind a default-off `mkv-output` cargo
  feature so the dvd crate stays useful for chapter-introspection
  consumers and so default-feature CI doesn't have to pull the
  (still-unreleased at time of writing) MKV chapter API in. No
  external library source (libdvdread, libdvdnav, libdvdcss, FFmpeg,
  VLC, mpv, xine, HandBrake) was consulted.
  - **`pgc_time_to_ns(PgcTime) -> u64`** — RFC 9559 §5.1.7 needs
    `ChapterTimeStart` / `ChapterTimeEnd` in nanoseconds; DVD's BCD
    `hh:mm:ss:ff` field uses 30 fps (NTSC) or 25 fps (PAL) per
    `mpucoder-pgc.html`. Conversion is exact rational math so
    `0:0:1.15 @ 30 fps` becomes the spec-exact 1_500_000_000 ns
    (truncating with the obvious `1e9 / 30` constant would have
    rounded to 1_499_999_995 ns — see the regression test).
  - **`mkv_writer::write_title_to_mkv(disc, title_idx, image_path,
    out_path)`** — two-pass DVD → MKV converter. Pass 1 probes the
    title's cells to enumerate the (video, AC-3 × N, DTS × N, LPCM
    × N, subpicture × N) stream set so MKV's mandatory upfront
    `Tracks` element can be sized correctly. Pass 2 re-walks the
    cells and forwards each PES packet to `MkvMuxer::write_packet`
    with PTS preserved verbatim in the PES's 90 kHz time base; the
    muxer rescales to its internal 1 ms `TimecodeScale`. Chapter
    atoms are queued via `MkvMuxer::add_chapter` before
    `write_header`, one per `DvdChapter` from the PGC's PTT list,
    titled `"Chapter N"`.
  - **`pipeline::convert_dvd_to_mkv(source, title_idx, out_path)`**
    — high-level front door accepting either a `dvd://...` URI or a
    bare filesystem path. Auto-detect (`dvd://`) is rejected as a
    Phase-2 followup, matching the existing source-driver semantics.
  - **`pipeline::list_titles(source)`** — convenience wrapper around
    `DvdDisc::enumerate_titles` for CLI front-ends that want to
    surface the title list before letting the user pick one.
  - **Sector walker** — `walk_cell_sectors` re-uses the constants
    from `vob::{SC_*, looks_like_nav_pack, PackHeader, PesPacket}`
    so the pack-header + system-header + nav-pack + padding
    transitions are decoded once across the round. Nav-packs are
    consumed (validated via `NavPack::parse`) but not surfaced
    further; subpicture / DTS / LPCM PES payloads' first byte (the
    substream ID) is stripped before the body lands in MKV.
- Phase 3b tests (10 cases, all gated behind `--features
  mkv-output`): PgcTime → ns for NTSC 30 fps / PAL 25 fps / illegal-
  frame-rate / hour-boundary; stream classification (codec id +
  media type) for video / AC-3 / DTS / LPCM / subpicture; sort
  determinism for the stream set; plus three `pipeline::resolve_*`
  tests covering URI parsing (`dvd:///abs`, bare path, `dvd://`
  auto-detect rejection).

- Phase 3a (VOB demuxer): clean-room MPEG-PS pack + nav-pack walker
  + DVD-substream router per mpucoder-{packhdr,pes-hdr,mpeghdrs,
  pci_pkt,dsi_pkt,dvdmpeg}.html + stnsoft-{vobov,sys_hdr}.html. No
  libdvdread, libdvdnav, FFmpeg, VLC, mpv, or xine source consulted.
  - **`PackHeader`** — 14-byte MPEG-2 Program Stream pack header
    decoder: `00 00 01 BA` sync + 33-bit SCR base + 9-bit SCR_ext +
    22-bit `program_mux_rate` + 3-bit `pack_stuffing_length`. All
    five marker bits are validated; bad sync / missing marker /
    `mux_rate == 0` raise `Error::InvalidUdf`.
  - **`PesPacket<'a>`** — zero-copy PES decoder for the DVD subset:
    `0xBA` pack, `0xBB` system header, `0xBD` private_stream_1,
    `0xBE` padding, `0xBF` private_stream_2 (no extension), and
    `0xC0..=0xDF` / `0xE0..=0xEF` (MPEG-2 extension with 5-byte PTS
    or 10-byte PTS+DTS). `PTS_DTS_flags == 01` is rejected per spec.
  - **`DvdSubstream`** — typed substream classifier for the first
    payload byte of a 0xBD packet: `Subpicture(0x20..=0x3F)`,
    `Ac3(0x80..=0x87)`, `Dts(0x88..=0x8F)`, `Lpcm(0xA0..=0xA7)`
    with `track()` accessor normalising to 0..=7 (audio) / 0..=31
    (subpicture).
  - **`PciPacket`** — Presentation Control Information decoder for
    the DVD-Video NAV-pack's PCI half: `nv_pck_lbn`, `vobu_cat`,
    `vobu_uop_ctl`, `vobu_s_ptm`, `vobu_e_ptm`, `vobu_se_e_ptm`,
    `c_eltm`, `hli_ss`.
  - **`DsiPacket`** — Data Search Information decoder for the
    DSI half: `nv_pck_scr`, `nv_pck_lbn`, `vobu_ea`,
    `vobu_{1,2,3}stref_ea`, `vobu_vob_idn`, `vobu_c_idn`, `c_eltm`,
    and the 43-entry `vobu_sri` search-pointer table (0xEA..0x196)
    used for chapter-accurate forward/backward seek.
  - **`NavPack`** — 2048-byte sector-level decoder that validates
    pack header + system header + 0xBF/0x00 PCI prefix + 0xBF/0x01
    DSI prefix and surfaces `(pci, dsi)`. A cheap `looks_like_nav_pack`
    probe skips the full parse on demux routing.
  - **`VobDemuxer`** — stateful walker that consumes 2048-byte
    sectors and routes packets into per-stream buffers. Nav-packs
    are consumed and stashed in `VobStreams::nav_packs`; video PES
    payloads append to `video`; private_stream_1 payloads are
    classified and routed to AC-3 / DTS / LPCM / subpicture
    `BTreeMap<u8, Vec<u8>>` per track. The first substream-ID byte
    is stripped before append so consumers see clean substream
    bytes. `0xC0..=0xC7` (MPEG audio) is pooled into `ac3` so
    callers can probe codec from the first frame.
  - **`demux_vobs(&mut reader, &disc, ts, &cells) -> VobStreams`**
    + `demux_vobs_path(...)` convenience wrapper: resolves
    `(VobId, CellId)` pairs through `VtsCAdt::lookup`, translates
    title-relative sector positions to absolute LBA via
    `VTS_xx_1.VOB`'s base LBA, then runs each cell's range through
    `VobDemuxer`.
- Phase 3a tests (all synthetic, 12 cases): pack-header roundtrip
  + bad-sync + zero-mux-rate rejection, PES with/without PTS +
  bad-start-code rejection, DVD substream classification across all
  four substream families, NavPack parse + corrupt-DSI rejection,
  and an end-to-end synthetic VOBU (nav sector + video PES sector +
  AC-3 PES sector) showing the demuxer routes payloads correctly
  while preserving nav-pack metadata.

- Phase 2 (IFO body parsing): clean-room IFO structural decoder per
  mpucoder + stnsoft DVD-Video reference pages (no libdvdread /
  libdvdnav / libdvdcss / FFmpeg / VLC / mpv / xine source consulted).
  - **VMGI_MAT** — full `VIDEO_TS.IFO` Video Manager Information
    Management Table parse: last-sector + IFO-end + version + VMG
    category + provider ID + number-of-title-sets + sector pointers
    to FP_PGC / menu VOB / TT_SRPT / VMGM_PGCI_UT / VMG_PTL_MAIT /
    VMG_VTS_ATRT / TXTDT_MG / VMGM_C_ADT / VMGM_VOBU_ADMAP.
  - **VTSI_MAT** — full `VTS_xx_0.IFO` Video Title Set Information
    Management Table parse: title-set last sector + IFO-end + version
    + VTS category + sector pointers to PTT_SRPT / PGCI / VTSM_PGCI_UT
    / TMAPTI / VTSM_C_ADT / VTSM_VOBU_ADMAP / VTS_C_ADT / VTS_VOBU_ADMAP.
  - **TT_SRPT** — Title Search Pointer Table walker (8-byte header
    + N × 12-byte entries) exposing per-title `(VTS_number,
    VTS_TTN, chapter_count, angle_count, parental_mask,
    vts_start_sector)`.
  - **VTS_PTT_SRPT** — Part-of-Title (chapter) search pointer table
    walker with per-title PTT body inferred from the offset list
    (boundaries derived from the next-title offset, or `end_address +
    1` for the last title).
  - **VTS_PGCI** — Program Chain Information table: 8-byte header +
    SRP list (per-PGC category + offset) + each PGC's 0xEC-byte
    header (nr_of_programs, nr_of_cells, BCD playback time +
    frame-rate bits, prohibited UOPs, next/prev/goup PGCN, still
    time, playback mode) + program map + Cell Playback Information
    Table (24 bytes per cell — category, restricted flag, still
    time, cell command, BCD playback time, first/last VOBU start +
    ILVU/last-VOBU end sectors) + Cell Position Information Table
    (4 bytes per cell — VOB ID + Cell ID).
  - **VTS_C_ADT** — Cell Address Table walker (shared format with
    VMGM_C_ADT + VTSM_C_ADT) — entry count recovered from the
    `end_address` header field, `(vob_id, cell_id) → (start_sector,
    end_sector)` lookup helper.
  - **`PgcTime`** — BCD playback-time decoder for the `hh:mm:ss:ff`
    field with `FrameRate::{Pal25, Ntsc30, Illegal, Reserved}`
    discrimination (bits 7+6 of frame byte per mpucoder-pgc.html).
  - **`VtsIfo`** materialiser — `parse(buf, vts_number) -> VtsIfo`
    pulls VTSI_MAT, PTT_SRPT, PGCI, and C_ADT into a single view
    and rebuilds per-title chapter lists (`DvdTitle` → `Vec<DvdChapter>`,
    each chapter carrying its first/last cell numbers derived from
    the PGC's program map).
  - **`DvdDisc`** Phase-2 API — `parse_vmg(&reader) -> VmgIfo`,
    `parse_vts(&reader, ts_index) -> VtsIfo`, `enumerate_titles(
    &reader) -> Vec<DvdTitleEntry>`, and a `parse_vmg_tt_srpt`
    convenience accessor.
- Phase 2 tests (all synthetic): VMGI_MAT parse + bad-magic
  rejection, VTSI_MAT parse, TT_SRPT walk (3 titles), PGCI with one
  PGC + 3 cells, VTS_PTT_SRPT walking 2 titles × 5 chapters, VTS_C_ADT
  with 4 cell entries, PgcTime decode (NTSC 30 fps + PAL 25 fps), and
  a full hand-built 4-sector composite VTS_xx_0.IFO image (VTSI_MAT
  + PTT_SRPT + PGCI + C_ADT) round-tripped through `VtsIfo::parse`
  with chapter-cell-range assertions.

- Bootstrap (Phase 1 — filesystem + disc detection): clean-room
  read-only DVD-Video support per ECMA-267 (DVD-ROM) + ECMA-268
  (DVD-ROM file system) + OSTA UDF 1.02 + the ECMA-167 UDF base
  standard. NO libdvdread, libdvdnav, libdvdcss, FFmpeg, xine, mpv,
  or VLC source consulted.
  - **ISO 9660 reader** (`iso9660` module) — Primary Volume Descriptor
    at sector 16, root directory record + path-table walk, A-string /
    D-string decode, recursive directory enumeration.
  - **UDF 1.02 mount** (`udf` module) — Anchor Volume Descriptor
    Pointer probing (sectors 256 / 512 / N-256), Volume Descriptor
    Sequence (Primary VD, Partition Descriptor, Logical Volume
    Descriptor, Terminating Descriptor), File Set Descriptor, root
    File Identifier Descriptor walk, File Entry / ICB with Short_ad
    / Long_ad / Ext_ad allocation descriptors, OSTA compressed
    Unicode (compression IDs 8 + 16) per UDF 1.02 §2.1.3.
  - **DVD-Video disc detection** (`disc` module) — sniff for ISO 9660
    PVD + UDF AVDP on a file or block device, require a top-level
    `VIDEO_TS/` directory containing `VIDEO_TS.IFO`, enumerate
    `VIDEO_TS.IFO` + `.VOB` + `.BUP` + per-VTS `VTS_xx_0.IFO` /
    `VTS_xx_0.VOB` (menu) / `VTS_xx_1..9.VOB` (title) / `VTS_xx_0.BUP`.
  - **`dvd://` source driver** (`source` module, default-on `registry`
    feature) — registers under `oxideav_core::SourceRegistry` so a
    `dvd:///path/to/disc.iso` URL surfaces a typed `DvdDiscSource`
    that carries the file enumeration + byte-range read.
- Tests (all synthetic, no real disc data): ISO 9660 PVD / d-string /
  path-table / dir walk / nested dir / EOF rejection; UDF 1.02 AVDP /
  tag checksum / LVID / FSD / FID iteration / Short_ad / Long_ad /
  Ext_ad / d-string compression-mode 8 vs 16; DVD-Video single-VTS,
  multi-VTS, AUDIO_TS-empty, rejection-when-no-VIDEO_TS; one full
  round-trip against a hand-assembled ~64 KB synthetic disc image
  under `tests/data/`.
