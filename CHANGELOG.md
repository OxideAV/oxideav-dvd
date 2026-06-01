# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.3](https://github.com/OxideAV/oxideav-dvd/compare/v0.0.2...v0.0.3) - 2026-06-01

### Other

- execute Type 4..6 compound CMP/SET/LNK families (Phase 3c completion)
- Phase 3c interpreter — SPRM/GPRM register file + Link/Jump/Call execution

### Added

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
