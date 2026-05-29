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
chapter / program-chain / cell layout out of every IFO, and demux
each cell's VOBUs into raw MPEG-2 video + AC-3 / DTS / LPCM audio
+ subpicture elementary streams keyed by track ID. The Phase 3a
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
for the compound Type 4..6 CMP/SET/LNK encodings. **No VM
execution yet** — an interpreter that owns SPRMs / GPRMs / PC /
RSM stack is the bulk of Phase 3c proper. **No CSS yet** — Phase
3c via the external `oxideav-css` crate.

| Layer | Status |
|-------|--------|
| ISO 9660 PVD + path-table + directory walk | landed |
| UDF 1.02 mount (PVD / PD / LVD / FSD / FE / FID) | landed |
| VIDEO_TS file enumeration (VIDEO_TS.IFO / .VOB / .BUP + VTS_xx) | landed |
| `dvd://` source driver (registry feature) | landed |
| VMGI / VTSI MAT parse (header + sector pointers) | landed |
| TT_SRPT (title list) + VTS_PTT_SRPT (chapter list) | landed |
| VTS_PGCI + PGC (program chains + cells + colour-LUT + command table) | landed |
| VTS_C_ADT (cell-to-VOB-sector lookup) | landed |
| VOB demux (MPEG-PS pack + nav-pack + PES) | landed (Phase 3a) |
| DVD substream routing (AC-3 / DTS / LPCM / subpicture) | landed (Phase 3a) |
| VOBU_SRI search-table decode | landed (Phase 3a) |
| NAV-pack PCI highlight (HLI_GI + SL_COLI + BTN_IT buttons) | landed (Phase 3a) |
| NAV-pack DSI typed sub-sections (DSI_GI + SML_PBI + SML_AGLI + VOBU_SRI + SYNCI) | landed (Phase 3a) |
| MKV mux + chapter encoding wiring | landed (Phase 3b, `mkv-output` feature) |
| VM instruction **decode** (typed `NavInstruction` disassembler — non-executing) | landed (Phase 3c precursor) |
| Sub-Picture Unit (SPU) decode (SPUH + SP_DCSQT command stream + PXDtf/PXDbf 2-bit RLE) | landed |
| SPU → RGBA compositor (palette + contrast resolve + BT.601 YCbCr→RGB + field interleave) | landed |
| VM **execution** (interpreter over SPRMs/GPRMs + RSM stack + PC) | Phase 3c |
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
source). The compound Type 4..6 forms (`SetCLnk`, `CSetCLnk`,
`CmpSetLnk`) surface their classifier `SetOp` / `CmpOp` sub-ops but
leave full operand decoding to a future executor. The `Register`
enum maps an operand byte to `Gprm(0..=15)` / `Sprm(0..=23)` /
`Invalid(_)` per the asterisk note on `mpucoder-vmi.html`.

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
  VTSM_PGCI_UT / VMGM_PGCI_UT / VMG_VTS_ATRT / VMG_PTL_MAIT) and
  the PGC body structure (PGC_GI header, audio/sub-picture stream
  control, the 16-entry `(0, Y, Cr, Cb)` subpicture colour-LUT at
  PGC offset `0x00A4`, the pre/post/cell command table, program
  map, Cell Playback Information Table, Cell Position Information
  Table). Decoding each `NavCommand` into a typed `NavInstruction`
  tree lives in the `nav` module; executing the decoded form is the
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

**Not consulted**: libdvdread, libdvdnav, libdvdcss, FFmpeg, xine,
mpv, VLC, or any other open-source DVD player or library. The
`libdvdread-README.md` / `libdvdnav-README.md` / `libdvdcss-README.md`
files in `docs/container/dvd/` are licence-trail transparency
markers, not implementation references.

## License

MIT — see [LICENSE](LICENSE).
