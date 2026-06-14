# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **HLI_GI `btn_md` typed decode.** `HighlightInfo::button_mode()`
  now returns a `ButtonMode { group_count, group_types: [u8; 3] }`
  decoded from the raw `btn_md` word per the `btn_md word` sub-table
  of `docs/container/dvd/application/mpucoder-pci_pkt.html`:
  `btngr_ns` (number of button groups, u16 bits 13..12) and the three
  3-bit `btngrN_ty` group-type codes (bits 10..8 / 6..4 / 2..0), with
  the reserved bits (15..14, 11, 7, 3) masked out. `ButtonMode`
  also provides `from_btn_md` / `to_btn_md` (reserved-bit-dropping
  round-trip). The reference labels the type codes "normal / lb /
  p/s" (normal / letterbox / pan-scan) but gives no numeric
  value-to-name mapping, so the codes are surfaced raw rather than as
  a named enum; the field had previously been kept as an opaque `u16`.

- **PCI NSML_AGLI non-seamless angle jump table.** `PciPacket` now
  decodes the 36-byte NSML_AGLI block at PCI packet offset
  `0x3C..0x60` into a typed `NsmlAgli { cells: [NsmlAngleCell; 9] }`
  per `docs/container/dvd/application/mpucoder-pci_pkt.html`. Each
  `nsml_agl_cN_dsta` cell carries the relative sector offset to the
  current ILVU for that angle, with bit 31 as the direction
  (0 = forward, 1 = backward) and the `0x0000_0000` (angle absent) /
  `0x7FFF_FFFF` (no more video) sentinels. `NsmlAngleCell` exposes
  `is_absent` / `is_no_more_video` / `is_backward` / `offset_sectors`;
  `NsmlAgli` exposes `is_empty`, `active_angle_count`, and a 1-based
  `angle(n)` accessor that pairs with SPRM 3 (current angle). This is
  the PCI counterpart to the existing DSI `SmlAgli` seamless-angle
  table, completing the multi-angle navigation surface a player needs
  to switch angles on a non-seamless interleaved block.
- **First-Play PGC reader — `DvdDisc::parse_fp_pgc`.** The VMGI_MAT
  word at `0x0084` is the start *byte* address of `FP_PGC`, the
  program chain a player enters at disc insertion before any title or
  menu domain is active — per
  `docs/container/dvd/application/mpucoder-ifo.html` it is the only
  VMGI structure addressed in bytes rather than sectors (same unit as
  the `0x0080` "end byte address of VMGI_MAT" word), and its body is
  an ordinary PGC per `mpucoder-pgc.html` (the MAT row links straight
  to the PGC page), so `Pgc::parse` decodes it unchanged. The new
  helper reads the MAT, follows the byte address, and parses the PGC;
  it returns `Ok(None)` when `fp_pgc_addr` is zero (no First-Play PGC
  authored). The read is bounded at the first non-zero sector-aligned
  VMG table so a malformed address can't pull bytes from an unrelated
  table — an address at/past that boundary is rejected with an error
  rather than mis-parsed. This closes the navigation bootstrap gap:
  the Phase 3c VM could already *execute* startup routing
  (`JumpSs(FirstPlay)` / `JumpTT` actions) but nothing could *fetch*
  the FP_PGC those commands live in. Three new tests: the populated
  path drives the disc-insertion sequence end-to-end (synthetic
  cell-less FP_PGC at byte `0x0400` → `parse_fp_pgc` →
  `commands.pre` → `Vm::run_list` → `VmAction::JumpTitle { ttn: 1 }`),
  plus the zero-pointer `None` path and the past-first-table
  rejection. **311 lib tests** (was 308) under default features;
  **321 lib tests** (was 318) under `--all-features`.

- **Menu `C_ADT` + `VOBU_ADMAP` reader helpers on `DvdDisc`.** The
  VMGI / VTSI MATs carry sector pointers to the menu-side cell-address
  tables (`vmgm_c_adt_sector` / `vtsm_c_adt_sector`) and menu VOBU
  address maps (`vmgm_vobu_admap_sector` / `vtsm_vobu_admap_sector`),
  but no high-level reader followed them. The body decoders already
  existed — `docs/container/dvd/application/mpucoder-ifo.html` documents
  `VMGM_C_ADT` / `VTSM_C_ADT` / `VTS_C_ADT` under one shared `#c_adt`
  heading (and the three VOBU_ADMAP variants under `#vam`) because all
  share the wire format, so `VtsCAdt::parse` / `VobuAdmap::parse`
  decode the menu copies unchanged. This round wires the four
  high-level reader helpers that read the appropriate MAT, follow the
  sector pointer, and parse the body:
  - `DvdDisc::parse_vmgm_c_adt(reader)` — VMG menu cell-address table
    (`VIDEO_TS.VOB` cells).
  - `DvdDisc::parse_vmgm_vobu_admap(reader)` — VMG menu VOBU sector
    list.
  - `DvdDisc::parse_vtsm_c_adt(reader, ts_index)` — per-title-set menu
    cell-address table (`VTS_xx_0.VOB` cells).
  - `DvdDisc::parse_vtsm_vobu_admap(reader, ts_index)` — per-title-set
    menu VOBU sector list.
  Each returns `Ok(None)` when the corresponding MAT sector pointer is
  zero (no menu VOB authored). The reads are bounded at the next
  non-zero table sector in the MAT so a malformed `end_address` length
  field can't pull bytes from an unrelated table — the same
  bounded-read discipline the `parse_vmgm_pgci_ut` / `parse_vtsm_pgci_ut`
  helpers use. Five new in-module tests cover the populated happy path
  for all four helpers (synthetic VMGI/VTSI disc image → cell lookup +
  VOBU sector-count/start round-trip) and the four zero-pointer `None`
  paths. **308 lib tests** (was 303) under default features; **318 lib
  tests** (was 313) under `--all-features`.

- **`VMGM_PGCI_UT` + `VTSM_PGCI_UT` decoders (menu PGCI Unit Table).**
  The MAT records the sector pointers `vmgm_pgci_ut_sector` and
  `vtsm_pgci_ut_sector` for the menu PGC tables on both the VMG and
  VTS sides, but no body parser existed. This round materialises both
  per `docs/container/dvd/application/mpucoder-ifo_vmg.html` §VMGM_PGCI_UT
  and `mpucoder-ifo_vts.html` §VTSM_PGCI_UT — the wire format is
  identical between the two sides:
  - `PgciUt` — the outer search-pointer list keyed by ISO 639 language
    code (each entry: 16-bit language code + 1-byte language-code
    extension + 1-byte `menu_existence` flag + 32-bit offset to LU).
    The `language_unit(lang_code)` lookup round-trips a packed
    `b"en"`-style code to its parsed Language Unit; the per-entry
    `has_root_menu` / `has_subpicture_menu` / `has_audio_menu` /
    `has_angle_menu` / `has_ptt_menu` accessors decode each
    menu-existence flag bit per the table at `mpucoder-ifo_vts.html`
    (bit `0x80` = root/title, `0x40` = sub-picture, `0x20` = audio,
    `0x10` = angle, `0x08` = PTT — the constants live in the public
    `menu_existence` sub-module).
  - `PgciLu` — one Language Unit body: a per-PGC search-pointer list
    (`PgciLuSrp`: 32-bit PGC category dword + 32-bit offset to the
    PGC body) plus the parsed `Pgc` bodies themselves (via
    `Pgc::parse`). The `PgciLuSrp::is_entry_pgc` /
    `menu_type` / `parental_mask` accessors decompose the category
    dword per `mpucoder-ifo_vts.html` (PGC category breakdown).
  - `MenuType` enum — decodes the low nibble of the PGC category
    byte 0 (`2` = title / `3` = root / `4` = sub-picture / `5` =
    audio / `6` = angle / `7` = PTT, plus `Unknown(_)` for the
    reserved nibble values).
  - `DvdDisc::parse_vmgm_pgci_ut(reader)` /
    `parse_vtsm_pgci_ut(reader, ts_index)` — high-level reader helpers
    that read the appropriate MAT, follow the sector pointer, and
    parse the body. Both return `Ok(None)` when the corresponding
    MAT sector pointer is zero (table absent on this disc / title
    set). The reads are bounded at the next non-zero table sector
    so a malformed length field can't pull bytes from an unrelated
    table.

  Nine new unit tests cover the happy path (two-language walkthrough
  with entry-PGC + menu-type round-trip), the boundary cases (zero
  language units, parental-mask extraction from the category dword),
  and the four malformed-input rejection paths (short header /
  SRP list past buffer / LU offset zero / LU offset past buffer /
  inner PGC offset past buffer).

- **`VMG_VTS_ATRT` + `VMG_PTL_MAIT` decoders on the VMG side.** The
  VMG IFO's MAT carries two table pointers we'd previously parsed
  (`vts_atrt_sector`, `ptl_mait_sector`) without surfacing the
  table bodies. This round materialises both per
  `docs/container/dvd/application/mpucoder-ifo_vmg.html`:
  - `VmgVtsAtrt` — per-VTS attribute copies that mirror each VTS
    IFO's attribute block (the buffer at VTS IFO offset `0x0100`,
    typically `0x300` bytes long) onto the VMG side. Each
    `VmgVtsAtrtEntry` exposes the entry's `vts_category` field
    (`0` = unspecified, `1` = Karaoke), a 1-based `vts_number`,
    and the raw attribute blob. `entry(vts_number)` looks up an
    entry; bound checks reject malformed EAs that would overlap
    the next entry.
  - `VmgPtlMait` — the country-keyed parental management table.
    Each `PtlMait` body carries the eight parental-level mask
    arrays (`Nts + 1` 16-bit masks per level — index 0 is the
    VMG-side mask, `1..=nts` are the title sets). The on-disc
    storage order is descending (level 8 first), but the typed
    `masks` array is surfaced ascending (`masks[0]` = level 1) so
    a caller can index with `parental_level - 1` directly.
    `country(code)` looks up a country sub-table; `mask(level,
    title_set)` returns the 16-bit allow-mask for the
    `(parental_level, title_set)` pair.
  - `DvdDisc::parse_vmg_vts_atrt(reader)` /
    `parse_vmg_ptl_mait(reader)` — high-level reader helpers that
    read the MAT, follow the sector pointer, and parse the body.
    Both return `Ok(None)` when the corresponding MAT sector
    pointer is zero (table absent on this disc). The PTL_MAIT
    reader bounds its sector read at the next non-zero table
    pointer in the MAT so a malformed length field can't pull
    bytes from an unrelated table.
  Nine new tests cover the happy path (two-country / two-VTS
  walkthroughs with mask + blob round-trip), boundary cases (zero
  countries, partial header), and the four malformed-input
  rejection paths (short header / offset list past buffer /
  body offset past buffer / per-entry EA overlapping the next
  entry).

- **Typed accessors for the remaining language / sentinel SPRMs.**
  Round 3c's first SPRM accessor sweep covered the six bit-packed
  slots (SPRM 2 / 8 / 11 / 14 / 15 / 20); the rest of
  `docs/container/dvd/application/mpucoder-sprm.html` documents
  nine more SPRMs that aren't plain integers either — the four
  two-byte ASCII slots (SPRM 0 menu language, SPRM 12 parental
  country, SPRM 16 / 18 preferred audio / sub-picture language,
  ISO 639 / ISO 3166 alpha-2) and the sentinel-typed integer
  slots (SPRM 1 audio stream `0..=7` + `15`-none, SPRM 3 angle
  `1..=9`, SPRM 13 parental level `1..=8` + `15`-none, SPRM 17
  audio language extension five-value enum, SPRM 19 sub-picture
  language extension eleven-value enum). New surface on
  `RegisterFile`:
  - `menu_language()` / `parental_country()` /
    `preferred_audio_language()` /
    `preferred_subpicture_language()` return a `LanguageCode`
    that exposes the raw word, an `is_not_specified()` predicate
    (the `0xFFFF` SPRM 16 / 18 default), an `ascii_bytes()` →
    `Option<[u8; 2]>` accessor that only succeeds when both bytes
    are printable ASCII letters, and an `as_string()` lower-cased
    alpha-2 form for downstream tooling.
  - `audio_stream()` returns an `AudioStreamSelector` enum that
    distinguishes the `15`-none sentinel from real stream indices
    `Stream(0..=7)` and preserves out-of-range raws as `Invalid`.
  - `angle_number()` collapses the SPRM 3 word to
    `Option<u8>` with the `1..=9` range enforced.
  - `parental_level()` returns a `ParentalLevel` enum with
    `Level(1..=8)` / `None` (= 15) / `Invalid` shapes.
  - `preferred_audio_language_ext()` /
    `preferred_subpicture_language_ext()` return
    `AudioLanguageExt` / `SubpictureLanguageExt` enums covering
    every spec-table value; unmapped values collapse to
    `Reserved(raw)` for round-trip.
  Twelve new tests cover the defaults, the in-range values, and
  the out-of-range / sentinel collapse for each accessor.

- **Typed-instruction iterators on `PgcCommandTable`.** The PGC
  command table carries three lists of raw 8-byte
  [`NavCommand`] words (pre / post / cell) per
  `docs/container/dvd/application/mpucoder-pgc.html`; the Phase 3c
  disassembler in the `nav` module turns one word into a typed
  [`NavInstruction`]. Previously the bridge between the two was
  manual — callers had to walk `commands.pre` / `commands.post` /
  `commands.cell`, then call `nav::NavCommand::decode()` on each
  entry themselves. New surface:
  - `NavCommand::decode_instruction()` — convenience that
    delegates to the Phase 3c precursor disassembler so the IFO
    side can reach a typed instruction without re-importing the
    `nav` module's surface.
  - `PgcCommandTable::pre_instructions()` /
    `post_instructions()` / `cell_instructions()` — borrowing
    iterators of `NavInstruction` that walk each list in storage
    order.
  - `PgcCommandTable::cell_instruction(index_1based: u16)` —
    1-based indexed lookup matching the on-wire encoding
    `CellPlaybackInfo::cell_command` carries; passes `0` for
    "no cell command", out-of-range indices return `None` rather
    than panicking. Per `mpucoder-pgc.html` the cell-command
    table is 1-based, so 1 → `cell[0]`, 2 → `cell[1]`, etc.

  Round-trip checked: a `NavCommand` constructed by hand with a
  Type 1 jumpcall + `cmd_nibble = 1` payload decodes through both
  `decode()` and `decode_instruction()` to the same `Exit`
  variant. Four new unit tests in `src/ifo.rs` (synth command
  table → typed walk; 1-based indexing; `0` and out-of-range
  return `None`; round-trip with explicit `nav::decode()`).

- **`HighlightStatus` typed enum on PCI_GI `hli_ss`.** The PCI
  packet's `HLI_GI 00` field carries a 16-bit word whose lower two
  bits encode how a player should treat the menu-button overlay
  for the VOBU. Previously the field was surfaced only as the raw
  `u16` (`PciPacket::hli_ss`), forcing every consumer to repeat
  the `& 0b11` masking and four-way `match` documented in
  `docs/container/dvd/application/mpucoder-pci_pkt.html`.
  New typed surface:
  - `HighlightStatus` enum with four exhaustive variants —
    `None` (`00`), `AllNew` (`01`), `UsePrevious` (`10`),
    `UsePreviousExceptCommands` (`11`).
  - `HighlightStatus::from_hli_ss(u16)` infallible constructor
    that ignores the 14 reserved upper bits.
  - `HighlightStatus::to_bits()` round-trip back to the 2-bit
    code.
  - Four classifier predicates — `is_none()`,
    `declares_new_geometry()`, `reuses_previous_geometry()`,
    `supplies_own_commands()` — that match the four-row spec
    table directly so call sites no longer have to re-derive
    "AllNew + UsePreviousExceptCommands ⇒ commands come from
    this VOBU" from scratch.
  - `PciPacket::highlight_status()` accessor wrapping the
    constructor; the raw `hli_ss` word stays exposed so callers
    that need the reserved bits still have them.

  The `HighlightInfo` geometry struct is still populated only
  when the VOBU actually declares buttons (`btn_ns > 0`); the
  typed status accessor is now the documented way to detect a
  "re-use previous geometry" VOBU whose own `BTN_IT` is empty.

- **`DsiGi` cell-elapsed-time typed accessor.** The DSI_GI block
  on every Nav-Pack carries a 4-byte BCD `c_eltm` field describing
  the elapsed playback time inside the current cell, layered out
  identically to the `PGC_GI` playback-time field (`hh:mm:ss:ff`
  + 2-bit frame-rate code per `mpucoder-dsi_pkt.html`). Previously
  surfaced only as the raw `u32`. New methods:
  - `DsiGi::cell_elapsed_time() -> PgcTime` decodes the four BE
    bytes through the existing `PgcTime::from_bytes` decoder, so
    the same `hours / minutes / seconds / frames / frame_rate`
    fields the PGC playback-time accessor returns become available
    on the DSI side without the caller re-implementing the BCD
    nibble split.
  - `DsiGi::cell_elapsed_ns() -> u64` collapses the typed view to
    absolute nanoseconds via the new `PgcTime::to_nanoseconds`
    method below.
  - `DsiPacket::cell_elapsed_time()` / `cell_elapsed_ns()`
    convenience getters mirror the existing flat `vobu_ea()` /
    `vobu_vob_idn()` shape.

- **`PgcTime::to_nanoseconds()` method.** Previously the
  nanosecond conversion lived only inside the `mkv-output`
  feature gate as a free function on the MKV-writer (because the
  chapter timeline was the only consumer). Promoted to a regular
  method on `PgcTime` so default-feature builds get the rational
  `(frames × 1e9) / fps` conversion (30 fps → 33,333,333 ns/frame,
  25 fps → 40,000,000 ns/frame, illegal / reserved rates drop the
  frame fraction and keep only the integer-second portion). The
  `mkv_writer::pgc_time_to_ns` free function is preserved as a
  thin wrapper for callers that imported it directly.

- **VTSI_MAT / VMGI_MAT stream-attribute extension blocks.**
  The two MAT structures previously stopped at sector-pointer
  offset 0x00E4 — the audio / sub-picture / multichannel
  attribute extension that occupies 0x0100..0x015C (menu) and
  0x0200..0x03D8 (VTS title content + karaoke multichannel) was
  ignored. This round adds typed decoders for every field in
  those blocks and surfaces them on `VtsiMat::menu_attributes` /
  `VtsiMat::title_attributes` / `VmgIfo::menu_attributes`.
  Clean-room per `docs/container/dvd/application/mpucoder-ifo.html`
  (the `vidatt`, `audatt`, `spatt`, and `mcext` field layouts);
  no external implementation source consulted.
  - **`VideoAttributes`** — coding mode (MPEG-1 / MPEG-2),
    NTSC / PAL standard, 4:3 / 16:9 aspect, pan-scan and
    letterbox display-mode disallow flags, line-21 CC-field
    flags, and a `VideoResolution::dimensions(standard)` helper
    that resolves the 3-bit resolution code to absolute pixel
    dimensions (Full-D1 / ¾-D1 / Half-D1 / SIF).
  - **`AudioAttributes`** — coding mode (AC-3 / MPEG-1 / MPEG-2-
    ext / LPCM / DTS), language type + two-letter ISO-639 code +
    code-extension byte (per the SPRM-17 alternate-
    director-comment scheme), application mode (unspecified /
    karaoke / surround), channel count, sample-rate selector
    (only 48 kHz defined), and dual-interpretation
    quantization / DRC field (16/20/24 bps for LPCM versus
    DRC-on/off for MPEG). Helpers: `sample_rate_hz()`,
    `dolby_surround_suitable()`, and the four karaoke decoders
    (`karaoke_channel_assignment`, `karaoke_version`,
    `karaoke_mc_intro_present`, `karaoke_duet`) for the
    application-info byte at offset 7.
  - **`SubpictureAttributes`** — 2-bit-RLE coding mode (the only
    one defined), language type, ISO-639 code, and code-extension
    byte (per the SPRM-19 scheme).
  - **`McExtensionEntry`** — 24-entry karaoke multichannel
    extension table; each 8-byte entry decodes the 14 ACH
    guide-melody / guide-vocal / sound-effect flag bits across
    channels 0..=4.
  - **Backwards-compatible parse.** `VtsiMat::parse` still accepts
    a 0x200-byte buffer; the menu block fits within that range
    and is populated, the title block stays empty and the
    multichannel-extension vec stays empty. Real `VTS_xx_0.IFO`
    files run to 0x03D8 and now populate fully.

- **`VobuAdmap` + `VtsTmapti` / `VtsTmap` — time-based seek tables.**
  The two title-set sector pointers `VTSI_MAT::vts_vobu_admap_sector`
  and `VTSI_MAT::vts_tmapti_sector` previously surfaced only as raw
  `u32` fields; this round materialises both tables into typed
  parsers and wires them onto `VtsIfo` so a player can answer
  "where is playback at second N?" without re-walking the IFO byte
  buffer. Clean-room per
  `docs/container/dvd/application/mpucoder-ifo.html` (VOBU_ADMAP
  layout) and `docs/container/dvd/application/mpucoder-ifo_vts.html`
  (VTS_TMAPTI / VTS_TMAP layout); no external implementation
  source consulted.
  - **`VobuAdmap`** — `{ end_address, entries: Vec<u32> }` decoder
    for the per-VOBU sector list shared by `VMGM_VOBU_ADMAP`,
    `VTSM_VOBU_ADMAP`, and `VTS_VOBU_ADMAP` (all three share the
    same wire format per `mpucoder-ifo.html`). Entry count is
    implicit in the `end_address` field; the parser carves
    `(end_address + 1 - 4) / 4` four-byte VOB-relative sector
    words. `vobu_count`, `vobu_start_sector(vobu_number)` (1-based
    lookup), and `vobu_containing(sector)` (binary-partition
    inverse lookup that returns the 1-based VOBU number whose
    range covers the requested sector) round out the surface.
  - **`VtsTmap` + `TmapEntry`** — per-PGC time map. The 4-byte
    header is `{ time_unit: u8, reserved: u8, number_of_entries:
    u16 }`; each entry is a 4-byte big-endian word with bit 31 set
    when the previous entry was time-discontinuous (a VOBU
    boundary that crosses an STC reset) and the low 31 bits
    carrying the VOB-relative sector. `sector_at(seconds)`
    translates a PGC-relative wall-clock time into the VOBU
    sector whose `[(i - 1) * time_unit, i * time_unit)` bracket
    contains it; the result clamps to the last entry once
    `seconds` runs past the map. Empty maps and `time_unit == 0`
    both yield `None` rather than panic, per spec language that
    declares an empty map legal but unindexable.
    `TmapEntry::DISCONTINUITY_BIT` + `SECTOR_MASK` constants make
    the bit-31 split explicit.
  - **`VtsTmapti`** — `{ number_of_pgcs, end_address, maps:
    Vec<VtsTmap> }`. The spec mandates "each PGC MUST have a time
    map, even if it is empty" so `maps.len() ==
    number_of_program_chains` is invariant. `get(pgcn)` returns
    the per-PGC map for a 1-based program-chain number.
  - **Wired onto `VtsIfo::parse`** as the two new
    `Option<VobuAdmap>` + `Option<VtsTmapti>` fields
    (`vobu_admap`, `time_map`). Both stay `None` when the
    corresponding `VTSI_MAT` sector pointer is zero — the spec
    lists `VTS_VOBU_ADMAP` as mandatory but some authoring tools
    elide it on title sets that hold only menu VOBs, and
    `VTS_TMAPTI` is the explicitly-optional one. The new
    **`VtsIfo::vobu_sector_at_pgc_time(pgcn, seconds)`** wrapper
    composes `time_map.get(pgcn)` with `VtsTmap::sector_at`, the
    expected entry point a playback engine uses when the user
    requests a wall-clock seek; combine with
    `VtsiMat::title_vob_sector` for the absolute disc LBA.
  - 15 new in-module tests (round-trip + partition lookup +
    pre-sector / past-end edges + non-multiple-of-4 / truncated /
    empty-map rejections for `VobuAdmap`; entry decode +
    discontinuity-bit isolation + time-bracket sweep + empty +
    zero-`time_unit` / truncated rejections for `VtsTmap`;
    two-PGC walk + empty-PGC invariant + short-offset rejection
    for `VtsTmapti`; end-to-end VOBU-map + time-map composite
    that walks a six-sector synthetic IFO through `VtsIfo::parse`
    and asserts `vobu_sector_at_pgc_time` on three sample
    timestamps). **244 lib tests** (was 229) under default
    features; **254 lib tests** (was 239) under `--all-features`.

- **`uops` module — DVD-Video User Operation flag decoder.**
  Three on-disc fields carry a UOP-prohibition bitmask: the
  TT_SRPT entry (bits 0+1 packed into `title_type`), the PGC
  header (offset `0x0008`), and the PCI packet (`PCI_GI 08`). The
  new `uops` module surfaces them as typed values, clean-room per
  `docs/container/dvd/application/mpucoder-uops.html` (25-row bit
  table + per-level applicability columns + the "set bit in *any*
  mask inhibits the associated control" three-level OR-merge
  rule).
  - **`UserOp`** enum — 25 variants (`TimePlayOrSearch`,
    `PttPlayOrSearch`, `TitlePlay`, `Stop`, `GoUp`,
    `TimeOrPttSearch`, `TopPgOrPrevPgSearch`, `NextPgSearch`,
    `ForwardScan`, `BackwardScan`, the six `MenuCall*` variants,
    `Resume`, `ButtonSelectOrActivate`, `StillOff`, `PauseOn`,
    `AudioStreamChange`, `SubpictureStreamChange`, `AngleChange`,
    `KaraokeAudioMixChange`, `VideoPresentationModeChange`) with
    `bit()`, `mask()`, `from_bit()`, and `ALL` accessors.
  - **`UopMask`** — `u32` newtype with `contains` / `is_allowed`
    / `with` / `without` / `set` / `clear` / `is_empty` / `count`
    / `iter` accessors plus `merge_or(a, b, c)` for the three-
    level OR. `defined_bits()` masks the raw word to bits 0..=24
    so reserved bits don't pollute the comparison. `fits_level`
    validates that a mask carries only bits the spec table marks
    present at the given level — useful for an IFO sanity check.
  - **`UopLevel`** enum (`TitleSearchPointer` / `ProgramChain`
    / `Vobu`) with a `cover()` accessor reporting which bits the
    spec table's PGC and VOBU columns mark check-marked. PGC
    cover excludes bit 4 (`GoUp`) per the spec table's row 4
    PGC-column blank; VOBU cover excludes bits 0/1/2/17 per the
    same table.
  - **`title_type_uop_mask(title_type) -> UopMask`** — extracts
    the 2-bit TT_SRPT subset from a `DvdTitleEntry::title_type`
    byte (low two bits only; remaining bits are jump/link/call
    permission flags per `mpucoder-ifo_vmg.html` and stay out of
    the UOP surface).
  - **Typed accessors wired into existing parsers**:
    - `Pgc::uop_mask()` / `Pgc::is_user_op_allowed(UserOp)`
      around `Pgc::prohibited_user_ops`.
    - `PciPacket::uop_mask()` / `PciPacket::is_user_op_allowed`
      around `PciPacket::vobu_uop_ctl`.
    - `DvdTitleEntry::uop_mask()` /
      `DvdTitleEntry::is_user_op_allowed` around
      `DvdTitleEntry::title_type` (low 2 bits).
  - **Constants** — `UOP_TIME_PLAY_OR_SEARCH` through
    `UOP_VIDEO_PRESENTATION_MODE_CHANGE` (25 named bit-number
    constants), `UOP_BIT_COUNT = 25`, and `UOP_DEFINED_BITS =
    0x01FF_FFFF`.
  - 21 new in-module tests (bit-number / mask round-trip; spec-
    table column reproduction including the GoUp/PGC-blank row;
    title_type byte sweep; merge-or commutativity / associativity
    / identity; iter ordering; reserved-bit skip; `fits_level`
    cross-products) plus 7 cross-module integration tests in
    `tests/uops_integration.rs` validating the typed accessors
    against a hand-built `Pgc::parse` / `PciPacket::parse` / raw
    `DvdTitleEntry` plus the three-level merge end-to-end.
    **229 lib tests** (was 208) + 7 integration tests.

- **`lpcm` module — DVD-Video LPCM 7-byte audio-pack header decoder.**
  The `private_stream_1` LPCM substream (`0xA0..=0xA7`) carries a
  fixed 7-byte audio-pack header ahead of the raw PCM sample bytes
  that pins the sample format, the seamless-playback frame counter,
  and the X/Y dynamic-range coefficients. The new `lpcm` module
  decodes that header into a typed `LpcmHeader`, clean-room per
  `docs/container/dvd/application/mpucoder-lpcm.html`
  (field layout + `linear_gain = 2^(4 - (X + Y/30))` /
  `gain_db = 24.082 - 6.0206 X - 0.2007 Y` formulas) and
  `docs/container/dvd/application/stnsoft-LimPcmAud.html` (the
  per-`(sample_rate × quantisation × channels)` bitrate table and
  the 6144 kbps DVD-Video LPCM ceiling). Clean-room from those two
  spec pages only.
  - **`LpcmHeader`** — `{ sub_stream_id, number_of_frame_headers,
    first_access_unit_pointer, audio_emphasis_flag, audio_mute_flag,
    audio_frame_number, quantisation, sample_frequency, channel_count,
    dynamic_range_x, dynamic_range_y }` decoded view.
  - **`LpcmQuantisation`** enum — `Bits16` / `Bits20` / `Bits24` /
    `Reserved`, with `bits_per_sample()` accessor.
  - **`LpcmSampleFrequency`** enum — `Hz48000` / `Hz96000` /
    `Reserved`, with `hz()` accessor.
  - **`LpcmHeader::bitrate_kbps()`** computes `channels × sample_rate
    × bits_per_sample / 1000` and returns `None` when either of the
    two reserved codes is present;
    **`LpcmHeader::is_within_dvd_video_limit()`** checks the result
    against the `stnsoft-LimPcmAud.html` 6144 kbps ceiling (the red-
    highlighted combinations such as 96 kHz × 24-bit × 8-channel
    return `false`).
  - **`LpcmHeader::linear_gain()`** + **`gain_db()`** evaluate the
    two parameterisations of the dynamic-range coefficient table.
    `X = 0, Y = 0` gives the unity-gain reference `(2^4, +24.082 dB)`;
    `X = 7, Y = 30` gives the `-24 dB` pole. Applying the gain to the
    decoded samples stays with the audio decoder.
  - **`peel_lpcm_payload(&[u8]) -> Result<(LpcmHeader, &[u8])>`** —
    splits the substream-ID-prefixed PES payload into the typed
    header and the raw PCM tail in one zero-copy call.
  - **Constants** — `LPCM_HEADER_LEN = 7` and
    `DVD_LPCM_MAX_BITRATE_KBPS = 6144`.
  - 14 new unit tests including a full reproduction of the
    `stnsoft-LimPcmAud.html` bitrate table (48 combinations across
    `{48k, 96k} × {16, 20, 24} × {1..=8 ch}`, every cell pinning both
    the decoded kbps and the green/red `is_within_dvd_video_limit`
    verdict). Parse-reject cases for the truncated buffer and the
    non-LPCM substream selector; isolated decode of every quantisation,
    sample-rate, and channel-count code; bit-by-bit decoding of the
    emphasis / mute / frame-number byte, the first-access-unit
    pointer, and the X/Y dynamic-range split; the unity-gain identity
    and the `-24 dB` attenuation pole; and `peel_lpcm_payload` round-
    trip + short-buffer rejection. **208 lib tests** (was 192).
- **`mkv_writer` strips the LPCM audio-pack header** before forwarding
  PCM samples to the MKV muxer, so the `pcm_s16be` track now receives
  the clean big-endian sample bytes `A_PCM/INT/BIG` expects (the
  previous comment had punted the stripping to "Phase 3c"; this round
  closes that gap by re-using the new `lpcm::LPCM_HEADER_LEN`
  constant).

- **SPRM bitfield-aware accessors + named indices for SPRMs 0/12/14..20.**
  The `vm` module now exposes typed views for the six packed SPRMs
  whose contents are documented as bit-packed payloads on
  `mpucoder-sprm.html`:
  - `RegisterFile::subpicture_stream()` → `SubpictureStreamView` —
    decodes SPRM 2 into the 6-bit stream index, the bit-6
    `display` flag, plus `is_none_sentinel` / `is_forced_sentinel`
    helpers for the spec's `62` / `63` special values.
  - `RegisterFile::highlight_button()` → `u8` — decodes SPRM 8's
    `1..=36` button number from bits 10..=15; out-of-range fields
    surface as `0` so a malformed disc cannot crash a player.
  - `RegisterFile::audio_mix_mode()` → `AudioMixMode` — decodes
    SPRM 11's six per-channel mix bits (bits 2/3/4 → front,
    bits 10/11/12 → rear).
  - `RegisterFile::video_preference()` → `VideoPreference` with
    `AspectRatio` (4:3 / NotSpecified / Reserved / 16:9) and
    `DisplayMode` (Normal / PanScan / Letterbox / Reserved) decoded
    from SPRM 14 bits 10..=11 and 8..=9 respectively.
  - `RegisterFile::audio_capabilities()` → `AudioCapabilities` —
    decodes SPRM 15's nine documented capability bits (SDDS / DTS /
    MPEG / Dolby / PCM, each with optional karaoke variant);
    `cannot_play()` returns `true` when the register is zero per the
    spec page's "0 = cannot play" semantic.
  - `RegisterFile::region_allowed(region)` / `region_mask()` —
    decode SPRM 20's 8-bit region mask (bit `i` ⇒ region `i + 1`).
  Named index constants added for the missing SPRMs: `SPRM_MENU_LANG`
  (0), `SPRM_CC_PLT` (12), `SPRM_VIDEO_PREF` (14), `SPRM_AUDIO_CAPS`
  (15), `SPRM_PREF_AUDIO_LANG` (16), `SPRM_PREF_AUDIO_LANG_EXT` (17),
  `SPRM_PREF_SUBP_LANG` (18), `SPRM_PREF_SUBP_LANG_EXT` (19),
  `SPRM_REGION_MASK` (20). Default-vector documentation table
  re-rendered with one row per SPRM index, the spec value, and the
  spec-page source. SPRMs 17 and 19 now hold an explicit `0` ("not
  specified") rather than an implicit zero-fill, matching the spec's
  language-extension enumeration. Clean-room per
  `docs/container/dvd/application/mpucoder-sprm.html`. 14 new tests
  cover each accessor's default value and bit-by-bit decode.

- **Compound CMP/SET/LNK execution (Type 4..6) — Phase 3c completion.**
  The `nav` module's `SetCLnk` / `CSetCLnk` / `CmpSetLnk` variants now
  carry the full operand triple (SET source, CMP RHS, shared selector,
  Type 5's independent CMP-LHS, `hl_bn` button override, Link subset)
  pulled out of the 8-byte word per the per-row layouts on
  `mpucoder-vmi.html` (table 2, rows 88..101). The `vm` interpreter
  executes each compound in spec order per `mpucoder-vmi-sum.html`:
  - **`SetCLnk`** — SET first, then CMP against post-SET selector,
    then Link on `true`; `false` collapses to `Continue` so the
    outer command list keeps walking.
  - **`CSetCLnk`** — CMP first; SET and Link only on `true`.
  - **`CmpSetLnk`** — CMP first; SET only on `true`; Link
    **unconditional** (the distinguishing semantic from `CSetCLnk`).
  Compound Link subsets `Nop` collapse to `Continue` even when the
  enclosing compound ran; `Rsm` pops the same RSM stack as a bare
  Type-1 link; `Invalid(_)` subsets degrade to `Continue` so a
  malformed disc cannot crash the interpreter. The two "Illegal"
  red rows (SET-dir=1 AND CMP-dir=1 for Types 5 and 6, where the
  operand bytes would overlap) surface as `NavInstruction::Invalid`
  per the spec page's explicit rejection. 14 new tests (the four
  full-operand decode forms across register / immediate mixes for
  Types 4, 5, 6, the two Invalid-row encodings, plus 10 VM-exec
  cases covering SET-then-LINK truth + false-branch behaviour for
  all three families, the Link-subset `Nop` / `Rsm` /
  `Invalid` collapse paths, and the `SetOp::None` "skip SET phase"
  short-circuit). **177 lib tests** (was 163 after Phase 3c VM
  landed). Clean-room per the spec pages cited above; no external
  implementation source consulted.

- **`vm` module — DVD-Video VM interpreter (Phase 3c).** Wraps the
  `nav` module's typed `NavInstruction` disassembler with a stateful
  executor. Clean-room per `docs/container/dvd/application/mpucoder-{vmi,vmi-sum,vmi-jmp,sprm,uops}.html`;
  no external implementation source consulted.
  - **`RegisterFile`** — 16 GPRMs (writable, persists across PGCs)
    + 24 SPRMs initialised to the spec-defined defaults (`ASTN = 15`,
    `SPSTN = 62`, `AGLN/TTN/VTS_TTN/PTTN = 1`, `HL_BTNN = 1 << 10`,
    preferred-language slots = `0xFFFF`) + a per-GPRM counter-mode
    bit-mask the `SetGPRMMD mf` flag toggles. `tick_counters(delta)`
    advances every counter-mode GPRM by `delta` seconds (saturating)
    so a playback engine that owns a wall clock can drive the
    1 Hz semantic without owning the register file. Out-of-range
    index reads return `0` and writes are silently dropped — matches
    the spec's "invalid register reads as 0" fallback observed in
    malformed PGC command tables.
  - **`Vm`** — owns a `RegisterFile`, the call/return stack
    (`ResumePoint` frames bounded by `MAX_RSM_DEPTH = 8` to detect
    runaway nesting without restricting commercial-disc 1–2-deep
    Menu Call → sub-menu use cases), and the per-list program
    counter. `Vm::step(NavInstruction) -> VmAction` advances one
    decoded instruction. `Vm::run_list(&[NavCommand])` walks a
    pre/post/cell command list end-to-end, honours intra-list
    `Goto` (1-based line numbers per the spec page; out-of-range
    target falls through to the end of the list) + `Break` + `Exit`
    control flow, and terminates a pathological `Goto` self-loop
    via a `len * 16` step budget so a malformed disc can never hang
    the interpreter.
  - **`VmAction`** — the playback-engine-visible effect of one step:
    `Continue` / `Break` / `Exit` / `Link(LinkAction)` / `JumpTitle`
    / `JumpVtsTitle` / `JumpVtsPtt` / `JumpSs(JumpSSTarget)` /
    `CallSs(CallSSTarget)` / `Resume(ResumePoint)` / `SetNavTimer
    { seconds, pgcn }` / `NoOpRaw(NavCommand)`. The interpreter
    applies any register / counter / SPRM mutations the instruction
    implied before returning, so the engine sees the post-state.
  - **`LinkAction`** + **`ResumePoint`** — typed Link-family
    descriptors. `LinkAction::Subset { subset, hl_bn }` covers the
    13 enum-style forms (`LinkTopCell` … `LinkTailPGC`); the four
    numbered forms (`Pgcn`, `Pttn`, `Pgn`, `Cn`) each get a dedicated
    variant. `ResumePoint { resume_cell, hl_btn }` carries the
    CallSS `rsm_cell` byte through to a matching `RSM` so a player
    can resume to a different cell than the one active at call time.
  - **`Vm::evaluate`** + **`Vm::apply_set`** — pure helpers exposing
    the CMP and SET sub-op tables. `evaluate` covers all 7 named
    comparison predicates plus the `None` "unconditional" sentinel;
    `apply_set` covers all 12 named SET ops (`mov`, `swp`, `add`,
    `sub`, `mul`, `div`, `mod`, `rnd`, `and`, `or`, `xor`) using
    wrapping arithmetic for overflow, `checked_div` / `checked_rem`
    for the zero-divisor case (returns the destination unchanged
    rather than panic), and a deterministic `0` placeholder for
    `rnd` until a caller wraps the VM with an entropy source.
  - **`Vm::push_resume`** / **`Vm::pop_resume`** / **`Vm::resume_depth`**
    — public RSM stack manipulators for tests + tooling. Push is
    capacity-bounded at `MAX_RSM_DEPTH` (drops the new frame and
    returns `false` rather than overflow).
  - 12 SPRM index constants re-exported at crate root
    (`SPRM_AUDIO_STREAM`, `SPRM_SUBPICTURE_STREAM`, `SPRM_ANGLE`,
    `SPRM_TITLE`, `SPRM_VTS_TITLE`, `SPRM_PGCN`, `SPRM_PTT`,
    `SPRM_HL_BTNN`, `SPRM_NV_TIMER`, `SPRM_NV_PGCN`, `SPRM_AMXMD`,
    `SPRM_PARENTAL_LEVEL`) so callers don't carry magic numbers.
  - 37 new unit tests covering register-file defaults + out-of-range
    indexing + counter-mode toggle + tick saturation, the full CMP
    sub-op truth table, every named SET op including overflow-wrap
    + zero-divisor + `Invalid` no-op, `step()` dispatch for every
    `NavInstruction` family (Set arithmetic / Swap exchange /
    SetStn per-flag application / SetNvtmr action + SPRM 9-10 load /
    SetGprmMd counter-mode toggle / SetHlBtnn / SetTmpPml / every
    Link/Jump/Call surface / CallSs push + RSM pop with hl_btn
    propagation / RSM-with-empty-stack falls through to Continue /
    push bounded to MAX_RSM_DEPTH / Unknown + Invalid → NoOpRaw
    without mutation), and `run_list()` PC handling (clean
    Nop chain / Break-mid-list / Exit-mid-list / Goto 1-based
    addressing / out-of-range Goto falls off the end / runaway
    Goto-self-loop terminates under budget / PC resets between
    invocations / default `NavCommand` runs as single NOP).
    **163 lib tests** (was 126 after the SPU compositor landed).

### Changed

- Phase 3c precursor → Phase 3c proper: the `nav` module's
  `NavInstruction` disassembler is now consumed by the new `vm`
  module's interpreter, and Type 4..6 compounds carry their full
  operand triple instead of just the classifier sub-ops. Existing
  `NavInstruction` decode + the disc / IFO / VOB / SPU / MKV
  surfaces are unchanged.
- **Breaking** — `NavInstruction::{SetCLnk, CSetCLnk, CmpSetLnk}`
  field layout extended with the per-row operand fields documented
  in `mpucoder-vmi.html` table 2; the previous classifier-only
  shape (`set_op`, `cmp_op`, and Type 4's `scr` only) no longer
  compiles. Pre-0.0.3 release — no published consumer to break.
- Scrubbed an attributive external-implementation mention in
  `disc.rs`'s `DvdFileKind` doc comment and an enumerated-denial
  paragraph at the bottom of `README.md`; both are now spec-only
  wording per the project's clean-room provenance discipline.


## [0.0.2](https://github.com/OxideAV/oxideav-dvd/compare/v0.0.1...v0.0.2) - 2026-05-29

### Other

- composite SPU into RGBA overlay (palette + contrast + BT.601)
- add DVD Sub-Picture Unit decoder
- scrub enumerated-denial / decorative-attribution prose (r131 disclaimer-hygiene sweep follow-up)
- add Phase-3c-precursor NavInstruction disassembler
- decode NAV-pack DSI typed sub-sections (SML_PBI / SML_AGLI / SYNCI)
- decode NAV-pack PCI highlight (HLI_GI + SL_COLI + BTN_IT)
- re-export PaletteEntry / NavCommand / PgcCommandTable at crate root
- decode PGC palette colour-LUT + pre/post/cell command table
- Phase 3b: VOB → MKV mux glue + convert_dvd_to_mkv pipeline
- Phase 3a: VOB demuxer — MPEG-PS pack + nav-pack + DVD substream router
- Phase 2: IFO body parser — VMGI/VTSI MAT + TT_SRPT + VTS_PTT_SRPT + VTS_PGCI + VTS_C_ADT

### Added

- **SPU RGBA compositor** — `SubPictureUnit::composite(buf, palette)`
  turns a parsed sub-picture plus the PGC's 16-entry `PaletteEntry`
  colour-LUT into a finished `SpuBitmap` overlay (row-major
  `[R, G, B, A]` pixels + on-screen rectangle), completing the
  "final framebuffer left to the caller" gap entirely inside the
  crate. Clean-room per `docs/container/dvd/application/mpucoder-spu.html`
  (SET_COLOR/SET_CONTR semantics, `0x0` transparent … `0xF` opaque,
  top/bottom field interleave) + `stnsoft-color_pick.html` (BT.601
  studio-swing luma scale `Y = 16` 0 % … `Y = 235` 100 %). No
  libdvdread / libdvdnav / libdvdcss / FFmpeg / VLC / mpv / xine
  source consulted; no web search.
  - **`ycbcr_to_rgb`** — standalone BT.601 studio-swing
    `(Y, Cb, Cr) -> (R, G, B)` inverse-matrix conversion in fixed
    point (1.164 / 1.596 / 0.391 / 0.813 / 2.018 coefficients scaled
    by `1<<16`, round-half-up, clamped to `0..=255`).
  - **`SpuBitmap`** — `{ x, y, width, height, rgba }` overlay: the
    `SET_DAREA` rectangle plus the composited pixels, ready to blend
    onto the decoded MPEG-2 frame by the player.
  - The four 2-bit pixel codes are resolved through the unit's own
    `SET_COLOR` (→ `0..=15` palette index) and `SET_CONTR`
    (→ `0..=15` alpha, expanded to 8-bit by nibble replication);
    a unit lacking those uses well-defined fallbacks (background
    index, fully-opaque). Returns `None` when `SET_DAREA` /
    `SET_DSPXA` are absent (a malformed unit per the spec).
  - +5 unit tests (BT.601 known-point conversion incl. clamp +
    red/blue dominance, contrast-nibble expansion, full solid-rect
    composite round-trip, missing-`SET_DAREA` → `None`).
    **126 lib tests** (was 121 after the SPU decoder landed).

- **`spu` module** — DVD Sub-Picture Unit decoder, the overlay
  graphics stream that carries subtitles, menu button highlights,
  and karaoke captions. Pure-bytes decoder clean-room per
  `docs/container/dvd/application/mpucoder-spu.html` (sole 160-line
  source; no libdvdread / libdvdnav / libdvdcss / FFmpeg / VLC / mpv
  / xine / HandBrake source consulted; no web search).
  - **`SpuHeader`** — the 4-byte SPUH (`SPDSZ` total size +
    `SP_DCSQTA` offset to the Sub-Picture Display Control Sequence
    Table).
  - **`SpuCommand`** — typed enum for the eight SP_DCSQ command
    codes: `ForcedStartDisplay` (`0x00`) / `StartDisplay` (`0x01`)
    / `StopDisplay` (`0x02`) / `SetColor` (`0x03`, four 4-bit
    palette indices) / `SetContrast` (`0x04`, four 4-bit alpha
    values) / `SetDisplayArea` (`0x05`, four 12-bit coordinates)
    / `SetPixelDataAddresses` (`0x06`, top/bottom field offsets) /
    `ChangeColorContrast` (`0x07`, raw `LN_CTLI` / `PX_CTLI`
    parameter blob preserved for the caller) / `EndOfSequence`
    (`0xFF`, the `CMD_END` terminator).
  - **`SpDcSq`** — one display-control sequence: a 4-byte header
    (90 kHz/1024 `start_time` + `next_offset` chain pointer) plus
    the decoded command list. Chain-walk validates per-block
    forward progress and rejects loops.
  - **`SubPictureUnit::parse`** — top-level entry that walks SPUH
    + every chained DCSQ from the `SP_DCSQTA` offset until a
    terminal block (whose `next_offset` points back at itself).
    Convenience accessors `pixel_data_offsets()` / `display_dimensions()`
    pull the PXDtf/PXDbf offsets and rectangle width/height out of
    the command stream.
  - **`decode_rle_field`** — the 2-bit / four-form PXD run-length
    decoder. Implements the nested-prefix encoding (`n n c c` /
    `0 0 n n n n c c` / `0 0 0 0 n n n n n n c c` /
    `0 0 0 0 0 0 n n n n n n n n c c`) and the 16-bit
    "count=0 = until end of line" terminator, with byte alignment
    at every row boundary per mpucoder-spu.html §PXDtf.
  - **`render_field`** — flattens the run vector into a row-major
    `Vec<u8>` of palette indices (`0..=3`), one byte per pixel,
    ready for blending against the PGC's 16-entry `PaletteEntry`
    table.
  - **`spdcsq_stm_to_ms`** — converts an `SP_DCSQ_STM` 90 kHz/1024
    delay to integer milliseconds via the inverse of the
    mpucoder-spu.html conversion table.

  Producing a final framebuffer (YCrCb + alpha) is intentionally
  left to the caller — that step needs the PGC `PaletteEntry`
  table (already exposed by `crate::ifo`) plus the renderer's
  preferred pixel format, both outside the SPU bitstream itself.

  +13 unit tests (header parse / delay conversion / one-run RLE
  for all four forms / end-of-line marker / EOL row padding /
  full-unit round-trip with six commands / `CHG_COLCON` raw
  round-trip / DCSQTA-out-of-range rejection / runaway-DCSQ
  rejection / opcode table). **132 tests total** (was 119).

- **`nav` module** — typed VM instruction decoder (Phase 3c precursor).
  The previous `NavCommand` surface exposed only an 8-byte raw word
  plus the 3-bit `command_type` classifier; the new `NavCommand::decode()
  -> NavInstruction` returns a typed-enum disassembly tree clean-room
  per `docs/container/dvd/application/mpucoder-vmi.html` +
  `mpucoder-vmi-sum.html` + `mpucoder-vmi-jmp.html` +
  `mpucoder-sprm.html` (no libdvdread / libdvdnav / libdvdcss /
  FFmpeg / VLC / mpv / xine / HandBrake source consulted; no web
  search). **No execution** — an interpreter that owns
  SPRMs / GPRMs / PC / RSM stack is the bulk of Phase 3c proper;
  decoding the stream is the prerequisite step shared by a future
  executor, an analyser, and a disc debugger.
  - **`Register`** — 8-bit operand classifier: `Gprm(0..=15)` /
    `Sprm(0..=23)` / `Invalid(raw)` per the asterisk note on the VMI
    spec page (only `0x00..=0x0F` and `0x80..=0x97` are valid).
  - **`SetOp`** + **`CmpOp`** — the SET (12 named codes: `mov`,
    `swp`, `add`, `sub`, `mul`, `div`, `mod`, `rnd`, `and`, `or`,
    `xor`) and CMP (7 named codes: `BC`, `EQ`, `NE`, `GE`, `GT`,
    `LE`, `LT`) sub-op tables from the same page.
  - **`LinkSubset`** — the 13-entry inner table for the `Type-1 0x20
    0x01` Link command: `LinkTopCell` / `LinkNextCell` /
    `LinkPrevCell` / `LinkTopPG` / `LinkNextPG` / `LinkPrevPG` /
    `LinkTopPGC` / `LinkNextPGC` / `LinkPrevPGC` / `LinkGoupPGC` /
    `LinkTailPGC` / `Rsm` + `Nop`, with the spec's invalid bag
    (`0x04, 0x08, 0x0E, 0x0F, 0x11..0x1F`) preserved via
    `Invalid(raw)`.
  - **`JumpSSTarget`** + **`CallSSTarget`** — the four-way
    destination selector (`FirstPlay` / `VmgmMenu { menu }` /
    `VtsmMenu { vts, ttn, menu }` / `VmgmPgcn { pgcn }`) from the
    `JumpSS` / `CallSS` rows in `mpucoder-vmi.html`. `CallSSTarget`
    additionally carries the `rsm_cell` resume-cell byte shared by
    all four CallSS variants.
  - **`NavInstruction`** — top-level decode enum. Variants for the
    well-defined opcodes: `Nop`, `Goto { line }`, `Break`,
    `SetTmpPml { level, line }`, `LinkSub { subset, hl_bn }`,
    `LinkPgcn { pgcn }`, `LinkPttn { pttn, hl_bn }`,
    `LinkPgn { pgn, hl_bn }`, `LinkCn { cn, hl_bn }`, `Exit`,
    `JumpTT { ttn }`, `JumpVtsTt { ttn }`,
    `JumpVtsPtt { ttn, pttn }`, `JumpSs(JumpSSTarget)`,
    `CallSs(CallSSTarget)`, `SetStn` (with `af`/`sf`/`nf` flag
    bits and per-channel register-or-immediate source), `SetNvtmr`,
    `SetGprmMd` (with `counter` mode bit), `SetAmxMd`, `SetHlBtnn`,
    `Set { op, dst, src }`. Compound Type 4..6 forms surface their
    classifier `SetOp` + `CmpOp` sub-ops via `SetCLnk` (Type 4),
    `CSetCLnk` (Type 5), `CmpSetLnk` (Type 6); the per-operand
    sub-decode is deferred to the executor. Type 7 returns
    `Unknown` (the VMI page documents the family has never been
    observed in real-world streams); structurally-impossible
    encodings return `Invalid`.
  - 42 new unit tests covering the `Register` GPRM / SPRM /
    invalid-hole classifier, the full `SetOp` / `CmpOp` /
    `LinkSubset` named-code tables, the spec's named-but-invalid
    sub-codes (`SetSystem` sub=5, `Set` sub=0/C/F, Type 0 cmd
    nibble 4..F, Link cmd nibble 2), the round-trip from a
    `NavCommand::default()` (all zero) decoding to `Nop`, and one
    decoded form per `NavInstruction` variant including the
    JumpSS four-way target selector and the CallSS rsm_cell field.

- NAV-pack DSI **typed sub-section decode** (Data Search Information):
  the `DsiPacket` decoder previously surfaced only the DSI_GI preamble
  and a flat 43-entry VOBU_SRI array; it now returns a typed
  `DsiPacket { general_info, sml_pbi, sml_agli, vobu_sri, synci }`
  with every spec-listed field exposed by name, clean-room per
  `mpucoder-dsi_pkt.html` (no libdvdread / libdvdnav / FFmpeg / VLC /
  mpv / xine source consulted).
  - **`DsiGi`** — DSI_GI general information (packet 0x00..0x20):
    `nv_pck_scr`, `nv_pck_lbn`, `vobu_ea`, the 1st/2nd/3rd reference-
    frame end-address triplet, the `(vobu_vob_idn, vobu_c_idn)`
    identifier pair, and the BCD `c_eltm` cell-elapsed-time + frame-
    rate bits field. Convenience getters
    (`DsiPacket::nv_pck_scr()` etc.) mirror the pre-refactor flat-field
    accessors so the bump stays source-compatible for call-sites that
    only read DSI_GI.
  - **`SmlPbi` + `SmlAudioGap`** — SML_PBI seamless-playback info
    (packet 0x20..0xB4, 148 bytes): the 16-bit `ilvu` flag word with
    `preu()` / `is_ilvu()` / `unit_start()` / `unit_end()` bit
    decoders, the `(ilvu_ea, nxt_ilvu_sa, nxt_ilvu_sz)` interleaved-
    block jump pointers, the VOB-span video PTM pair, and the 8 ×
    16-byte per-audio-stream gap table (`stp_ptm1`, `stp_ptm2`,
    `gap_len1`, `gap_len2` per stream).
  - **`SmlAgli` + `SmlAngleCell`** — SML_AGLI seamless-angle info
    (packet 0xB4..0xEA, 54 bytes): 9 angle cells, each 6 bytes wide
    (`dsta: u32` with bit-31 direction flag + sentinel values for
    "absent" and "no more video"; `sz: u16` ILVU size in sectors).
  - **`VobuSri`** — VOBU search-information table (packet 0xEA..0x192,
    168 bytes = 42 × 4): `sri_nvwv` (next-VOBU-with-video), 19 forward
    scaled-distance entries, `sri_nv` + `sri_pv` brackets, 19 backward
    entries, `sri_pvwv` (previous-VOBU-with-video). The bit-31
    `VALID_BIT`, bit-30 `INTERMEDIATE_BIT`, and 30-bit `OFFSET_MASK`
    constants make sentinel handling explicit. (Previous flat-array
    decode over-read by 4 bytes into SYNCI; the typed layout fixes
    that.)
  - **`Synci`** — SYNCI A/V-sync pointer table (packet 0x192..0x222,
    144 bytes): `a_synca: [u16; 8]` audio + `sp_synca: [u32; 32]`
    subpicture per-stream first-packet offsets. `AUDIO_DIRECTION_BIT`
    (bit 15) and `SP_DIRECTION_BIT` (bit 31) constants surface the
    spec-defined direction flag.
  - 9 new unit tests (`dsi_section_offsets_match_spec`,
    `dsi_parses_general_info_block`,
    `dsi_parses_sml_pbi_block_and_ilvu_flags`,
    `dsi_pbi_ilvu_flag_decoders_isolate_bits`,
    `dsi_parses_sml_agli_block`,
    `dsi_parses_vobu_sri_block_and_brackets`,
    `dsi_parses_synci_block`, `dsi_rejects_short_buffer`,
    `dsi_nav_pack_round_trip_through_full_sector`) and a new
    `build_dsi_body` helper that emits a fully-populated 546-byte DSI
    body so every per-section offset is pinned exactly.

### Changed

- **Breaking** — `DsiPacket`'s public field layout. The previous flat
  `{ nv_pck_scr, nv_pck_lbn, vobu_ea, vobu_1stref_ea, vobu_2ndref_ea,
  vobu_3rdref_ea, vobu_vob_idn, vobu_c_idn, c_eltm, vobu_sri: Box<[u32;
  43]> }` shape was replaced by the typed sub-section struct described
  above. Source-compatible getters (`nv_pck_scr()` etc.) are provided
  for the DSI_GI fields; the `vobu_sri` field is now a `VobuSri` struct
  rather than a flat boxed array. Pre-0.0.2 release — no published
  consumer to break.

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
