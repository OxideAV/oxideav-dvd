# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
