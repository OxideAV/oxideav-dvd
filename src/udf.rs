//! Minimal read-only UDF 1.02 mounter, scoped to what's needed to
//! walk the `VIDEO_TS/` and `AUDIO_TS/` directories of a DVD-Video
//! disc.
//!
//! References:
//! - ECMA-167 3rd edition (June 1997) — the UDF base standard.
//! - OSTA UDF 1.02 (2007 Wayback snapshot) — the OSTA profile used
//!   by DVD-Video and DVD-ROM, per ECMA-268 §9.
//!
//! UDF 1.02 is a strict subset of UDF 2.50 by ECMA-167 — every
//! descriptor we touch (PVD, Partition, LVD, FSD, FID, FE) has an
//! identical wire format. The notable differences for DVD-Video:
//!
//! - Extended File Entries (tag 266) do **not** appear in 1.02 —
//!   only the plain File Entry (tag 261). We refuse EFE as a
//!   safety net but in practice DVD-Video discs never carry them.
//! - The Logical Volume Descriptor's domain identifier suffix is
//!   `\x00\x02\x01\x00` (UDF Revision = 0x0102) for DVD-Video,
//!   versus `\x00\x02\x50\x00` for UDF 2.50. We do not gate on this.
//! - The AVDP can live at sector 256, the second-anchor sector
//!   (typically 512 or last-sector for DVD-Video), and N-256 per
//!   §3/10.2. We probe in that order; the first valid AVDP wins.
//!
//! ## What's implemented
//!
//! - 2048-byte logical sector size (mandatory on DVD per ECMA-268).
//! - AVDP probe at sectors 256 / 512 / (volume_size - 256).
//! - Volume Descriptor Sequence (PVD / PD / LVD / Terminating).
//! - File Set Descriptor + Root Directory ICB.
//! - File Identifier Descriptor walks with the §14.4 padding rule
//!   (records are rounded up to a 4-byte boundary).
//! - File Entry parsing with Short_ad / Long_ad / Ext_ad and
//!   embedded-in-ICB content.
//! - OSTA Compressed Unicode `d-string` decoding (compression IDs
//!   8 and 16) per UDF 1.02 §2.1.3.
//!
//! ## What's not implemented (Phase 1 — surface `Unsupported`)
//!
//! - Multi-extent partition maps (`partition_map_count > 1`).
//! - ICB strategy types other than 4 (the spec's default linear).
//! - Sparse / sequential files.
//! - Allocation Extent Descriptor continuation (extent_type == 3).
//! - Extended File Entry (tag 266).
//! - Symbolic Links / Streams.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{Error, Result};

/// Logical sector size on a DVD: mandatory 2048 bytes (ECMA-268 §6.1).
// internal — exposed for tests/fuzz; not part of the stable API (use `ifo::DVD_SECTOR`)
#[doc(hidden)]
pub const SECTOR_SIZE: u64 = 2048;
/// First sector at which we look for the Anchor Volume Descriptor
/// Pointer per ECMA-167 §3/10.2.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub const AVDP_SECTOR_PRIMARY: u64 = 256;
/// Conventional secondary AVDP location used by DVD-Video authoring
/// tools. The actual ECMA-167 rule is "last sector or last-sector-
/// minus-256" but DVDs commonly mirror the AVDP at sector 512 too;
/// we probe both.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub const AVDP_SECTOR_SECONDARY: u64 = 512;

// ─────────────────────── Descriptor tag (§7.2) ───────────────────────

/// Numeric `TagIdentifier` of every descriptor we touch. Values from
/// ECMA-167 §3/7.2.1 unless otherwise noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum TagId {
    PrimaryVolume = 1,
    AnchorVolumeDescriptorPointer = 2,
    VolumeDescriptorPointer = 3,
    ImplementationUseVolume = 4,
    Partition = 5,
    LogicalVolume = 6,
    UnallocatedSpace = 7,
    Terminating = 8,
    LogicalVolumeIntegrity = 9,
    FileSet = 256,
    FileIdentifier = 257,
    AllocationExtent = 258,
    Indirect = 259,
    Terminal = 260,
    FileEntry = 261,
    ExtendedAttributeHeader = 262,
    UnallocatedSpaceEntry = 263,
    SpaceBitmap = 264,
    PartitionIntegrityEntry = 265,
    ExtendedFileEntry = 266,
}

impl TagId {
    pub fn from_raw(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::PrimaryVolume,
            2 => Self::AnchorVolumeDescriptorPointer,
            3 => Self::VolumeDescriptorPointer,
            4 => Self::ImplementationUseVolume,
            5 => Self::Partition,
            6 => Self::LogicalVolume,
            7 => Self::UnallocatedSpace,
            8 => Self::Terminating,
            9 => Self::LogicalVolumeIntegrity,
            256 => Self::FileSet,
            257 => Self::FileIdentifier,
            258 => Self::AllocationExtent,
            259 => Self::Indirect,
            260 => Self::Terminal,
            261 => Self::FileEntry,
            262 => Self::ExtendedAttributeHeader,
            263 => Self::UnallocatedSpaceEntry,
            264 => Self::SpaceBitmap,
            265 => Self::PartitionIntegrityEntry,
            266 => Self::ExtendedFileEntry,
            _ => return None,
        })
    }
}

/// The 16-byte descriptor tag prefix common to every numbered
/// descriptor (§7.2). All multi-byte fields are little-endian.
///
/// ```text
///   0  TagIdentifier        u16 LE
///   2  DescriptorVersion    u16 LE   (2 for UDF 1.0x, 3 for 2.x+)
///   4  TagChecksum          u8       (sum of bytes 0..16 except [4] mod 256)
///   5  Reserved             u8
///   6  TagSerialNumber      u16 LE
///   8  DescriptorCRC        u16 LE
///  10  DescriptorCRCLength  u16 LE
///  12  TagLocation          u32 LE   (sector this tag is recorded at)
/// ```
#[derive(Debug, Clone, Copy)]
pub struct DescriptorTag {
    pub id: TagId,
    pub descriptor_version: u16,
    pub serial_number: u16,
    pub crc: u16,
    pub crc_length: u16,
    pub location: u32,
}

impl DescriptorTag {
    pub const SIZE: usize = 16;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::InvalidUdf("descriptor tag truncated"));
        }
        let id_raw = u16::from_le_bytes([bytes[0], bytes[1]]);
        let id = TagId::from_raw(id_raw).ok_or(Error::InvalidUdf("unknown TagId"))?;
        let descriptor_version = u16::from_le_bytes([bytes[2], bytes[3]]);
        let checksum = bytes[4];
        if bytes[5] != 0 {
            return Err(Error::InvalidUdf("DescriptorTag reserved byte non-zero"));
        }
        let serial_number = u16::from_le_bytes([bytes[6], bytes[7]]);
        let crc = u16::from_le_bytes([bytes[8], bytes[9]]);
        let crc_length = u16::from_le_bytes([bytes[10], bytes[11]]);
        let location = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);

        let calc: u32 = bytes[..Self::SIZE]
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 4)
            .map(|(_, b)| *b as u32)
            .sum();
        if (calc & 0xFF) as u8 != checksum {
            return Err(Error::InvalidUdf("DescriptorTag checksum mismatch"));
        }

        Ok(Self {
            id,
            descriptor_version,
            serial_number,
            crc,
            crc_length,
            location,
        })
    }

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let id = self.id as u16;
        out[0..2].copy_from_slice(&id.to_le_bytes());
        out[2..4].copy_from_slice(&self.descriptor_version.to_le_bytes());
        out[6..8].copy_from_slice(&self.serial_number.to_le_bytes());
        out[8..10].copy_from_slice(&self.crc.to_le_bytes());
        out[10..12].copy_from_slice(&self.crc_length.to_le_bytes());
        out[12..16].copy_from_slice(&self.location.to_le_bytes());
        let sum: u32 = out
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 4)
            .map(|(_, b)| *b as u32)
            .sum();
        out[4] = (sum & 0xFF) as u8;
        out
    }
}

// ─────────────────────── extent_ad (§7.1) ───────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentAd {
    pub length: u32,   // bytes
    pub location: u32, // logical block number
}

impl ExtentAd {
    pub const SIZE: usize = 8;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::InvalidUdf("extent_ad truncated"));
        }
        Ok(Self {
            length: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            location: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.length.to_le_bytes());
        out[4..8].copy_from_slice(&self.location.to_le_bytes());
        out
    }
}

// ─────────────────────── short_ad (§14.14.1.1) ───────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortAd {
    pub length: u32,
    pub extent_type: u8,
    pub block_location: u32,
}

impl ShortAd {
    pub const SIZE: usize = 8;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::InvalidUdf("short_ad truncated"));
        }
        let raw_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let block_location = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        Ok(Self {
            length: raw_len & 0x3FFF_FFFF,
            extent_type: ((raw_len >> 30) & 0b11) as u8,
            block_location,
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let raw_len = (self.length & 0x3FFF_FFFF) | ((self.extent_type as u32 & 0b11) << 30);
        out[0..4].copy_from_slice(&raw_len.to_le_bytes());
        out[4..8].copy_from_slice(&self.block_location.to_le_bytes());
        out
    }
}

// ─────────────────────── lb_addr / long_ad / ext_ad (§7.1) ───────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LbAddr {
    pub block: u32,
    pub partition_ref: u16,
}

impl LbAddr {
    pub const SIZE: usize = 6;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::InvalidUdf("lb_addr truncated"));
        }
        Ok(Self {
            block: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            partition_ref: u16::from_le_bytes([bytes[4], bytes[5]]),
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.block.to_le_bytes());
        out[4..6].copy_from_slice(&self.partition_ref.to_le_bytes());
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongAd {
    pub length: u32,
    pub extent_type: u8,
    pub location: LbAddr,
    pub implementation_use: [u8; 6],
}

impl LongAd {
    pub const SIZE: usize = 16;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::InvalidUdf("long_ad truncated"));
        }
        let raw_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let location = LbAddr::parse(&bytes[4..10])?;
        let mut impl_use = [0u8; 6];
        impl_use.copy_from_slice(&bytes[10..16]);
        Ok(Self {
            length: raw_len & 0x3FFF_FFFF,
            extent_type: ((raw_len >> 30) & 0b11) as u8,
            location,
            implementation_use: impl_use,
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let raw_len = (self.length & 0x3FFF_FFFF) | ((self.extent_type as u32 & 0b11) << 30);
        out[0..4].copy_from_slice(&raw_len.to_le_bytes());
        out[4..10].copy_from_slice(&self.location.encode());
        out[10..16].copy_from_slice(&self.implementation_use);
        out
    }
}

/// `ext_ad`: 20-byte extended allocation descriptor (§7.1).
/// Layout: 4-byte length+type, 4-byte recorded length, 4-byte
/// information length, 6-byte logical block address, 2-byte
/// implementation use field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtAd {
    pub length: u32,
    pub extent_type: u8,
    pub recorded_length: u32,
    pub information_length: u32,
    pub location: LbAddr,
    pub implementation_use: [u8; 2],
}

impl ExtAd {
    pub const SIZE: usize = 20;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::InvalidUdf("ext_ad truncated"));
        }
        let raw_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let recorded_length = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let information_length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let location = LbAddr::parse(&bytes[12..18])?;
        let mut impl_use = [0u8; 2];
        impl_use.copy_from_slice(&bytes[18..20]);
        Ok(Self {
            length: raw_len & 0x3FFF_FFFF,
            extent_type: ((raw_len >> 30) & 0b11) as u8,
            recorded_length,
            information_length,
            location,
            implementation_use: impl_use,
        })
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let raw_len = (self.length & 0x3FFF_FFFF) | ((self.extent_type as u32 & 0b11) << 30);
        out[0..4].copy_from_slice(&raw_len.to_le_bytes());
        out[4..8].copy_from_slice(&self.recorded_length.to_le_bytes());
        out[8..12].copy_from_slice(&self.information_length.to_le_bytes());
        out[12..18].copy_from_slice(&self.location.encode());
        out[18..20].copy_from_slice(&self.implementation_use);
        out
    }
}

// ─────────────────────── d-string / OSTA compressed unicode ───────────────────────

/// Decode an OSTA Compressed Unicode `d-string` per UDF 1.02 §2.1.3.
/// First byte is the compression ID (8 = 8-bit per char, 16 = 16-bit
/// BE). Returns the decoded `String`; null bytes truncate.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub fn decode_dstring(payload: &[u8]) -> Result<String> {
    if payload.is_empty() {
        return Ok(String::new());
    }
    match payload[0] {
        0 => Ok(String::new()),
        8 => {
            let mut s = String::with_capacity(payload.len() - 1);
            for &b in &payload[1..] {
                if b == 0 {
                    break;
                }
                s.push(b as char);
            }
            Ok(s)
        }
        16 => {
            let body = &payload[1..];
            if body.len() % 2 != 0 {
                return Err(Error::InvalidUdf("16-bit d-string with odd byte count"));
            }
            let mut s = String::with_capacity(body.len() / 2);
            for chunk in body.chunks_exact(2) {
                let cp = u16::from_be_bytes([chunk[0], chunk[1]]);
                if cp == 0 {
                    break;
                }
                if let Some(c) = char::from_u32(cp as u32) {
                    s.push(c);
                }
            }
            Ok(s)
        }
        _ => Err(Error::InvalidUdf("unknown d-string compression id")),
    }
}

/// Decode a fixed-length d-string field; the last byte of the field
/// holds the payload length in bytes.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub fn decode_dstring_field(field: &[u8]) -> Result<String> {
    if field.is_empty() {
        return Ok(String::new());
    }
    let len = *field.last().unwrap() as usize;
    if len > field.len() - 1 {
        return Err(Error::InvalidUdf("d-string length overflows field"));
    }
    decode_dstring(&field[..len])
}

// ─────────────────────── AnchorVolumeDescriptorPointer (§10.2) ───────────────────────

#[derive(Debug, Clone, Copy)]
pub struct AnchorVolumeDescriptorPointer {
    pub tag: DescriptorTag,
    pub main_volume_descriptor_sequence: ExtentAd,
    pub reserve_volume_descriptor_sequence: ExtentAd,
}

impl AnchorVolumeDescriptorPointer {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::AnchorVolumeDescriptorPointer {
            return Err(Error::InvalidUdf("expected AVDP tag"));
        }
        let main = ExtentAd::parse(&bytes[16..24])?;
        let reserve = ExtentAd::parse(&bytes[24..32])?;
        Ok(Self {
            tag,
            main_volume_descriptor_sequence: main,
            reserve_volume_descriptor_sequence: reserve,
        })
    }
}

// ─────────────────────── PrimaryVolumeDescriptor (§10.1) ───────────────────────

#[derive(Debug, Clone)]
pub struct PrimaryVolumeDescriptor {
    pub tag: DescriptorTag,
    pub volume_descriptor_sequence_number: u32,
    pub primary_volume_descriptor_number: u32,
    pub volume_identifier: String,
}

impl PrimaryVolumeDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::PrimaryVolume {
            return Err(Error::InvalidUdf("expected PVD tag"));
        }
        if bytes.len() < 56 {
            return Err(Error::InvalidUdf("PVD truncated"));
        }
        let vds_n = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let pvd_n = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let volume_identifier = decode_dstring_field(&bytes[24..56])?;
        Ok(Self {
            tag,
            volume_descriptor_sequence_number: vds_n,
            primary_volume_descriptor_number: pvd_n,
            volume_identifier,
        })
    }
}

// ─────────────────────── PartitionDescriptor (§10.5) ───────────────────────

#[derive(Debug, Clone)]
pub struct PartitionDescriptor {
    pub tag: DescriptorTag,
    pub volume_descriptor_sequence_number: u32,
    pub partition_flags: u16,
    pub partition_number: u16,
    pub partition_starting_location: u32,
    pub partition_length: u32,
}

impl PartitionDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::Partition {
            return Err(Error::InvalidUdf("expected PD tag"));
        }
        if bytes.len() < 196 {
            return Err(Error::InvalidUdf("PD truncated"));
        }
        let vds_n = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let part_flags = u16::from_le_bytes([bytes[20], bytes[21]]);
        let part_num = u16::from_le_bytes([bytes[22], bytes[23]]);
        let part_start = u32::from_le_bytes([bytes[188], bytes[189], bytes[190], bytes[191]]);
        let part_len = u32::from_le_bytes([bytes[192], bytes[193], bytes[194], bytes[195]]);
        Ok(Self {
            tag,
            volume_descriptor_sequence_number: vds_n,
            partition_flags: part_flags,
            partition_number: part_num,
            partition_starting_location: part_start,
            partition_length: part_len,
        })
    }
}

// ─────────────────────── LogicalVolumeDescriptor (§10.6) ───────────────────────

#[derive(Debug, Clone)]
pub struct LogicalVolumeDescriptor {
    pub tag: DescriptorTag,
    pub volume_descriptor_sequence_number: u32,
    pub logical_volume_identifier: String,
    pub logical_block_size: u32,
    pub file_set_descriptor_location: LongAd,
}

impl LogicalVolumeDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::LogicalVolume {
            return Err(Error::InvalidUdf("expected LVD tag"));
        }
        if bytes.len() < 440 {
            return Err(Error::InvalidUdf("LVD truncated"));
        }
        let vds_n = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let lvi = decode_dstring_field(&bytes[84..212])?;
        let lbs = u32::from_le_bytes([bytes[212], bytes[213], bytes[214], bytes[215]]);
        let fsd = LongAd::parse(&bytes[248..264])?;
        Ok(Self {
            tag,
            volume_descriptor_sequence_number: vds_n,
            logical_volume_identifier: lvi,
            logical_block_size: lbs,
            file_set_descriptor_location: fsd,
        })
    }
}

// ─────────────────────── LogicalVolumeIntegrityDescriptor (§10.10) ───────────────────────

#[derive(Debug, Clone)]
pub struct LogicalVolumeIntegrityDescriptor {
    pub tag: DescriptorTag,
    pub number_of_partitions: u32,
    pub length_of_implementation_use: u32,
}

impl LogicalVolumeIntegrityDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::LogicalVolumeIntegrity {
            return Err(Error::InvalidUdf("expected LVID tag"));
        }
        if bytes.len() < 80 {
            return Err(Error::InvalidUdf("LVID truncated"));
        }
        // bytes 16..28 — recording date (12 bytes)
        // bytes 28..32 — integrity type
        // bytes 32..40 — next integrity extent
        // bytes 40..72 — logical volume contents use
        let nop = u32::from_le_bytes([bytes[72], bytes[73], bytes[74], bytes[75]]);
        let liu = u32::from_le_bytes([bytes[76], bytes[77], bytes[78], bytes[79]]);
        Ok(Self {
            tag,
            number_of_partitions: nop,
            length_of_implementation_use: liu,
        })
    }
}

// ─────────────────────── FileSetDescriptor (§14.1) ───────────────────────

#[derive(Debug, Clone)]
pub struct FileSetDescriptor {
    pub tag: DescriptorTag,
    pub root_directory_icb: LongAd,
}

impl FileSetDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::FileSet {
            return Err(Error::InvalidUdf("expected FSD tag"));
        }
        if bytes.len() < 416 {
            return Err(Error::InvalidUdf("FSD truncated"));
        }
        let root = LongAd::parse(&bytes[400..416])?;
        Ok(Self {
            tag,
            root_directory_icb: root,
        })
    }
}

// ─────────────────────── FileIdentifierDescriptor (§14.4) ───────────────────────

#[derive(Debug, Clone)]
pub struct FileIdentifierDescriptor {
    pub tag: DescriptorTag,
    pub file_version_number: u16,
    pub file_characteristics: u8,
    pub identifier: String,
    pub icb: LongAd,
    /// Total padded size of the FID record on disc (§14.4.9: round up
    /// to the next 4-byte boundary).
    pub total_size: usize,
}

impl FileIdentifierDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 38 {
            return Err(Error::InvalidUdf("FID truncated"));
        }
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::FileIdentifier {
            return Err(Error::InvalidUdf("expected FID tag"));
        }
        let file_version_number = u16::from_le_bytes([bytes[16], bytes[17]]);
        let file_characteristics = bytes[18];
        let len_fi = bytes[19] as usize;
        let icb = LongAd::parse(&bytes[20..36])?;
        let len_impl_use = u16::from_le_bytes([bytes[36], bytes[37]]) as usize;
        let id_off = 38 + len_impl_use;
        let id_end = id_off + len_fi;
        if bytes.len() < id_end {
            return Err(Error::InvalidUdf("FID identifier overruns buffer"));
        }
        let identifier = decode_dstring(&bytes[id_off..id_end])?;
        let total = id_end.div_ceil(4) * 4;
        Ok(Self {
            tag,
            file_version_number,
            file_characteristics,
            identifier,
            icb,
            total_size: total,
        })
    }

    pub fn is_deleted(&self) -> bool {
        self.file_characteristics & 0x04 != 0
    }
    pub fn is_directory(&self) -> bool {
        self.file_characteristics & 0x02 != 0
    }
    pub fn is_parent(&self) -> bool {
        self.file_characteristics & 0x08 != 0
    }
}

// ─────────────────────── FileEntry / ICB (§14.6, §14.9) ───────────────────────

#[derive(Debug, Clone, Copy)]
pub struct IcbTag {
    pub prior_recorded_entries: u32,
    pub strategy_type: u16,
    pub strategy_parameter: u16,
    pub max_entries: u16,
    pub file_type: u8,
    pub parent_icb: LbAddr,
    pub flags: u16,
}

impl IcbTag {
    pub const SIZE: usize = 20;
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::SIZE {
            return Err(Error::InvalidUdf("ICB tag truncated"));
        }
        Ok(Self {
            prior_recorded_entries: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            strategy_type: u16::from_le_bytes([bytes[4], bytes[5]]),
            strategy_parameter: u16::from_le_bytes([bytes[6], bytes[7]]),
            max_entries: u16::from_le_bytes([bytes[8], bytes[9]]),
            file_type: bytes[11],
            parent_icb: LbAddr::parse(&bytes[12..18])?,
            flags: u16::from_le_bytes([bytes[18], bytes[19]]),
        })
    }
}

/// Allocation Descriptor type encoded in `IcbTag::flags & 0b111`
/// (§14.6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdType {
    Short,
    Long,
    Extended,
    EmbeddedInIcb,
}

impl AdType {
    pub fn from_flags(flags: u16) -> Result<Self> {
        match flags & 0b111 {
            0 => Ok(Self::Short),
            1 => Ok(Self::Long),
            2 => Ok(Self::Extended),
            3 => Ok(Self::EmbeddedInIcb),
            _ => Err(Error::InvalidUdf("unknown ad_type")),
        }
    }
}

/// A normalised allocation extent — the unifying type carrying enough
/// information for the high-level disc reader regardless of which AD
/// variant (`short_ad` / `long_ad` / `ext_ad`) was on disc.
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    /// Length in bytes.
    pub length: u32,
    /// In-partition block number.
    pub block_location: u32,
    /// 0 = recorded+allocated, 1 = allocated-not-recorded,
    /// 2 = not-allocated, 3 = AllocationExtentDescriptor continuation.
    pub extent_type: u8,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub tag: DescriptorTag,
    pub icb_tag: IcbTag,
    pub uid: u32,
    pub gid: u32,
    pub permissions: u32,
    pub file_link_count: u16,
    pub record_format: u8,
    pub record_display_attributes: u8,
    pub record_length: u32,
    pub information_length: u64,
    pub logical_blocks_recorded: u64,
    pub length_of_extended_attributes: u32,
    pub length_of_allocation_descriptors: u32,
    pub extents: Vec<Extent>,
    pub embedded_data: Vec<u8>,
    pub ad_type: AdType,
}

impl FileEntry {
    pub const PREFIX_SIZE: usize = 176;

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::PREFIX_SIZE {
            return Err(Error::InvalidUdf("FileEntry truncated"));
        }
        let tag = DescriptorTag::parse(bytes)?;
        if tag.id != TagId::FileEntry {
            // UDF 1.02 only emits plain FE; reject EFE explicitly.
            return Err(Error::InvalidUdf("expected FileEntry tag (no EFE in 1.02)"));
        }
        let icb_tag = IcbTag::parse(&bytes[16..36])?;
        let uid = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
        let gid = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
        let permissions = u32::from_le_bytes([bytes[44], bytes[45], bytes[46], bytes[47]]);
        let flc = u16::from_le_bytes([bytes[48], bytes[49]]);
        let rec_format = bytes[50];
        let rec_disp_attr = bytes[51];
        let rec_len = u32::from_le_bytes([bytes[52], bytes[53], bytes[54], bytes[55]]);
        let info_len = u64::from_le_bytes([
            bytes[56], bytes[57], bytes[58], bytes[59], bytes[60], bytes[61], bytes[62], bytes[63],
        ]);
        let lbr = u64::from_le_bytes([
            bytes[64], bytes[65], bytes[66], bytes[67], bytes[68], bytes[69], bytes[70], bytes[71],
        ]);
        let l_ea = u32::from_le_bytes([bytes[168], bytes[169], bytes[170], bytes[171]]);
        let l_ad = u32::from_le_bytes([bytes[172], bytes[173], bytes[174], bytes[175]]);

        let ad_type = AdType::from_flags(icb_tag.flags)?;

        let ea_off = Self::PREFIX_SIZE;
        let ea_end = ea_off + l_ea as usize;
        let ad_off = ea_end;
        let ad_end = ad_off + l_ad as usize;
        if bytes.len() < ad_end {
            return Err(Error::InvalidUdf("FE allocation area overruns FE"));
        }

        let mut extents = Vec::new();
        let mut embedded_data = Vec::new();
        match ad_type {
            AdType::Short => {
                let mut o = 0;
                while o + ShortAd::SIZE <= l_ad as usize {
                    let ad = ShortAd::parse(&bytes[ad_off + o..ad_off + o + ShortAd::SIZE])?;
                    if ad.length == 0 {
                        break;
                    }
                    if ad.extent_type == 3 {
                        return Err(Error::InvalidUdf(
                            "Allocation Extent Descriptor continuation unsupported",
                        ));
                    }
                    extents.push(Extent {
                        length: ad.length,
                        block_location: ad.block_location,
                        extent_type: ad.extent_type,
                    });
                    o += ShortAd::SIZE;
                }
            }
            AdType::Long => {
                let mut o = 0;
                while o + LongAd::SIZE <= l_ad as usize {
                    let ad = LongAd::parse(&bytes[ad_off + o..ad_off + o + LongAd::SIZE])?;
                    if ad.length == 0 {
                        break;
                    }
                    if ad.extent_type == 3 {
                        return Err(Error::InvalidUdf(
                            "Allocation Extent Descriptor continuation unsupported",
                        ));
                    }
                    extents.push(Extent {
                        length: ad.length,
                        block_location: ad.location.block,
                        extent_type: ad.extent_type,
                    });
                    o += LongAd::SIZE;
                }
            }
            AdType::Extended => {
                let mut o = 0;
                while o + ExtAd::SIZE <= l_ad as usize {
                    let ad = ExtAd::parse(&bytes[ad_off + o..ad_off + o + ExtAd::SIZE])?;
                    if ad.length == 0 {
                        break;
                    }
                    if ad.extent_type == 3 {
                        return Err(Error::InvalidUdf(
                            "Allocation Extent Descriptor continuation unsupported",
                        ));
                    }
                    extents.push(Extent {
                        length: ad.length,
                        block_location: ad.location.block,
                        extent_type: ad.extent_type,
                    });
                    o += ExtAd::SIZE;
                }
            }
            AdType::EmbeddedInIcb => {
                embedded_data.extend_from_slice(&bytes[ad_off..ad_end]);
            }
        }

        Ok(Self {
            tag,
            icb_tag,
            uid,
            gid,
            permissions,
            file_link_count: flc,
            record_format: rec_format,
            record_display_attributes: rec_disp_attr,
            record_length: rec_len,
            information_length: info_len,
            logical_blocks_recorded: lbr,
            length_of_extended_attributes: l_ea,
            length_of_allocation_descriptors: l_ad,
            extents,
            embedded_data,
            ad_type,
        })
    }

    pub fn is_directory(&self) -> bool {
        self.icb_tag.file_type == 4
    }
}

// ─────────────────────── UdfVolume + UdfFile (high-level surface) ───────────────────────

/// Where a file's bytes live on disc, as in-partition extents.
#[derive(Debug, Clone)]
pub struct UdfFile {
    pub name: String,
    pub is_dir: bool,
    pub extents: Vec<Extent>,
    pub length: u64,
    /// Resolved ICB (kept around so a future Phase-2 caller can re-
    /// parse the File Entry for stat-like metadata).
    pub icb: LongAd,
}

/// A mounted UDF 1.02 volume with its file enumeration pre-computed.
pub struct UdfVolume<R: Read + Seek> {
    pub reader: R,
    pub partition_start_sector: u64,
    pub logical_block_size: u32,
    pub volume_identifier: String,
    pub logical_volume_identifier: String,
    pub root_directory_icb: LongAd,
}

impl<R: Read + Seek> std::fmt::Debug for UdfVolume<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdfVolume")
            .field("partition_start_sector", &self.partition_start_sector)
            .field("logical_block_size", &self.logical_block_size)
            .field("volume_identifier", &self.volume_identifier)
            .field("logical_volume_identifier", &self.logical_volume_identifier)
            .field("root_directory_icb", &self.root_directory_icb)
            .finish()
    }
}

impl<R: Read + Seek> UdfVolume<R> {
    /// Mount the volume. Probes AVDP at the conventional candidate
    /// sectors and walks the Volume Descriptor Sequence.
    pub fn open(mut reader: R) -> Result<Self> {
        let avdp = find_avdp(&mut reader)?;
        let main = avdp.main_volume_descriptor_sequence;
        let mut pvd: Option<PrimaryVolumeDescriptor> = None;
        let mut pd: Option<PartitionDescriptor> = None;
        let mut lvd: Option<LogicalVolumeDescriptor> = None;
        let max_sectors = (main.length as u64).div_ceil(SECTOR_SIZE);
        for i in 0..max_sectors {
            let sec = main.location as u64 + i;
            let buf = read_sector(&mut reader, sec)?;
            let id_raw = u16::from_le_bytes([buf[0], buf[1]]);
            let tag_id = match TagId::from_raw(id_raw) {
                Some(t) => t,
                None => continue,
            };
            match tag_id {
                TagId::PrimaryVolume => pvd = Some(PrimaryVolumeDescriptor::parse(&buf)?),
                TagId::Partition => pd = Some(PartitionDescriptor::parse(&buf)?),
                TagId::LogicalVolume => lvd = Some(LogicalVolumeDescriptor::parse(&buf)?),
                TagId::Terminating => break,
                _ => {}
            }
        }
        let pvd = pvd.ok_or(Error::InvalidUdf("no Primary Volume Descriptor"))?;
        let pd = pd.ok_or(Error::InvalidUdf("no Partition Descriptor"))?;
        let lvd = lvd.ok_or(Error::InvalidUdf("no Logical Volume Descriptor"))?;

        if lvd.logical_block_size as u64 != SECTOR_SIZE {
            return Err(Error::InvalidUdf(
                "logical_block_size != 2048 (DVD mandates 2048)",
            ));
        }
        if lvd.file_set_descriptor_location.location.partition_ref != pd.partition_number {
            return Err(Error::InvalidUdf("FSD references non-default partition"));
        }

        let fsd_sec = pd.partition_starting_location as u64
            + lvd.file_set_descriptor_location.location.block as u64;
        let fsd_buf = read_sector_into_vec(&mut reader, fsd_sec)?;
        let fsd = FileSetDescriptor::parse(&fsd_buf)?;

        Ok(Self {
            reader,
            partition_start_sector: pd.partition_starting_location as u64,
            logical_block_size: lvd.logical_block_size,
            volume_identifier: pvd.volume_identifier,
            logical_volume_identifier: lvd.logical_volume_identifier,
            root_directory_icb: fsd.root_directory_icb,
        })
    }

    /// Read a logical block from the partition (absolute sector =
    /// `partition_start_sector + block`).
    pub fn read_partition_block(&mut self, partition_block: u64) -> Result<Vec<u8>> {
        let sec = self.partition_start_sector + partition_block;
        read_sector_into_vec(&mut self.reader, sec)
    }

    /// Read the File Entry at the given ICB location.
    pub fn read_file_entry(&mut self, icb: LongAd) -> Result<FileEntry> {
        if icb.length == 0 {
            return Err(Error::InvalidUdf("FE ICB length 0"));
        }
        let buf = self.read_partition_block(icb.location.block as u64)?;
        FileEntry::parse(&buf)
    }

    /// Materialise the bytes of a file via its ICB.
    pub fn read_file(&mut self, icb: LongAd) -> Result<Vec<u8>> {
        let fe = self.read_file_entry(icb)?;
        let want = fe.information_length as usize;
        if fe.ad_type == AdType::EmbeddedInIcb {
            return Ok(fe.embedded_data[..want.min(fe.embedded_data.len())].to_vec());
        }
        let mut out = Vec::with_capacity(want);
        for ad in &fe.extents {
            if ad.extent_type != 0 {
                return Err(Error::InvalidUdf("non-recorded extent in file"));
            }
            let blocks = (ad.length as u64).div_ceil(SECTOR_SIZE);
            for i in 0..blocks {
                let buf = self.read_partition_block(ad.block_location as u64 + i)?;
                let to_copy =
                    (ad.length as usize).saturating_sub(i as usize * SECTOR_SIZE as usize);
                let take = to_copy.min(SECTOR_SIZE as usize);
                out.extend_from_slice(&buf[..take]);
                if out.len() >= want {
                    break;
                }
            }
            if out.len() >= want {
                break;
            }
        }
        out.truncate(want);
        Ok(out)
    }

    /// List the entries of a directory at `dir_icb`.
    pub fn read_directory(&mut self, dir_icb: LongAd) -> Result<Vec<UdfFile>> {
        let raw = self.read_file(dir_icb)?;
        let mut out = Vec::new();
        let mut o = 0;
        while o + 38 <= raw.len() {
            let fid = FileIdentifierDescriptor::parse(&raw[o..])?;
            let step = fid.total_size;
            if step == 0 {
                break;
            }
            o += step;
            if fid.is_deleted() || fid.is_parent() {
                continue;
            }
            // For each entry, follow the ICB to learn the extents +
            // length. Embedded directories surface zero extents +
            // their length is the embedded payload size — callers
            // who want the FID's child names re-enter via
            // `read_directory(fid.icb)`.
            let fe = self.read_file_entry(fid.icb)?;
            let extents = fe.extents.clone();
            out.push(UdfFile {
                name: fid.identifier.clone(),
                is_dir: fid.is_directory(),
                extents,
                length: fe.information_length,
                icb: fid.icb,
            });
        }
        Ok(out)
    }

    /// Recursively enumerate every regular file under the volume.
    /// Returns `(path, file)`. Directories appear as path prefixes.
    pub fn enumerate(&mut self) -> Result<Vec<(String, UdfFile)>> {
        let mut out = Vec::new();
        let root_icb = self.root_directory_icb;
        self.enumerate_into("", root_icb, &mut out)?;
        Ok(out)
    }

    fn enumerate_into(
        &mut self,
        prefix: &str,
        dir_icb: LongAd,
        out: &mut Vec<(String, UdfFile)>,
    ) -> Result<()> {
        let entries = self.read_directory(dir_icb)?;
        for entry in entries {
            let p = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if entry.is_dir {
                let icb = entry.icb;
                out.push((p.clone(), entry));
                self.enumerate_into(&p, icb, out)?;
            } else {
                out.push((p, entry));
            }
        }
        Ok(())
    }
}

/// Probe the conventional AVDP sector locations (256, 512) for a
/// well-formed AVDP. Returns the first valid one.
// internal — exposed for tests/fuzz; not part of the stable API (use `UdfVolume::mount`)
#[doc(hidden)]
pub fn find_avdp<R: Read + Seek>(reader: &mut R) -> Result<AnchorVolumeDescriptorPointer> {
    for sec in [AVDP_SECTOR_PRIMARY, AVDP_SECTOR_SECONDARY] {
        let Ok(buf) = read_sector(reader, sec) else {
            continue;
        };
        if let Ok(avdp) = AnchorVolumeDescriptorPointer::parse(&buf) {
            return Ok(avdp);
        }
    }
    // Last-resort: try N-256 by extending the file to discover its size.
    if let Ok(end) = reader.seek(SeekFrom::End(0)) {
        let total_sectors = end / SECTOR_SIZE;
        if total_sectors > 256 {
            let sec = total_sectors - 256;
            if let Ok(buf) = read_sector(reader, sec) {
                if let Ok(avdp) = AnchorVolumeDescriptorPointer::parse(&buf) {
                    return Ok(avdp);
                }
            }
        }
    }
    Err(Error::InvalidUdf(
        "no Anchor Volume Descriptor Pointer found",
    ))
}

// ─────────────────────── sector helpers ───────────────────────

fn read_sector<R: Read + Seek>(r: &mut R, sector: u64) -> Result<[u8; SECTOR_SIZE as usize]> {
    r.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
    let mut buf = [0u8; SECTOR_SIZE as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_sector_into_vec<R: Read + Seek>(r: &mut R, sector: u64) -> Result<Vec<u8>> {
    Ok(read_sector(r, sector)?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_tag_round_trip() {
        let tag = DescriptorTag {
            id: TagId::FileSet,
            descriptor_version: 2,
            serial_number: 0x1234,
            crc: 0xABCD,
            crc_length: 200,
            location: 0xDEAD_BEEF,
        };
        let bytes = tag.encode();
        let parsed = DescriptorTag::parse(&bytes).unwrap();
        assert_eq!(parsed.id, TagId::FileSet);
        assert_eq!(parsed.descriptor_version, 2);
        assert_eq!(parsed.serial_number, 0x1234);
        assert_eq!(parsed.location, 0xDEAD_BEEF);
    }

    #[test]
    fn tag_checksum_detects_corruption() {
        let tag = DescriptorTag {
            id: TagId::FileEntry,
            descriptor_version: 2,
            serial_number: 1,
            crc: 0,
            crc_length: 0,
            location: 100,
        };
        let mut bytes = tag.encode();
        bytes[12] ^= 0xFF;
        assert!(matches!(
            DescriptorTag::parse(&bytes),
            Err(Error::InvalidUdf(_))
        ));
    }

    #[test]
    fn lb_addr_round_trip() {
        let a = LbAddr {
            block: 0x12345678,
            partition_ref: 7,
        };
        assert_eq!(LbAddr::parse(&a.encode()).unwrap(), a);
    }

    #[test]
    fn short_ad_packs_extent_type() {
        let ad = ShortAd {
            length: 0x12345678 & 0x3FFF_FFFF,
            extent_type: 2,
            block_location: 99,
        };
        let parsed = ShortAd::parse(&ad.encode()).unwrap();
        assert_eq!(parsed.length, ad.length);
        assert_eq!(parsed.extent_type, ad.extent_type);
        assert_eq!(parsed.block_location, ad.block_location);
    }

    #[test]
    fn long_ad_round_trip() {
        let a = LongAd {
            length: 4096,
            extent_type: 0,
            location: LbAddr {
                block: 12,
                partition_ref: 0,
            },
            implementation_use: [0xAA; 6],
        };
        let parsed = LongAd::parse(&a.encode()).unwrap();
        assert_eq!(parsed, a);
    }

    #[test]
    fn ext_ad_round_trip() {
        let a = ExtAd {
            length: 2048,
            extent_type: 0,
            recorded_length: 2048,
            information_length: 2048,
            location: LbAddr {
                block: 77,
                partition_ref: 0,
            },
            implementation_use: [0xCC, 0xDD],
        };
        let parsed = ExtAd::parse(&a.encode()).unwrap();
        assert_eq!(parsed, a);
    }

    #[test]
    fn dstring_8bit() {
        let payload = b"\x08VIDEO_TS";
        assert_eq!(decode_dstring(payload).unwrap(), "VIDEO_TS");
    }

    #[test]
    fn dstring_16bit_be() {
        let mut payload = vec![16u8];
        for c in "DVD".chars() {
            let v = c as u16;
            payload.push((v >> 8) as u8);
            payload.push(v as u8);
        }
        assert_eq!(decode_dstring(&payload).unwrap(), "DVD");
    }

    #[test]
    fn ad_type_from_flags_table() {
        assert_eq!(AdType::from_flags(0).unwrap(), AdType::Short);
        assert_eq!(AdType::from_flags(1).unwrap(), AdType::Long);
        assert_eq!(AdType::from_flags(2).unwrap(), AdType::Extended);
        assert_eq!(AdType::from_flags(3).unwrap(), AdType::EmbeddedInIcb);
        assert_eq!(AdType::from_flags(0xFFF8).unwrap(), AdType::Short);
    }

    #[test]
    fn lvid_parses_partition_count() {
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        // Construct a tag with id=9 (LVID), version=2, location=200.
        let tag = DescriptorTag {
            id: TagId::LogicalVolumeIntegrity,
            descriptor_version: 2,
            serial_number: 0,
            crc: 0,
            crc_length: 0,
            location: 200,
        };
        buf[..16].copy_from_slice(&tag.encode());
        // number_of_partitions at offset 72; lvid_use stub at 76.
        buf[72..76].copy_from_slice(&1u32.to_le_bytes());
        buf[76..80].copy_from_slice(&0u32.to_le_bytes());
        let lvid = LogicalVolumeIntegrityDescriptor::parse(&buf).unwrap();
        assert_eq!(lvid.number_of_partitions, 1);
    }
}
