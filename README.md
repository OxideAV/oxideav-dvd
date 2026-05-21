# oxideav-dvd

[![Crates.io](https://img.shields.io/crates/v/oxideav-dvd.svg)](https://crates.io/crates/oxideav-dvd)
[![Docs.rs](https://docs.rs/oxideav-dvd/badge.svg)](https://docs.rs/oxideav-dvd)
[![CI](https://github.com/OxideAV/oxideav-dvd/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-dvd/actions/workflows/ci.yml)

Read-only **DVD-Video** disc reader — ISO 9660 + UDF 1.02 mount +
`VIDEO_TS/` directory walk + IFO body parser (VMGI / VTSI / PGC /
TT_SRPT / VTS_PTT_SRPT / VTS_C_ADT) + VOB demuxer (MPEG-PS pack
+ nav-pack + DVD substream router). Clean-room per ECMA-267 /
ECMA-268 + OSTA UDF 1.02 + the ECMA-167 UDF base standard +
mpucoder + stnsoft community RE references.

## Scope

Phases 1, 2, and 3a (this release) handle the **physical +
filesystem + disc-identification + IFO structural + VOB demux**
layers — enough to point a player at a DVD-Video disc image or
block device, enumerate the title-set files, pull the title /
chapter / program-chain / cell layout out of every IFO, and demux
each cell's VOBUs into raw MPEG-2 video + AC-3 / DTS / LPCM audio
+ subpicture elementary streams keyed by track ID. The Phase 3a
nav-pack decoder also surfaces the `VOBU_SRI` search-pointer table
that chapter-accurate seek needs. **No VM execution, no CSS yet**
— those land in Phase 3b/3c.

| Layer | Status |
|-------|--------|
| ISO 9660 PVD + path-table + directory walk | landed |
| UDF 1.02 mount (PVD / PD / LVD / FSD / FE / FID) | landed |
| VIDEO_TS file enumeration (VIDEO_TS.IFO / .VOB / .BUP + VTS_xx) | landed |
| `dvd://` source driver (registry feature) | landed |
| VMGI / VTSI MAT parse (header + sector pointers) | landed |
| TT_SRPT (title list) + VTS_PTT_SRPT (chapter list) | landed |
| VTS_PGCI + PGC (program chains + cells) | landed |
| VTS_C_ADT (cell-to-VOB-sector lookup) | landed |
| VOB demux (MPEG-PS pack + nav-pack + PES) | landed (Phase 3a) |
| DVD substream routing (AC-3 / DTS / LPCM / subpicture) | landed (Phase 3a) |
| VOBU_SRI search-table decode | landed (Phase 3a) |
| MKV mux + chapter encoding wiring | Phase 3b |
| VM execution (HDMV nav opcodes + SPRMs/GPRMs) | Phase 3c |
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
  control, palette, program map, Cell Playback Information Table,
  Cell Position Information Table).
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
