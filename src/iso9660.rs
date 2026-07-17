//! Minimal read-only ISO 9660 (ECMA-119) reader.
//!
//! Scope: just enough to look at the **bridge** layer that every
//! DVD-Video disc carries alongside its UDF 1.02 file system per
//! ECMA-268. We read the Primary Volume Descriptor at sector 16,
//! walk the path table or recurse directory records, and surface
//! file extents so the disc-detection layer can sniff for
//! `VIDEO_TS/VIDEO_TS.IFO` even on systems that don't speak UDF.
//!
//! References: ECMA-119 (= ECMA-268 §9 bridge constraints).
//!
//! ## What's implemented
//!
//! - 2048-byte logical sectors (the DVD-mandated value).
//! - PVD parse at sector 16 — magic `CD001`, volume ID, root
//!   directory record, path-table location.
//! - Directory record decoding (§9.1): name length, file flags,
//!   extent LBA, data length, recursive descent into subdirectories.
//! - A-string + D-string decoding per §7.4 / §7.5 (ASCII subset).
//!
//! ## What's not implemented (out of Phase 1 scope)
//!
//! - Supplementary Volume Descriptors (Joliet / UCS-2 names) — DVD
//!   discs carry one but we don't need it for the VIDEO_TS sniff.
//! - Rock Ridge extensions.
//! - El Torito boot records.
//! - Interleaved files (`file_unit_size != 0`).
//! - Extended Attribute Records inside directory records.

use std::io::{Read, Seek, SeekFrom};

use crate::error::{Error, Result};

/// 2048-byte sector — DVD-Video mandates this value (ECMA-268 §6.1).
// internal — exposed for tests/fuzz; not part of the stable API (use `ifo::DVD_SECTOR`)
#[doc(hidden)]
pub const SECTOR_SIZE: u64 = 2048;
/// The Primary Volume Descriptor lives at LBA 16 per ECMA-119 §6.7.1.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub const PVD_SECTOR: u64 = 16;
/// 5-byte magic at offset 1 of every Volume Descriptor (§8.1.2).
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub const STANDARD_ID: &[u8; 5] = b"CD001";

/// Volume Descriptor types (ECMA-119 §8.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VolumeDescriptorType {
    Boot = 0,
    Primary = 1,
    Supplementary = 2,
    Partition = 3,
    Terminator = 255,
}

impl VolumeDescriptorType {
    pub fn from_raw(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Boot,
            1 => Self::Primary,
            2 => Self::Supplementary,
            3 => Self::Partition,
            255 => Self::Terminator,
            _ => return None,
        })
    }
}

/// File-flags bits in a Directory Record (§9.1.6).
pub mod file_flags {
    pub const HIDDEN: u8 = 1 << 0;
    pub const DIRECTORY: u8 = 1 << 1;
    pub const ASSOCIATED: u8 = 1 << 2;
    pub const RECORD: u8 = 1 << 3;
    pub const PROTECTION: u8 = 1 << 4;
    // bits 5..6 reserved
    pub const MULTI_EXTENT: u8 = 1 << 7;
}

/// Read both halves of a "both-endian" u32 (§7.3.3) — the 8-byte
/// field stores little-endian followed by big-endian. We trust the
/// little-endian copy and validate that the big-endian copy matches.
fn read_both_le_u32(bytes: &[u8]) -> Result<u32> {
    if bytes.len() < 8 {
        return Err(Error::InvalidIso9660("both-endian u32 field truncated"));
    }
    let le = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let be = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if le != be {
        return Err(Error::InvalidIso9660("both-endian u32 mismatch"));
    }
    Ok(le)
}

/// Read both halves of a "both-endian" u16 (§7.2.3).
fn read_both_le_u16(bytes: &[u8]) -> Result<u16> {
    if bytes.len() < 4 {
        return Err(Error::InvalidIso9660("both-endian u16 field truncated"));
    }
    let le = u16::from_le_bytes([bytes[0], bytes[1]]);
    let be = u16::from_be_bytes([bytes[2], bytes[3]]);
    if le != be {
        return Err(Error::InvalidIso9660("both-endian u16 mismatch"));
    }
    Ok(le)
}

/// Decode an `a-string` field per §7.4. The character set is the
/// printable ASCII subset (0x20..=0x7E) with trailing 0x20 padding
/// stripped.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub fn decode_a_string(field: &[u8]) -> String {
    decode_ascii_with_pad(field)
}

/// Decode a `d-string` field per §7.5. The d-character subset is
/// `A-Z 0-9 _` plus space-padding to the field length.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub fn decode_d_string(field: &[u8]) -> String {
    decode_ascii_with_pad(field)
}

fn decode_ascii_with_pad(field: &[u8]) -> String {
    let mut s = String::with_capacity(field.len());
    for &b in field {
        if (0x20..=0x7E).contains(&b) {
            s.push(b as char);
        }
    }
    // Trim trailing spaces (the canonical pad character).
    while s.ends_with(' ') {
        s.pop();
    }
    s
}

/// Primary Volume Descriptor (§8.4) — only the fields we need.
#[derive(Debug, Clone)]
pub struct PrimaryVolumeDescriptor {
    pub volume_id: String,
    pub system_id: String,
    pub set_id: String,
    pub publisher_id: String,
    pub volume_space_size: u32, // total sectors on the volume
    pub root_record: DirectoryRecord,
    pub path_table_size: u32,    // bytes
    pub l_path_table_lba: u32,   // LE path table LBA
    pub m_path_table_lba: u32,   // BE path table LBA
    pub logical_block_size: u16, // bytes — always 2048 on DVD
}

impl PrimaryVolumeDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < (SECTOR_SIZE as usize) {
            return Err(Error::InvalidIso9660("PVD sector truncated"));
        }
        // §8.1: byte 0 = vol-desc type, bytes 1..6 = "CD001",
        //       byte 6 = vol-desc version (=1 for ECMA-119).
        if bytes[0] != VolumeDescriptorType::Primary as u8 {
            return Err(Error::InvalidIso9660("PVD type byte != 1"));
        }
        if &bytes[1..6] != STANDARD_ID {
            return Err(Error::InvalidIso9660("PVD standard ID != CD001"));
        }
        if bytes[6] != 1 {
            return Err(Error::InvalidIso9660("PVD version != 1"));
        }
        // byte 7 — reserved (unused field 1) — should be 0
        // bytes 8..40 — system identifier (a-string, 32 bytes)
        let system_id = decode_a_string(&bytes[8..40]);
        // bytes 40..72 — volume identifier (d-string, 32 bytes)
        let volume_id = decode_d_string(&bytes[40..72]);
        // bytes 72..80 — unused field 2
        // bytes 80..88 — volume space size (both-endian u32, in sectors)
        let volume_space_size = read_both_le_u32(&bytes[80..88])?;
        // bytes 88..120 — unused field 3 (32 bytes)
        // bytes 120..124 — volume set size (both-endian u16)
        // bytes 124..128 — volume sequence number (both-endian u16)
        // bytes 128..132 — logical block size (both-endian u16)
        let logical_block_size = read_both_le_u16(&bytes[128..132])?;
        // bytes 132..140 — path table size (both-endian u32, bytes)
        let path_table_size = read_both_le_u32(&bytes[132..140])?;
        // bytes 140..144 — L Path Table location (LE u32)
        let l_path_table_lba = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]);
        // bytes 144..148 — Optional L Path Table location (LE u32) (skipped)
        // bytes 148..152 — M Path Table location (BE u32)
        let m_path_table_lba = u32::from_be_bytes([bytes[148], bytes[149], bytes[150], bytes[151]]);
        // bytes 152..156 — Optional M Path Table location (BE u32) (skipped)
        // bytes 156..190 — Root directory record (34 bytes embedded)
        let root_record = DirectoryRecord::parse(&bytes[156..156 + 34])?;
        // bytes 190..318 — volume set identifier (d-string, 128 bytes)
        let set_id = decode_d_string(&bytes[190..318]);
        // bytes 318..446 — publisher identifier (a-string, 128 bytes)
        let publisher_id = decode_a_string(&bytes[318..446]);
        // bytes 446..574 — data preparer identifier (skipped)
        // bytes 574..702 — application identifier (skipped)
        // bytes 702..739 — copyright file id (skipped)
        // bytes 739..776 — abstract file id (skipped)
        // bytes 776..813 — bibliographic file id (skipped)
        // bytes 813..830 — vol creation date/time (skipped)
        // ... rest of the PVD is decoration we don't need.
        Ok(Self {
            volume_id,
            system_id,
            set_id,
            publisher_id,
            volume_space_size,
            root_record,
            path_table_size,
            l_path_table_lba,
            m_path_table_lba,
            logical_block_size,
        })
    }
}

/// A single Directory Record (§9.1).
#[derive(Debug, Clone)]
pub struct DirectoryRecord {
    /// Length of this record (1..=255). 0 means "skip to next sector".
    pub length: u8,
    pub extended_attribute_record_length: u8,
    pub extent_lba: u32,
    pub data_length: u32, // bytes
    pub file_flags: u8,
    pub file_unit_size: u8,
    pub interleave_gap_size: u8,
    /// The Volume Sequence Number; should be 1 for the only volume.
    pub volume_sequence_number: u16,
    /// File identifier — decoded. Length is at offset 32.
    pub identifier: String,
}

impl DirectoryRecord {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(Error::InvalidIso9660("directory record empty"));
        }
        let length = bytes[0];
        if length == 0 {
            return Err(Error::InvalidIso9660("directory record length 0"));
        }
        if (length as usize) > bytes.len() {
            return Err(Error::InvalidIso9660("directory record overruns buffer"));
        }
        if length < 33 {
            return Err(Error::InvalidIso9660("directory record < 33 bytes"));
        }
        let ear_len = bytes[1];
        let extent_lba = read_both_le_u32(&bytes[2..10])?;
        let data_length = read_both_le_u32(&bytes[10..18])?;
        // bytes 18..25 — recording date and time (7 bytes, skipped)
        let file_flags = bytes[25];
        let file_unit_size = bytes[26];
        let interleave_gap_size = bytes[27];
        let volume_sequence_number = read_both_le_u16(&bytes[28..32])?;
        let lfi = bytes[32] as usize;
        if 33 + lfi > length as usize {
            return Err(Error::InvalidIso9660(
                "directory record name overruns record length",
            ));
        }
        let name_field = &bytes[33..33 + lfi];
        let identifier = decode_file_identifier(name_field);
        Ok(Self {
            length,
            extended_attribute_record_length: ear_len,
            extent_lba,
            data_length,
            file_flags,
            file_unit_size,
            interleave_gap_size,
            volume_sequence_number,
            identifier,
        })
    }

    pub fn is_dir(&self) -> bool {
        self.file_flags & file_flags::DIRECTORY != 0
    }

    /// True iff this record points at `.` (the "current directory" pseudo-entry).
    pub fn is_self(&self) -> bool {
        self.identifier == "."
    }

    /// True iff this record points at `..` (the parent pseudo-entry).
    pub fn is_parent(&self) -> bool {
        self.identifier == ".."
    }
}

/// Decode a file identifier byte sequence. The two special pseudo-
/// entries (`\x00` = `.`, `\x01` = `..`) are surfaced as `.` / `..`
/// so downstream code can match them by string. Otherwise we strip
/// the optional `;1` version suffix that ECMA-119 §7.5.1 always
/// requires on file (not directory) identifiers.
fn decode_file_identifier(bytes: &[u8]) -> String {
    match bytes {
        [0x00] => ".".to_string(),
        [0x01] => "..".to_string(),
        _ => {
            let s = decode_d_string(bytes);
            // Strip a trailing `;1` (or any `;N`) — DVD authoring tools
            // sometimes also emit `;`-less names; tolerate both.
            if let Some(semi) = s.rfind(';') {
                s[..semi].to_string()
            } else {
                s
            }
        }
    }
}

/// A walked file or directory entry produced by [`Iso9660Volume::list_dir`].
#[derive(Debug, Clone)]
pub struct Iso9660Entry {
    pub name: String,
    pub is_dir: bool,
    pub lba: u32,
    pub size: u32,
}

/// A successfully parsed ISO 9660 volume. Held alongside the reader
/// so directory walks can pull more sectors lazily.
pub struct Iso9660Volume<R: Read + Seek> {
    pub reader: R,
    pub pvd: PrimaryVolumeDescriptor,
}

impl<R: Read + Seek> std::fmt::Debug for Iso9660Volume<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Iso9660Volume")
            .field("pvd", &self.pvd)
            .finish()
    }
}

impl<R: Read + Seek> Iso9660Volume<R> {
    /// Open the volume by reading the PVD at sector 16.
    pub fn open(mut reader: R) -> Result<Self> {
        let mut buf = [0u8; SECTOR_SIZE as usize];
        reader.seek(SeekFrom::Start(PVD_SECTOR * SECTOR_SIZE))?;
        reader.read_exact(&mut buf)?;
        let pvd = PrimaryVolumeDescriptor::parse(&buf)?;
        if pvd.logical_block_size as u64 != SECTOR_SIZE {
            return Err(Error::InvalidIso9660(
                "logical block size != 2048 (only DVD-mandated 2048 supported)",
            ));
        }
        Ok(Self { reader, pvd })
    }

    /// List entries under the root directory.
    pub fn list_root(&mut self) -> Result<Vec<Iso9660Entry>> {
        self.list_dir(
            self.pvd.root_record.extent_lba,
            self.pvd.root_record.data_length,
        )
    }

    /// List entries under a specific directory extent.
    pub fn list_dir(&mut self, lba: u32, size: u32) -> Result<Vec<Iso9660Entry>> {
        let bytes = self.read_extent(lba, size)?;
        let mut entries = Vec::new();
        let mut o = 0usize;
        while o < bytes.len() {
            // §6.8.1.1: a length byte of 0 means "skip to start of
            // next logical sector".
            if bytes[o] == 0 {
                let next = ((o / SECTOR_SIZE as usize) + 1) * SECTOR_SIZE as usize;
                if next >= bytes.len() {
                    break;
                }
                o = next;
                continue;
            }
            let rec = DirectoryRecord::parse(&bytes[o..])?;
            let rec_len = rec.length as usize;
            // Skip `.` and `..` pseudo-entries.
            if !rec.is_self() && !rec.is_parent() {
                entries.push(Iso9660Entry {
                    name: rec.identifier.clone(),
                    is_dir: rec.is_dir(),
                    lba: rec.extent_lba,
                    size: rec.data_length,
                });
            }
            o += rec_len;
        }
        Ok(entries)
    }

    /// Read the raw bytes of an extent at the given LBA.
    pub fn read_extent(&mut self, lba: u32, size: u32) -> Result<Vec<u8>> {
        let n_sectors = (size as u64).div_ceil(SECTOR_SIZE);
        self.reader
            .seek(SeekFrom::Start(lba as u64 * SECTOR_SIZE))?;
        let mut buf = vec![0u8; (n_sectors * SECTOR_SIZE) as usize];
        self.reader.read_exact(&mut buf)?;
        buf.truncate(size as usize);
        Ok(buf)
    }

    /// Walk the L-Path table (§9.4) — a flat list of directory paths
    /// rooted at the volume's root. Useful for catching missing
    /// `VIDEO_TS` without recursing the full directory tree.
    pub fn walk_l_path_table(&mut self) -> Result<Vec<PathTableEntry>> {
        let table_bytes = self.read_extent(self.pvd.l_path_table_lba, self.pvd.path_table_size)?;
        parse_l_path_table(&table_bytes)
    }
}

/// A single L-Path table entry (§9.4.1).
#[derive(Debug, Clone)]
pub struct PathTableEntry {
    pub name: String,
    pub extent_lba: u32,
    pub parent_dir_number: u16,
}

/// Parse the L-Path table (little-endian) byte buffer.
// internal — exposed for tests/fuzz; not part of the stable API
#[doc(hidden)]
pub fn parse_l_path_table(bytes: &[u8]) -> Result<Vec<PathTableEntry>> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o + 8 <= bytes.len() {
        let len_di = bytes[o] as usize;
        if len_di == 0 {
            // Padding to end of table.
            break;
        }
        let _ear_len = bytes[o + 1];
        let extent_lba =
            u32::from_le_bytes([bytes[o + 2], bytes[o + 3], bytes[o + 4], bytes[o + 5]]);
        let parent_dir_number = u16::from_le_bytes([bytes[o + 6], bytes[o + 7]]);
        let name_off = o + 8;
        if name_off + len_di > bytes.len() {
            return Err(Error::InvalidIso9660("path-table entry overruns buffer"));
        }
        let name = decode_d_string(&bytes[name_off..name_off + len_di]);
        // Records are aligned to a 2-byte boundary (pad with 0x00).
        let total = 8 + len_di + (len_di & 1);
        out.push(PathTableEntry {
            name,
            extent_lba,
            parent_dir_number,
        });
        o += total;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build the 34-byte canonical Directory Record for the root, used
    /// inside the synthetic PVD.
    fn root_dir_record(lba: u32, size: u32) -> Vec<u8> {
        let mut r = vec![0u8; 34];
        r[0] = 34; // record length
        r[1] = 0; // EAR length
                  // extent LBA (both-endian)
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[6..10].copy_from_slice(&lba.to_be_bytes());
        // data length (both-endian)
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        // recording date/time at 18..25 — zeros are fine for test
        r[25] = file_flags::DIRECTORY;
        r[26] = 0; // file_unit_size
        r[27] = 0; // interleave gap
                   // volume sequence number = 1 (both-endian u16)
        r[28..30].copy_from_slice(&1u16.to_le_bytes());
        r[30..32].copy_from_slice(&1u16.to_be_bytes());
        r[32] = 1; // identifier length
        r[33] = 0x00; // identifier '.' marker
        r
    }

    fn make_pvd_sector(vol_id: &str, root_lba: u32, root_size: u32) -> Vec<u8> {
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        buf[0] = VolumeDescriptorType::Primary as u8;
        buf[1..6].copy_from_slice(STANDARD_ID);
        buf[6] = 1; // PVD version
                    // bytes 8..40 system identifier — leave as spaces
        for b in &mut buf[8..40] {
            *b = 0x20;
        }
        // bytes 40..72 volume identifier
        for b in &mut buf[40..72] {
            *b = 0x20;
        }
        let vid_bytes = vol_id.as_bytes();
        buf[40..40 + vid_bytes.len()].copy_from_slice(vid_bytes);
        // volume space size (sectors) = 1000 (both-endian)
        buf[80..84].copy_from_slice(&1000u32.to_le_bytes());
        buf[84..88].copy_from_slice(&1000u32.to_be_bytes());
        // logical block size = 2048
        buf[128..130].copy_from_slice(&2048u16.to_le_bytes());
        buf[130..132].copy_from_slice(&2048u16.to_be_bytes());
        // path table size = 64 (bytes, both-endian)
        buf[132..136].copy_from_slice(&64u32.to_le_bytes());
        buf[136..140].copy_from_slice(&64u32.to_be_bytes());
        // L path table LBA = 20
        buf[140..144].copy_from_slice(&20u32.to_le_bytes());
        // optional L path table LBA = 0
        // M path table LBA = 21
        buf[148..152].copy_from_slice(&21u32.to_be_bytes());
        // root directory record at 156..190
        buf[156..190].copy_from_slice(&root_dir_record(root_lba, root_size));
        // volume set identifier
        for b in &mut buf[190..318] {
            *b = 0x20;
        }
        // publisher id
        for b in &mut buf[318..446] {
            *b = 0x20;
        }
        buf
    }

    #[test]
    fn pvd_parses_volume_id() {
        let sector = make_pvd_sector("TESTDVD", 50, 4096);
        let pvd = PrimaryVolumeDescriptor::parse(&sector).unwrap();
        assert_eq!(pvd.volume_id, "TESTDVD");
        assert_eq!(pvd.volume_space_size, 1000);
        assert_eq!(pvd.logical_block_size, 2048);
        assert_eq!(pvd.root_record.extent_lba, 50);
        assert_eq!(pvd.root_record.data_length, 4096);
        assert!(pvd.root_record.is_dir());
    }

    #[test]
    fn pvd_rejects_wrong_magic() {
        let mut sector = make_pvd_sector("X", 50, 4096);
        sector[1..6].copy_from_slice(b"BADID");
        assert!(matches!(
            PrimaryVolumeDescriptor::parse(&sector),
            Err(Error::InvalidIso9660(_))
        ));
    }

    #[test]
    fn d_string_strips_trailing_spaces() {
        assert_eq!(decode_d_string(b"HELLO   "), "HELLO");
        assert_eq!(decode_d_string(b"        "), "");
        assert_eq!(decode_d_string(b""), "");
    }

    /// A short L-Path-table buffer with two entries: root (`\x00`) +
    /// `VIDEO_TS`.
    fn make_path_table() -> Vec<u8> {
        let mut t = Vec::new();
        // root entry: name len 1, EAR=0, extent LBA=50, parent=1, name=\x00
        t.push(1); // len_di
        t.push(0); // ear
        t.extend_from_slice(&50u32.to_le_bytes()); // LBA LE
        t.extend_from_slice(&1u16.to_le_bytes()); // parent
        t.push(0x00); // identifier byte
        t.push(0x00); // pad to even
                      // VIDEO_TS entry: name len 8, EAR=0, extent LBA=100, parent=1
        t.push(8);
        t.push(0);
        t.extend_from_slice(&100u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(b"VIDEO_TS");
        t
    }

    #[test]
    fn path_table_decodes_root_plus_video_ts() {
        let table = make_path_table();
        let entries = parse_l_path_table(&table).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].extent_lba, 50);
        assert_eq!(entries[1].name, "VIDEO_TS");
        assert_eq!(entries[1].extent_lba, 100);
    }

    /// Build an in-memory directory extent containing `.`, `..`, and
    /// two children: a file `INDEX.DAT;1` and a directory `SUB`.
    fn make_dir_extent() -> Vec<u8> {
        fn record(name_bytes: &[u8], lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
            let len_di = name_bytes.len();
            let rec_len = 33 + len_di + (1 - (len_di & 1)); // round up to even
            let mut r = vec![0u8; rec_len];
            r[0] = rec_len as u8;
            r[1] = 0; // EAR
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[6..10].copy_from_slice(&lba.to_be_bytes());
            r[10..14].copy_from_slice(&size.to_le_bytes());
            r[14..18].copy_from_slice(&size.to_be_bytes());
            r[25] = if is_dir { file_flags::DIRECTORY } else { 0 };
            r[28..30].copy_from_slice(&1u16.to_le_bytes());
            r[30..32].copy_from_slice(&1u16.to_be_bytes());
            r[32] = len_di as u8;
            r[33..33 + len_di].copy_from_slice(name_bytes);
            r
        }
        let mut out = Vec::new();
        out.extend_from_slice(&record(&[0x00], 0, 0, true)); // '.'
        out.extend_from_slice(&record(&[0x01], 0, 0, true)); // '..'
        out.extend_from_slice(&record(b"INDEX.DAT;1", 200, 4096, false));
        out.extend_from_slice(&record(b"SUB", 300, 2048, true));
        // Pad to 2048 sectors.
        out.resize(SECTOR_SIZE as usize, 0);
        out
    }

    #[test]
    fn dir_extent_walk_emits_two_entries() {
        let mut bytes = vec![0u8; 100 * SECTOR_SIZE as usize];
        let dir = make_dir_extent();
        // Put the directory at LBA 50.
        bytes[50 * SECTOR_SIZE as usize..50 * SECTOR_SIZE as usize + dir.len()]
            .copy_from_slice(&dir);
        // Write a PVD at sector 16.
        let pvd = make_pvd_sector("DIR_WALK_TEST", 50, dir.len() as u32);
        bytes[16 * SECTOR_SIZE as usize..16 * SECTOR_SIZE as usize + pvd.len()]
            .copy_from_slice(&pvd);
        let mut vol = Iso9660Volume::open(Cursor::new(bytes)).unwrap();
        let listing = vol.list_root().unwrap();
        assert_eq!(listing.len(), 2);
        let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"INDEX.DAT"));
        assert!(names.contains(&"SUB"));
    }

    #[test]
    fn dir_record_zero_length_skips_to_next_sector() {
        // Two records, then a zero-length byte triggering a jump to
        // the next sector boundary. The second sector holds one more
        // record.
        let mut bytes = vec![0u8; 100 * SECTOR_SIZE as usize];
        let mut dir = Vec::new();
        fn record(name: &[u8], lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
            let len_di = name.len();
            let rec_len = 33 + len_di + (1 - (len_di & 1));
            let mut r = vec![0u8; rec_len];
            r[0] = rec_len as u8;
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[6..10].copy_from_slice(&lba.to_be_bytes());
            r[10..14].copy_from_slice(&size.to_le_bytes());
            r[14..18].copy_from_slice(&size.to_be_bytes());
            r[25] = if is_dir { file_flags::DIRECTORY } else { 0 };
            r[28..30].copy_from_slice(&1u16.to_le_bytes());
            r[30..32].copy_from_slice(&1u16.to_be_bytes());
            r[32] = len_di as u8;
            r[33..33 + len_di].copy_from_slice(name);
            r
        }
        dir.extend_from_slice(&record(&[0x00], 0, 0, true));
        dir.extend_from_slice(&record(&[0x01], 0, 0, true));
        dir.extend_from_slice(&record(b"AAA", 100, 1000, false));
        // pad remainder of first sector with zeros (a zero length-byte
        // is the signal to skip to the next sector boundary).
        dir.resize(SECTOR_SIZE as usize, 0);
        // Second sector — one entry.
        dir.extend_from_slice(&record(b"BBB", 200, 2000, false));
        dir.resize(2 * SECTOR_SIZE as usize, 0);

        bytes[60 * SECTOR_SIZE as usize..60 * SECTOR_SIZE as usize + dir.len()]
            .copy_from_slice(&dir);
        let pvd = make_pvd_sector("ZERO_LEN", 60, dir.len() as u32);
        bytes[16 * SECTOR_SIZE as usize..16 * SECTOR_SIZE as usize + pvd.len()]
            .copy_from_slice(&pvd);
        let mut vol = Iso9660Volume::open(Cursor::new(bytes)).unwrap();
        let listing = vol.list_root().unwrap();
        let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"AAA"));
        assert!(names.contains(&"BBB"));
    }
}
