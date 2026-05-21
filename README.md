# oxideav-dvd

[![Crates.io](https://img.shields.io/crates/v/oxideav-dvd.svg)](https://crates.io/crates/oxideav-dvd)
[![Docs.rs](https://docs.rs/oxideav-dvd/badge.svg)](https://docs.rs/oxideav-dvd)
[![CI](https://github.com/OxideAV/oxideav-dvd/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-dvd/actions/workflows/ci.yml)

Read-only **DVD-Video** disc reader — ISO 9660 + UDF 1.02 mount +
`VIDEO_TS/` directory walk. Clean-room per ECMA-267 / ECMA-268 +
OSTA UDF 1.02 + the ECMA-167 UDF base standard.

## Scope

Phase 1 (this release) handles the **physical + filesystem + disc-
identification** layers — everything needed to point a player at a
DVD-Video disc image or block device and enumerate the title-set
files. **No IFO / VOB / PGC / VM / CSS parsing yet** — those land
in Phase 2 and Phase 3.

| Layer | Status |
|-------|--------|
| ISO 9660 PVD + path-table + directory walk | landed |
| UDF 1.02 mount (PVD / PD / LVD / FSD / FE / FID) | landed |
| VIDEO_TS file enumeration (VIDEO_TS.IFO / .VOB / .BUP + VTS_xx) | landed |
| `dvd://` source driver (registry feature) | landed |
| IFO body parsing (VMGI / VTSI / PGC / cell address tables) | Phase 2 |
| VOB demux (MPEG-2 PS + nav-packs) | Phase 2 |
| VM execution (HDMV nav opcodes + SPRMs/GPRMs) | Phase 3 |
| CSS authentication + descrambling | Phase 3 (external `oxideav-css` crate) |

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
  `mpucoder-ifo_vmg.html`, `mpucoder-ifo_vts.html` — community
  reverse-engineering references mirrored under
  [`docs/container/dvd/application/`](../../docs/container/dvd/application/)
  for the VIDEO_TS layout discriminator (file names, BUP backups,
  per-VTS numbering scheme). These pages are reference-only; the
  Phase 1 file enumerator does not parse IFO bodies.

**Not consulted**: libdvdread, libdvdnav, libdvdcss, FFmpeg, xine,
mpv, VLC, or any other open-source DVD player or library. The
`libdvdread-README.md` / `libdvdnav-README.md` / `libdvdcss-README.md`
files in `docs/container/dvd/` are licence-trail transparency
markers, not implementation references.

## License

MIT — see [LICENSE](LICENSE).
