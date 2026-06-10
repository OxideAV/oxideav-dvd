# oxideav-dvd

[![Crates.io](https://img.shields.io/crates/v/oxideav-dvd.svg)](https://crates.io/crates/oxideav-dvd)
[![Docs.rs](https://docs.rs/oxideav-dvd/badge.svg)](https://docs.rs/oxideav-dvd)
[![CI](https://github.com/OxideAV/oxideav-dvd/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-dvd/actions/workflows/ci.yml)

Read-only **DVD-Video** disc reader — ISO 9660 + UDF 1.02 mount +
`VIDEO_TS/` directory walk + IFO body parser (VMGI / VTSI / PGC /
TT_SRPT / VTS_PTT_SRPT / VTS_C_ADT) + VOB demuxer (MPEG-PS pack
+ nav-pack + DVD substream router) + optional **VOB → Matroska
mux** with `ChapterTimeStart` / `ChapterTimeEnd` carried over from
the PGC playback-time fields. Clean-room per ECMA-267 / ECMA-268 +
OSTA UDF 1.02 + the ECMA-167 UDF base standard + RFC 9559 §5.1.7 +
mpucoder + stnsoft community RE references.

## Scope

Phases 1, 2, and 3a (this release) handle the **physical +
filesystem + disc-identification + IFO structural + VOB demux**
layers — enough to point a player at a DVD-Video disc image or
block device, enumerate the title-set files, pull the title /
chapter / program-chain / cell layout out of every IFO, demux
each cell's VOBUs into raw MPEG-2 video + AC-3 / DTS / LPCM audio
+ subpicture elementary streams keyed by track ID, and answer
time-based seek queries through the per-PGC `VTS_TMAPTI` time
map + the title-set `VTS_VOBU_ADMAP` absolute-sector list. The Phase 3a
nav-pack decoder also surfaces the typed **DSI** sub-sections
(`DSI_GI` general info; `SML_PBI` seamless-playback interleaved-unit
flags + jump pointers + per-stream audio-gap table; `SML_AGLI` 9-cell
seamless-angle table; `VOBU_SRI` 19-forward + 19-backward seek
pointers + bracket pointers; `SYNCI` per-audio + per-subpicture
sync pointers) that chapter-accurate seek and A/V sync need, plus the
PCI **highlight information** (HLI_GI timing + the three SL_COLI
selection/action colour-contrast schemes + the per-button BTN_IT
table — geometry, D-pad adjacency and the raw action command) that a
menu renderer needs to draw and route button input. A new
**`nav` module** (Phase 3c precursor) decodes each 8-byte
`NavCommand` word into a typed `NavInstruction` tree —
NOP / Goto / Break / SetTmpPML, the full `Link*` family (with
the 13-entry link-subset table), `Exit` / `JumpTT` /
`JumpVTS_TT` / `JumpVTS_PTT`, the four-way `JumpSS` /
`CallSS` target selector, the `SetSystem` family
(`SetSTN` / `SetNVTMR` / `SetGPRMMD` with counter-mode bit /
`SetAMXMD` / `SetHL_BTNN`), the plain `Set` arithmetic family
(12 SET sub-ops × GPRM-or-SPRM source), and classifier sub-ops
for the compound Type 4..6 CMP/SET/LNK encodings. The Phase 3c
**`vm` module** wraps the decoder with a register file
(16 GPRMs + 24 SPRMs with spec-defined defaults + per-GPRM
counter-mode bits), an RSM call/return stack, intra-list PC
handling (`Goto` / `Break` / runaway-loop bound), and a
`Vm::step(NavInstruction) -> VmAction` interpreter that surfaces
`Link` / `JumpTitle` / `JumpVtsTitle` / `JumpVtsPtt` / `JumpSs` /
`CallSs` / `Resume` / `SetNavTimer` / `Exit` actions to the
playback engine. **No CSS yet** — Phase 3c via the external
`oxideav-css` crate.

| Layer | Status |
|-------|--------|
| ISO 9660 PVD + path-table + directory walk | landed |
| UDF 1.02 mount (PVD / PD / LVD / FSD / FE / FID) | landed |
| VIDEO_TS file enumeration (VIDEO_TS.IFO / .VOB / .BUP + VTS_xx) | landed |
| `dvd://` source driver (registry feature) | landed |
| VMGI / VTSI MAT parse (header + sector pointers) | landed |
| VTSI_MAT / VMGI_MAT stream-attribute extension (video / audio × 8 / subpicture × 32 / karaoke MC extension) | landed |
| VMG_VTS_ATRT (per-VTS attribute copies — entry header + VTS_CAT + raw attribute blob) | landed |
| VMG_PTL_MAIT (country-keyed parental management — 8 levels × (Nts + 1) 16-bit allow-masks per country) | landed |
| VMGM_PGCI_UT / VTSM_PGCI_UT (menu PGCI Unit Table — outer ISO 639 language-unit search-pointer list + inner per-LU PGC search-pointer list + PGC bodies + menu-existence flag decoder) | landed |
| TT_SRPT (title list) + VTS_PTT_SRPT (chapter list) | landed |
| VTS_PGCI + PGC (program chains + cells + colour-LUT + command table) | landed |
| VTS_C_ADT (cell-to-VOB-sector lookup) | landed |
| VMGM_C_ADT / VTSM_C_ADT + VMGM_VOBU_ADMAP / VTSM_VOBU_ADMAP (menu cell-address tables + menu VOBU sector maps via `DvdDisc::parse_vmgm_c_adt` / `parse_vtsm_c_adt` / `parse_vmgm_vobu_admap` / `parse_vtsm_vobu_admap`) | landed |
| VTS_VOBU_ADMAP (per-VOBU sector list + partition lookup) | landed |
| VTS_TMAPTI (per-PGC time map + seconds → VOBU sector seek) | landed |
| VOB demux (MPEG-PS pack + nav-pack + PES) | landed (Phase 3a) |
| DVD substream routing (AC-3 / DTS / LPCM / subpicture) | landed (Phase 3a) |
| LPCM 7-byte audio-pack header decode (quantisation / sample rate / channels / dynamic range) | landed (Phase 3a) |
| User Operation flag decoder (TT_SRPT / PGC / PCI-VOBU three-level OR-merged `UopMask`) | landed (Phase 3c support) |
| VOBU_SRI search-table decode | landed (Phase 3a) |
| NAV-pack PCI highlight (HLI_GI + SL_COLI + BTN_IT buttons) | landed (Phase 3a) |
| PCI_GI `hli_ss` → typed `HighlightStatus` enum (None / AllNew / UsePrevious / UsePreviousExceptCommands) + geometry-inheritance + own-commands classifiers | landed (Phase 3a) |
| NAV-pack DSI typed sub-sections (DSI_GI + SML_PBI + SML_AGLI + VOBU_SRI + SYNCI; DSI_GI `c_eltm` → typed `PgcTime` + ns) | landed (Phase 3a) |
| MKV mux + chapter encoding wiring | landed (Phase 3b, `mkv-output` feature) |
| VM instruction **decode** (typed `NavInstruction` disassembler — non-executing) | landed (Phase 3c precursor) |
| `PgcCommandTable` typed-instruction iterators (`pre_instructions` / `post_instructions` / `cell_instructions` + 1-based `cell_instruction(index)`) | landed (Phase 3c bridge) |
| Sub-Picture Unit (SPU) decode (SPUH + SP_DCSQT command stream + PXDtf/PXDbf 2-bit RLE) | landed |
| SPU → RGBA compositor (palette + contrast resolve + BT.601 YCbCr→RGB + field interleave) | landed |
| VM **execution** (interpreter over SPRMs/GPRMs + RSM stack + PC) | landed (Phase 3c — Type 0..6, including compound SET+CMP+LINK) |
| Typed SPRM accessors — language slots + sentinel-typed integer slots (SPRM 0 / 1 / 3 / 12 / 13 / 16 / 17 / 18 / 19) | landed |
| CSS authentication + descrambling | Phase 3c (external `oxideav-css` crate) |

## Quick start

```rust,no_run
use oxideav_dvd::DvdDisc;

// Open a DVD-Video ISO or block device.
let disc = DvdDisc::open("path/to/disc.iso").unwrap();

println!("volume_id = {}", disc.volume_id);
println!("title_set_count = {}", disc.title_set_count);
for f in &disc.video_ts_files {
    println!("  {:?}  lba={}  size={}", f.kind, f.lba, f.size);
}
```

## Standalone build

`oxideav-core` is gated behind the default-on `registry` feature.
Drop the framework dependency entirely with:

```toml
oxideav-dvd = { version = "0.0", default-features = false }
```

The `DvdDisc`, `iso9660::*`, and `udf::*` parser surface stays
available; only the `dvd://` source-registry plumbing disappears.

## DVD → MKV (Phase 3b)

Enable the **`mkv-output`** feature to convert a DVD title to a
Matroska file:

```toml
oxideav-dvd = { version = "0.0", features = ["mkv-output"] }
```

```rust,no_run
oxideav_dvd::convert_dvd_to_mkv(
    "dvd:///mnt/disc.iso",  // or a bare /path/to/disc.iso
    1,                       // title number (1-based)
    "/tmp/title-01.mkv",
).unwrap();
```

The muxer preserves each PES packet's 90 kHz PTS, sizes the MKV
`Tracks` element to the streams the title actually carries
(video + AC-3 / DTS / LPCM / subpicture), and emits one MKV
`ChapterAtom` per `DvdChapter` with `ChapterTimeStart` /
`ChapterTimeEnd` computed from the PGC playback-time BCD field
(30 fps for NTSC, 25 fps for PAL — per `mpucoder-pgc.html`).

The feature is **default-off** so the parse-only surface above
keeps compiling against any `oxideav-mkv` patch release on
crates.io. Toggle it on once you've got `oxideav-mkv >= 0.0.8`
(the release that landed `MkvMuxer::add_chapter`).

## Decoding a Sub-Picture Unit

The `spu` module turns the raw bytes of a DVD subtitle / menu
overlay into a typed control sequence plus run-length-decoded
pixel data, without rendering. The PGC palette + final
framebuffer step stays with the caller:

```rust,no_run
use oxideav_dvd::{SubPictureUnit, SpuCommand, decode_rle_field, render_field};

// `spu_bytes` is the concatenation of every subpicture PES packet
// payload for one subpicture stream over one display interval.
let spu_bytes: &[u8] = &[];
let unit = SubPictureUnit::parse(spu_bytes).unwrap();

for dcsq in &unit.control_sequences {
    println!("delay = {} ms", oxideav_dvd::spdcsq_stm_to_ms(dcsq.start_time));
    for cmd in &dcsq.commands {
        if let SpuCommand::SetColor { emphasis2, emphasis1, pattern, background } = cmd {
            println!("palette = {:x}/{:x}/{:x}/{:x}",
                     background, pattern, emphasis1, emphasis2);
        }
    }
}

// Materialise the top field's 2-bit palette indices given the
// known display width / line count.
if let (Some((top_off, _)), Some((w, h))) = (unit.pixel_data_offsets(), unit.display_dimensions()) {
    let lines = (h + 1) / 2;
    let pixels = render_field(&spu_bytes[top_off as usize..], w, lines).unwrap();
    println!("decoded {} top-field palette-index pixels", pixels.len());
}
```

The decoder handles the four PXD run-length forms (`n n c c` /
`0 0 n n n n c c` / `0 0 0 0 n n n n n n c c` /
`0 0 0 0 0 0 n n n n n n n n c c`), the 16-bit-form "count=0 =
until end of line" terminator, and the per-row byte alignment
required by `mpucoder-spu.html` §PXDtf.

To go all the way to a finished overlay, pass the parsed unit and
the PGC's 16-entry palette to `SubPictureUnit::composite`:

```rust,no_run
use oxideav_dvd::SubPictureUnit;
use oxideav_dvd::ifo::PaletteEntry;

let spu_bytes: &[u8] = &[];
let palette: [PaletteEntry; 16] = [PaletteEntry::default(); 16];

let unit = SubPictureUnit::parse(spu_bytes).unwrap();
if let Some(bmp) = unit.composite(spu_bytes, &palette).unwrap() {
    // bmp.rgba is width*height*4 bytes of [R, G, B, A], to be blended
    // onto the decoded MPEG-2 frame at (bmp.x, bmp.y).
    println!("overlay {}x{} at ({},{})", bmp.width, bmp.height, bmp.x, bmp.y);
}
```

`composite` resolves the four 2-bit pixel codes through the unit's
own `SET_COLOR` (→ `0..=15` palette index) and `SET_CONTR`
(→ `0..=15` alpha), converts the palette's BT.601 studio-swing
YCbCr to RGB (`ycbcr_to_rgb`, luma scale `Y = 16` 0 % … `Y = 235`
100 % per `stnsoft-color_pick.html`), and interleaves the
top-field (lines 1, 3, 5, …) and bottom-field pixel data into one
row-major `SpuBitmap`. Positioning/scaling the overlay onto the
video frame stays with the player.

## Decoding an LPCM audio-pack header

The `lpcm` module pulls the 7-byte audio-pack header off the start
of a `private_stream_1` LPCM PES payload (substream `0xA0..=0xA7`)
and surfaces the sample format the raw PCM bytes were encoded in:

```rust,no_run
use oxideav_dvd::{LpcmHeader, LpcmQuantisation, LpcmSampleFrequency, peel_lpcm_payload};

// `lpcm_payload` starts at the substream-ID byte (`0xA0..=0xA7`) —
// the same shape `VobStreams::lpcm` would carry if the demuxer
// preserved the substream selector ahead of the body.
let lpcm_payload: &[u8] = &[
    0xA0, // sub_stream_id (track 0)
    0x01, // number_of_frame_headers
    0x00, 0x14, // first_access_unit_pointer = 20
    0x00, // emphasis=0 mute=0 frame=0
    0x01, // q=16-bit, sr=48 kHz, 2-channel
    0x00, // dynamic_range X=0, Y=0
    /* … raw big-endian PCM samples … */
];

let (h, samples) = peel_lpcm_payload(lpcm_payload).unwrap();
assert_eq!(h.track(), 0);
assert_eq!(h.quantisation, LpcmQuantisation::Bits16);
assert_eq!(h.sample_frequency, LpcmSampleFrequency::Hz48000);
assert_eq!(h.channel_count, 2);
assert_eq!(h.bitrate_kbps(), Some(1_536));
assert!(h.is_within_dvd_video_limit());
// `samples` is the big-endian PCM tail, ready for sample unpacking.
let _ = samples;
```

`LpcmHeader::bitrate_kbps()` returns the `channels × sample_rate ×
bits_per_sample / 1000` rate; `is_within_dvd_video_limit()` checks
the result against the 6144 kbps DVD-Video ceiling per
`stnsoft-LimPcmAud.html` (the red-highlighted combinations like
96 kHz × 24-bit × 8-channel return `false`). `linear_gain()` /
`gain_db()` evaluate the two parameterisations of the X/Y
dynamic-range coefficient on `mpucoder-lpcm.html`
(`2^(4 - (X + Y/30))` and `24.082 - 6.0206 X - 0.2007 Y`); applying
the gain to the decoded samples stays with the audio decoder.

## Querying User Operation prohibitions

The `uops` module surfaces the 25-entry user-operation table per
`mpucoder-uops.html` plus the spec's three-level OR-merge rule.
Three on-disc fields carry a UOP-prohibition mask — the TT_SRPT
entry (bits 0+1 packed in `title_type`), the PGC header (offset
`0x0008`), and the PCI packet (`PCI_GI 08`) — and a set bit in
*any* of them inhibits the associated control.

```rust
use oxideav_dvd::ifo::{DvdTitleEntry, Pgc};
use oxideav_dvd::uops::{UopMask, UserOp};
use oxideav_dvd::vob::PciPacket;

fn user_op_allowed(
    title: &DvdTitleEntry,
    pgc: &Pgc,
    pci: &PciPacket,
    op: UserOp,
) -> bool {
    let merged = UopMask::merge_or(
        title.uop_mask(),
        pgc.uop_mask(),
        pci.uop_mask(),
    );
    merged.is_allowed(op)
}

// `Pgc::uop_mask().iter()` walks the currently-prohibited ops in
// ascending bit order; reserved bits above bit 24 are skipped.
fn report(pgc: &Pgc) {
    for op in pgc.uop_mask().iter() {
        println!("PGC blocks {:?}", op);
    }
}
```

Each accessor wraps a `u32` newtype; `UopMask::from_bits` /
`raw()` round-trip the on-disc value exactly. `fits_level(level)`
validates that a mask carries only the bits the spec table marks
present at that level — useful for an IFO sanity-checker.



## Reading the DSI cell-elapsed time

Every Nav-Pack carries a DSI_GI block whose 4-byte `c_eltm` field
is a BCD `hh:mm:ss:ff` cell-elapsed timestamp plus a 2-bit frame-
rate code (`11` = 30 fps, `01` = 25 fps; `00` / `10` are spec-
illegal). The typed accessor decodes the field through the same
`PgcTime` shape used by the PGC playback-time fields:

```rust,no_run
use oxideav_dvd::{NavPack, ifo::FrameRate};

let sector: &[u8] = &[]; // 2048 bytes covering one Nav-Pack
let nav = NavPack::parse(sector).unwrap();

let t = nav.dsi.cell_elapsed_time();
assert!(matches!(t.frame_rate, FrameRate::Ntsc30 | FrameRate::Pal25));
println!("cell elapsed = {:02}:{:02}:{:02}.{:02} ({} ns)",
         t.hours, t.minutes, t.seconds, t.frames,
         nav.dsi.cell_elapsed_ns());
```

`PgcTime::to_nanoseconds()` is also available directly on the
type, returning the rational `(frames × 1e9) / fps` conversion
without needing the `mkv-output` feature.

## Time-based seek (`VTS_TMAPTI` + `VTS_VOBU_ADMAP`)

Once a `VtsIfo` is parsed, the `time_map` field carries one time map
per PGC and `vobu_admap` carries the title-set VOBU sector list. Both
fields are `Option`s; a `None` indicates the corresponding sector
pointer in `VTSI_MAT` was zero (the table was elided by the
authoring tool).

```rust,no_run
use oxideav_dvd::DvdDisc;

let disc = DvdDisc::open("path/to/disc.iso").unwrap();
let mut reader = std::fs::File::open("path/to/disc.iso").unwrap();
let vts = disc.parse_vts(&mut reader, 0).unwrap();

// Where does PGC 1's playback timeline sit at the 30-second mark?
if let Some(sector_in_vob) = vts.vobu_sector_at_pgc_time(1, 30) {
    // Translate the VOB-relative sector to an absolute disc LBA.
    let abs_lba = vts.mat.title_vob_sector + sector_in_vob;
    println!("seek to LBA {abs_lba}");
}

// Iterate every VOBU in the title-set VOBs.
if let Some(admap) = &vts.vobu_admap {
    for vobu in 1..=admap.vobu_count() as u32 {
        let s = admap.vobu_start_sector(vobu).unwrap();
        println!("VOBU {vobu} starts at VOB-relative sector {s}");
    }
}
```

`VobuAdmap::vobu_containing(sector)` performs the inverse lookup —
given a VOB-relative sector, return the 1-based VOBU number whose
range covers it (using a binary partition over the entry list).

## Disassembling a NavCommand (Phase 3c precursor)

The `nav` module decodes each 8-byte PGC command word into a typed
`NavInstruction` tree without executing anything — useful for disc
debuggers, analysers, and the future Phase-3c executor:

```rust,no_run
use oxideav_dvd::{NavInstruction, JumpSSTarget};
use oxideav_dvd::ifo::NavCommand;

// A JumpSS-to-First-Play command (type=1 jump family, selector=0).
let nc = NavCommand { bytes: [0x30, 0x06, 0, 0, 0, 0x00, 0, 0] };
assert_eq!(nc.decode(), NavInstruction::JumpSs(JumpSSTarget::FirstPlay));
```

The decoder covers the well-defined opcodes in Types 0..3 (NOP,
Goto, Break, SetTmpPML, the full Link family with the 13-entry
link-subset table, Exit, JumpTT, JumpVTS_TT, JumpVTS_PTT, the
four-way JumpSS / CallSS target selector, the SetSystem family,
the plain Set arithmetic family with 12 SET sub-ops × GPRM-or-SPRM
source) AND the compound Type 4..6 forms (`SetCLnk`, `CSetCLnk`,
`CmpSetLnk`) — each surfaces the full operand triple: SET source
(register `srs` / `sr2` or 16-bit immediate per the SET-dir flag),
CMP right-hand side (register `cr2` or 16-bit immediate per the
CMP-dir flag), shared selector register (`scr` for Type 4, `sr1`
for Types 5+6), CMP left-hand register (Type 5 only — `cr1`), the
6-bit `hl_bn` highlight-button override, and the 5-bit Link
subset code. The two "Illegal" red rows (SET-dir=1 AND CMP-dir=1
for Types 5+6) surface as `NavInstruction::Invalid` per the spec
page's rejection. The `Register` enum maps an operand byte to
`Gprm(0..=15)` / `Sprm(0..=23)` / `Invalid(_)` per the asterisk
note on `mpucoder-vmi.html`.

## Executing PGC commands (Phase 3c VM)

The `vm` module wraps the disassembler with an interpreter that
owns the register file (16 GPRMs + 24 SPRMs with spec-defined
defaults) and the navigation-resume stack:

```rust,no_run
use oxideav_dvd::{Vm, VmAction, SPRM_AUDIO_STREAM};
use oxideav_dvd::ifo::NavCommand;

let mut vm = Vm::new();
// SPRM defaults loaded per mpucoder-sprm.html.
assert_eq!(vm.regs.sprm(SPRM_AUDIO_STREAM), 15);

// Run a PGC's pre-command list end-to-end.
let pre: Vec<NavCommand> = vec![/* … from PgcCommandTable.pre … */];
let (action, pc) = vm.run_list(&pre);

match action {
    VmAction::Continue => { /* list ran off the end cleanly */ }
    VmAction::JumpTitle { ttn } => { /* player loads title `ttn` */ }
    VmAction::Link(link) => { /* re-enter PGC per `link` */ }
    VmAction::CallSs(target) => { /* push resume + load `target` */ }
    VmAction::Resume(point) => { /* RSM popped — restore `point` */ }
    VmAction::SetNavTimer { seconds, pgcn } => { /* arm wall-clock callback */ }
    other => { /* Break / Exit / NoOpRaw / … */ }
}
let _ = pc;
```

`Vm::step` handles Type 0..3 instructions (Set / SetSystem /
SetGprmMd / SetStn / SetNvtmr / SetHl_BTNN / SetTmpPml / NOP /
Goto / Break) entirely in-process; the Link / Jump / Call family
mutates persistent register state when relevant (CallSs pushes
the resume frame, RSM pops it) then surfaces the destination as
a typed `VmAction` for the playback engine. The compound Type
4..6 forms are executed in spec order per
`mpucoder-vmi-sum.html`:

- **Type 4 `SetCLnk`** runs the SET first, then compares the
  post-SET value of `scr` against `cmp_rhs`; on a `true` compare
  the inner Link subset surfaces as `VmAction::Link(Subset)`,
  otherwise the action collapses to `VmAction::Continue` (the
  outer command list keeps walking).
- **Type 5 `CSetCLnk`** compares first; on `true` it runs SET
  then fires Link; on `false` neither SET nor Link runs.
- **Type 6 `CmpSetLnk`** compares first; on `true` it runs SET;
  the Link **always** fires regardless of the compare outcome —
  that's how Type 6 differs from Type 5.

A compound whose inner Link subset is `Nop` collapses to
`Continue` even when the SET / CMP ran; an `Rsm` subset pops the
same RSM stack as a bare Type-1 `LinkSub::Rsm`. Runaway `Goto`
loops are bounded by a step budget so a malformed disc can never
hang the interpreter.

### SPRM bitfield-aware accessors

Every SPRM the spec page documents with a non-integer layout has a
typed accessor — bit-packed payloads (SPRM 2 sub-picture state,
SPRM 8 highlighted button, SPRM 11 karaoke mixing, SPRM 14 video
preference, SPRM 15 audio capabilities, SPRM 20 region mask) as well
as the two-byte ISO 639 / ISO 3166 language slots
(SPRM 0 menu language, SPRM 12 parental country, SPRM 16 / 18
preferred audio / sub-picture language) and the sentinel-typed
integer slots (SPRM 1 audio stream `0..=7` + `15`-none, SPRM 3 angle
`1..=9`, SPRM 13 parental level `1..=8` + `15`-none, SPRM 17 / 19
language extension enums). The `RegisterFile` surfaces them all so a
playback engine doesn't re-implement the layouts on each callsite —
refer to `docs/container/dvd/application/mpucoder-sprm.html` for the
canonical field allocations.

```rust,no_run
use oxideav_dvd::{
    Vm, AspectRatio, DisplayMode, AudioStreamSelector, ParentalLevel,
    AudioLanguageExt, SubpictureLanguageExt,
};

let vm = Vm::new();
// SPRM 2 default = "stream 62 / do-not-display"
let spu = vm.regs.subpicture_stream();
assert!(spu.is_none_sentinel());
assert!(!spu.display);

// SPRM 8 default = button 1
assert_eq!(vm.regs.highlight_button(), 1);

// SPRM 14 video preference (4:3 / Normal mode by default)
let vp = vm.regs.video_preference();
assert_eq!(vp.aspect, AspectRatio::Ar4x3);
assert_eq!(vp.mode, DisplayMode::Normal);

// SPRM 20 region mask — default = no regions enabled
assert!(!vm.regs.region_allowed(1));

// SPRM 1 / 3 sentinels — defaults
assert_eq!(vm.regs.audio_stream(), AudioStreamSelector::None);
assert_eq!(vm.regs.angle_number(), Some(1));

// SPRM 13 parental level — uninitialised raw "0" → `Invalid(0)`
// (the spec defines `1..=8` real, `15` = control-off; the
// "player specific" default leaves the slot zero).
assert_eq!(vm.regs.parental_level(), ParentalLevel::Invalid(0));

// SPRM 16 / 18 language code defaults — `0xFFFF` "not specified"
assert!(vm.regs.preferred_audio_language().is_not_specified());
assert!(vm.regs.preferred_subpicture_language().is_not_specified());

// SPRM 17 / 19 extension defaults — "not specified"
assert_eq!(
    vm.regs.preferred_audio_language_ext(),
    AudioLanguageExt::NotSpecified,
);
assert_eq!(
    vm.regs.preferred_subpicture_language_ext(),
    SubpictureLanguageExt::NotSpecified,
);
```

For the language slots, `LanguageCode::ascii_bytes()` returns
`Some([hi, lo])` when both bytes are printable ASCII letters and
`as_string()` returns the lower-cased ISO 639 / ISO 3166 alpha-2
form. The `0xFFFF` value matches the `LanguageCode::NOT_SPECIFIED`
sentinel and `is_not_specified()` short-circuits both decoders.

Each accessor decomposes the raw `u16` according to the spec
page's bit allocation and preserves the original word on the
returned view's `raw` field, so a caller that wants to forward
the SPRM verbatim can round-trip it bit-for-bit.

## Clean-room sources

This crate was written entirely against:

- `docs/container/dvd/physical/ECMA-267_3rd_edition_april_2001.pdf`
  — DVD-ROM physical layer.
- `docs/container/dvd/physical/ECMA-268_3rd_edition_april_2001.pdf`
  — DVD-ROM file system specification (UDF 1.02 + ISO 9660 bridge
  layer constraints).
- `docs/container/dvd/physical/OSTA_UDF_1.02.pdf` — OSTA UDF profile
  used by DVD-Video.
- `docs/container/bluray/ECMA-167_3rd_edition_june_1997.pdf` — the
  underlying UDF base standard (cross-referenced; UDF 1.02 is a
  strict subset of UDF 2.50 by ECMA-167).
- `docs/container/dvd/application/mpucoder-ifo.html`,
  `mpucoder-ifo_vmg.html`, `mpucoder-ifo_vts.html`,
  `mpucoder-pgc.html`, `stnsoft-vmindx.html` — community
  reverse-engineering references mirrored under
  [`docs/container/dvd/application/`](../../docs/container/dvd/application/)
  for the VIDEO_TS layout, the IFO field layouts (VMGI_MAT /
  VTSI_MAT / TT_SRPT / VTS_PTT_SRPT / VTS_PGCI / VTS_C_ADT /
  VTSM_PGCI_UT / VMGM_PGCI_UT / VMG_VTS_ATRT / VMG_PTL_MAIT /
  VTS_VOBU_ADMAP / VTS_TMAPTI / VTS_TMAP) and the PGC body
  structure (PGC_GI header, audio/sub-picture stream control, the
  16-entry `(0, Y, Cr, Cb)` subpicture colour-LUT at PGC offset
  `0x00A4`, the pre/post/cell command table, program map, Cell
  Playback Information Table, Cell Position Information Table).
  Decoding each `NavCommand` into a typed `NavInstruction` tree
  lives in the `nav` module; executing the decoded form is the
  remaining Phase 3c VM work.
- `docs/container/dvd/application/mpucoder-vmi.html`,
  `mpucoder-vmi-sum.html`, `mpucoder-vmi-jmp.html`,
  `mpucoder-sprm.html` — the VM instruction set (the full opcode
  table including SET/CMP sub-ops and the link-subset inner table,
  the plain-English instruction-family summary, the jump/call target
  table, and the 24-entry SPRM map) feeding the `nav` module's
  `NavInstruction` decoder.
- `docs/container/dvd/application/mpucoder-spu.html` — the
  Sub-Picture Unit layout (SPUH, the four PXD run-length forms +
  the end-of-line terminator + per-row byte alignment, the eight
  SP_DCSQ command codes including the `LN_CTLI` / `PX_CTLI`
  parameter hierarchy of `CHG_COLCON`, and the 90 kHz/1024 delay
  conversion table) feeding the `spu` module's `SubPictureUnit`
  decoder.
- `docs/container/dvd/application/stnsoft-color_pick.html` — fixes the
  subpicture palette's BT.601 studio-swing luma scale (`Y = 16` 0 % …
  `Y = 235` 100 %) used by the `spu` module's `ycbcr_to_rgb` / RGBA
  compositor.
- `docs/container/dvd/application/mpucoder-packhdr.html`,
  `mpucoder-pes-hdr.html`, `mpucoder-mpeghdrs.html`,
  `mpucoder-pci_pkt.html`, `mpucoder-dsi_pkt.html`,
  `mpucoder-dvdmpeg.html`, `stnsoft-vobov.html`,
  `stnsoft-sys_hdr.html` — VOB MPEG-PS pack header, PES header
  (DVD subset), MPEG-PS stream-ID table, NAV-pack PCI / DSI
  packet layouts, DVD substream allocations, VOBU / cell / VOB
  semantics, and the Program Stream System Header used by the
  Phase 3a VOB demuxer.
- `docs/container/dvd/application/mpucoder-lpcm.html` — the 7-byte
  LPCM audio-pack header layout (quantisation / sample-rate /
  channel-count fields, first-access-unit pointer, the X/Y
  dynamic-range coefficients) feeding the `lpcm` module's
  `LpcmHeader` decoder.
- `docs/container/dvd/application/stnsoft-LimPcmAud.html` — the
  per-`(sample_rate × quantisation × channels)` bitrate table and
  the 6144 kbps DVD-Video ceiling used by
  `LpcmHeader::is_within_dvd_video_limit`.
- `docs/container/dvd/application/mpucoder-uops.html` — the 25-row
  User Operation flag table (bit numbers + per-level applicability
  matrix + three-level OR-merge rule) feeding the `uops` module's
  `UserOp` / `UopMask` / `UopLevel` decoder and the typed
  `uop_mask()` / `is_user_op_allowed()` accessors exposed on
  `Pgc`, `PciPacket`, and `DvdTitleEntry`.

The crate is built clean-room from the spec PDFs and the
behavioural-trace HTML pages listed above; no external player or
library source was consulted. The `*-COPYING` / `*-README.md`
files in `docs/container/dvd/` are licence-trail transparency
markers carried by the docs collaborator, not implementation
references.

## License

MIT — see [LICENSE](LICENSE).
