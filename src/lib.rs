//! Read-only **DVD-Video** disc reader — ISO 9660 + UDF 1.02 mount +
//! `VIDEO_TS/` directory walk.
//!
//! Phase 1 (this release) handles the physical + filesystem + disc-
//! identification layers — enough to point a player at a DVD-Video
//! disc image or block device and enumerate the title-set files.
//! **No IFO / VOB / PGC / VM / CSS parsing yet.**
//!
//! ## Scope
//!
//! - ISO 9660 PVD + path-table + directory walk (the ECMA-268
//!   bridge layer).
//! - UDF 1.02 mount: AVDP, Volume Descriptor Sequence, File Set
//!   Descriptor, root File Identifier Descriptor walk, File Entry
//!   parsing with short_ad / long_ad / ext_ad allocation descriptors.
//! - `VIDEO_TS/` file enumeration: `VIDEO_TS.IFO` + per-VTS
//!   `VTS_xx_0.IFO` / `VTS_xx_0.VOB` (menu) / `VTS_xx_1..9.VOB`
//!   (title content) / `VTS_xx_0.BUP`.
//! - `dvd://` URI handler (default-on `registry` feature) that
//!   surfaces a `DvdDiscSource` to `oxideav_core::SourceRegistry`.
//!
//! Out of scope (deferred to Phase 2 / Phase 3):
//! - IFO body parsing (VMGI, VTSI, PGCI, cell-address tables).
//! - VOB demuxing (MPEG-2 Program Stream + nav-pack overlays).
//! - VM execution (HDMV navigation opcodes, SPRMs / GPRMs).
//! - CSS authentication + descrambling (lives in a future
//!   `oxideav-css` sibling crate).
//!
//! ## Sub-Picture Unit decoder (`spu` module)
//!
//! The `spu` module parses one DVD subpicture (subtitle / menu
//! overlay) blob assembled from concatenated PES packet payloads
//! on substream `0x20..=0x3F`: SPUH + the chained
//! `SP_DCSQT` command stream + the two PXD fields' 2-bit
//! run-length-encoded pixel data. [`spu::SubPictureUnit::composite`]
//! optionally resolves those palette indices through the PGC's
//! 16-entry [`ifo::PaletteEntry`] colour-LUT (BT.601 studio-swing
//! YCbCr → RGB plus the SET_CONTR alpha) into a finished RGBA
//! [`spu::SpuBitmap`] overlay; blending it onto the decoded video
//! frame stays with the player.
//!
//! ## Phase 3b — `mkv-output` feature (default off)
//!
//! Enable `mkv-output` to pull in [`pipeline::convert_dvd_to_mkv`],
//! which walks a title's VOBs and writes a Matroska file with the
//! PGC's chapter timeline. The feature pulls in `oxideav-mkv` as a
//! runtime dependency; default builds (and default-feature CI) stay
//! free of it so the crate keeps compiling against any published
//! `oxideav-mkv` version.
//!
//! ## Clean-room references
//!
//! - `docs/container/dvd/physical/ECMA-267_3rd_edition_april_2001.pdf`
//! - `docs/container/dvd/physical/ECMA-268_3rd_edition_april_2001.pdf`
//! - `docs/container/dvd/physical/OSTA_UDF_1.02.pdf`
//! - `docs/container/bluray/ECMA-167_3rd_edition_june_1997.pdf` (cross-ref)
//! - `docs/container/dvd/application/mpucoder-ifo.html` (for the
//!   VIDEO_TS directory layout, file-naming convention, and BUP
//!   backup semantics only — IFO bodies are out of Phase-1 scope)
//!
//! ## Quick start
//!
//! ```no_run
//! use oxideav_dvd::DvdDisc;
//!
//! let disc = DvdDisc::open("path/to/disc.iso").unwrap();
//! println!("volume_id = {}", disc.volume_id);
//! println!("title_set_count = {}", disc.title_set_count);
//! for f in &disc.video_ts_files {
//!     println!("  {:?}  lba={}  size={}", f.kind, f.lba, f.size);
//! }
//! ```
//!
//! ## Standalone build
//!
//! `oxideav-core` is gated behind the default-on `registry` feature.
//! Drop the framework dependency entirely with:
//!
//! ```toml
//! oxideav-dvd = { version = "0.0", default-features = false }
//! ```

#![deny(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod disc;
pub mod error;
pub mod ifo;
pub mod iso9660;
pub mod lpcm;
pub mod nav;
pub mod source;
pub mod spu;
pub mod udf;
pub mod uops;
pub mod vm;
pub mod vob;

#[cfg(feature = "mkv-output")]
pub mod mkv_writer;
#[cfg(feature = "mkv-output")]
pub mod pipeline;

pub use disc::{DvdDisc, DvdFile, DvdFileKind};
pub use error::{Error, Result};
pub use ifo::{
    AudioApplicationMode, AudioAttributes, AudioCodingMode, AudioLanguageType,
    AudioQuantizationDrc, CellAddrEntry, CellPlaybackInfo, CellPositionInfo, DvdChapter, DvdTitle,
    DvdTitleEntry, FrameRate, McExtensionEntry, MenuAttributes, NavCommand, PaletteEntry, Pgc,
    PgcCommandTable, PgcTime, Pgci, PgciSrp, Ptt, PttTitle, SubpictureAttributes,
    SubpictureCodingMode, SubpictureLanguageType, TitleAttributes, TmapEntry, TtSrpt,
    VideoAspectRatio, VideoAttributes, VideoCodingMode, VideoResolution, VideoStandard, VmgIfo,
    VobuAdmap, VtsCAdt, VtsIfo, VtsPttSrpt, VtsTmap, VtsTmapti, VtsiMat, DVD_SECTOR, VMG_MAGIC,
    VTS_MAGIC,
};
pub use iso9660::{
    DirectoryRecord, Iso9660Entry, Iso9660Volume, PathTableEntry, PrimaryVolumeDescriptor,
    VolumeDescriptorType,
};
pub use lpcm::{
    peel_lpcm_payload, LpcmHeader, LpcmQuantisation, LpcmSampleFrequency,
    DVD_LPCM_MAX_BITRATE_KBPS, LPCM_HEADER_LEN,
};
pub use nav::{
    CallSSTarget, CmpOp, JumpSSTarget, LinkSubset, NavInstruction, Operand, Register, SetOp,
};
pub use source::{parse_dvd_uri, DvdDiscSource, DvdUri};
pub use spu::{
    decode_rle_field, render_field, spdcsq_stm_to_ms, ycbcr_to_rgb, PixelRun, SpDcSq, SpuBitmap,
    SpuCommand, SpuHeader, SubPictureUnit,
};
pub use udf::{
    AdType, AnchorVolumeDescriptorPointer, DescriptorTag, ExtAd, Extent, FileEntry,
    FileIdentifierDescriptor, FileSetDescriptor, IcbTag, LbAddr, LogicalVolumeDescriptor, LongAd,
    PartitionDescriptor, ShortAd, TagId, UdfFile, UdfVolume,
};
pub use uops::{
    title_type_uop_mask, UopIter, UopLevel, UopMask, UserOp, UOP_ANGLE_CHANGE,
    UOP_AUDIO_STREAM_CHANGE, UOP_BACKWARD_SCAN, UOP_BIT_COUNT, UOP_BUTTON_SELECT_OR_ACTIVATE,
    UOP_DEFINED_BITS, UOP_FORWARD_SCAN, UOP_GO_UP, UOP_KARAOKE_AUDIO_MIX_CHANGE,
    UOP_MENU_CALL_ANGLE, UOP_MENU_CALL_AUDIO, UOP_MENU_CALL_PTT, UOP_MENU_CALL_ROOT,
    UOP_MENU_CALL_SUBPICTURE, UOP_MENU_CALL_TITLE, UOP_NEXT_PG_SEARCH, UOP_PAUSE_ON,
    UOP_PTT_PLAY_OR_SEARCH, UOP_RESUME, UOP_STILL_OFF, UOP_STOP, UOP_SUBPICTURE_STREAM_CHANGE,
    UOP_TIME_OR_PTT_SEARCH, UOP_TIME_PLAY_OR_SEARCH, UOP_TITLE_PLAY, UOP_TOP_PG_OR_PREV_PG_SEARCH,
    UOP_VIDEO_PRESENTATION_MODE_CHANGE,
};
pub use vm::{
    AspectRatio, AudioCapabilities, AudioMixMode, DisplayMode, LinkAction, RegisterFile,
    ResumePoint, SubpictureStreamView, VideoPreference, Vm, VmAction, GPRM_COUNT, MAX_RSM_DEPTH,
    SPRM_AMXMD, SPRM_ANGLE, SPRM_AUDIO_CAPS, SPRM_AUDIO_STREAM, SPRM_CC_PLT, SPRM_COUNT,
    SPRM_HL_BTNN, SPRM_MENU_LANG, SPRM_NV_PGCN, SPRM_NV_TIMER, SPRM_PARENTAL_LEVEL, SPRM_PGCN,
    SPRM_PREF_AUDIO_LANG, SPRM_PREF_AUDIO_LANG_EXT, SPRM_PREF_SUBP_LANG, SPRM_PREF_SUBP_LANG_EXT,
    SPRM_PTT, SPRM_REGION_MASK, SPRM_SUBPICTURE_STREAM, SPRM_TITLE, SPRM_VIDEO_PREF,
    SPRM_VTS_TITLE,
};
pub use vob::{
    demux_vobs, demux_vobs_path, looks_like_nav_pack, ButtonInfo, CellId, DsiGi, DsiPacket,
    DvdSubstream, ElementaryStream, HighlightInfo, NavPack, PackHeader, PciPacket, PesPacket,
    SlColi, SlColiCell, SmlAgli, SmlAngleCell, SmlAudioGap, SmlPbi, Synci, VobDemuxer, VobId,
    VobStreams, VobuSri,
};

#[cfg(feature = "registry")]
pub use source::register;

#[cfg(feature = "mkv-output")]
pub use mkv_writer::{pgc_time_to_ns, write_title_to_mkv};
#[cfg(feature = "mkv-output")]
pub use pipeline::{convert_dvd_to_mkv, list_titles};

// Canonical sibling entry point. Registers the `dvd://` source driver
// under `oxideav_core::RuntimeContext::sources`.
#[cfg(feature = "registry")]
oxideav_core::register!("dvd", source::register);
