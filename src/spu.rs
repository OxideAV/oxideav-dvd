//! DVD Sub-Picture Unit (SPU) decoder.
//!
//! A DVD sub-picture is the overlay graphics stream that carries
//! subtitles, menu button highlights, karaoke captions — anything
//! drawn on top of the MPEG-2 video. Each Sub-Picture Unit (SPU)
//! is a self-contained blob assembled from one or more PES packets
//! routed through DVD substream `0x20..=0x3F` (`DvdSubstream::Subpicture`
//! in [`crate::vob`]).
//!
//! Per `docs/container/dvd/application/mpucoder-spu.html`, the unit's
//! internal layout is:
//!
//! ```text
//! | SPUH | PXDtf | PXDbf | SP_DCSQT |
//! ```
//!
//! - **SPUH** — Sub-Picture Unit Header (4 bytes total): `SPDSZ` =
//!   total SPU size, `SP_DCSQTA` = offset to the SP_DCSQT.
//! - **PXDtf / PXDbf** — RLE-compressed pixel data for the top
//!   field (lines 1, 3, 5, …) and the bottom field (lines 2, 4, 6,
//!   …) respectively. Pixel codes are 2 bits each (`background`,
//!   `pattern`, `emphasis 1`, `emphasis 2`).
//! - **SP_DCSQT** — Sub-Picture Display Control Sequence Table, a
//!   chain of `SP_DCSQ` blocks, each with a 90 kHz/1024 start-time
//!   delay + a pointer to the next block, then a stream of one-byte
//!   commands ending in `0xFF` (`CMD_END`).
//!
//! This module exposes:
//! - [`SpuHeader`] — the 4-byte SPUH.
//! - [`SpuCommand`] — the typed command enum (FSTA_DSP / STA_DSP /
//!   STP_DSP / SET_COLOR / SET_CONTR / SET_DAREA / SET_DSPXA /
//!   CHG_COLCON / END).
//! - [`SpDcSq`] — one display-control sequence (delay + commands +
//!   pointer to next).
//! - [`SubPictureUnit`] — the parsed unit (header + all DCSQs + the
//!   raw pixel-data slice references).
//! - [`decode_rle_field`] — the 2-bit / 4-form RLE expander that
//!   turns one field's PXDtf or PXDbf bytes into a row-major
//!   sequence of `(count, code)` runs.
//! - [`render_field`] — convenience that drives `decode_rle_field`
//!   to fill a 2D `Vec<u8>` of palette indices for a known
//!   display-area width.
//!
//! Pure-bytes decoder. Producing a final framebuffer (YCrCb +
//! alpha) is left to the caller because it needs the PGC palette
//! ([`crate::ifo::PaletteEntry`]) and the renderer's own pixel
//! format choice — both outside the SPU bitstream itself.
//!
//! ## Clean-room reference
//!
//! `docs/container/dvd/application/mpucoder-spu.html` (160 lines)
//! — sole source for the SPUH layout, the four pixel-data
//! run-length formats, the end-of-line zero-count special case,
//! the four-fill-bits line padding, the eight SP_DCSQ command
//! codes and their operand layouts, and the 90 kHz/1024 delay
//! conversion table.

use crate::error::{Error, Result};

/// One Sub-Picture Unit's 4-byte header.
///
/// Per mpucoder-spu.html §SPUH the header is two 16-bit big-endian
/// words: `SPDSZ` (total SPU size, including the header and any
/// trailing SP_DCSQT padding) and `SP_DCSQTA` (offset within the
/// SPU to the SP_DCSQT). The PXDtf / PXDbf offsets are not stored
/// directly in the header — they are recovered from the SP_DSPXA
/// (`0x06`) command in the first DCSQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpuHeader {
    /// `SPDSZ` — total size in bytes of this SPU (the value at
    /// offset 0 in the unit).
    pub size: u16,
    /// `SP_DCSQTA` — offset within the SPU to the SP_DCSQT.
    pub dcsqt_offset: u16,
}

impl SpuHeader {
    /// Parse the 4-byte SPUH at the start of an SPU buffer.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 {
            return Err(Error::InvalidUdf("SPU header shorter than 4 bytes"));
        }
        Ok(Self {
            size: u16::from_be_bytes([buf[0], buf[1]]),
            dcsqt_offset: u16::from_be_bytes([buf[2], buf[3]]),
        })
    }
}

/// One typed entry of an `SP_DCSQ` command stream.
///
/// Per mpucoder-spu.html §Commands. The first SP_DCSQ of a unit
/// should normally carry `SetColor` + `SetContrast` + `SetDisplayArea`
/// + `SetPixelDataAddresses` before any `StartDisplay` / `EndOfSequence`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpuCommand {
    /// `0x00` — FSTA_DSP, "Forced Start Display". Begin showing
    /// regardless of menu-button gating.
    ForcedStartDisplay,
    /// `0x01` — STA_DSP, "Start Display".
    StartDisplay,
    /// `0x02` — STP_DSP, "Stop Display".
    StopDisplay,
    /// `0x03` — SET_COLOR. Four 4-bit palette indices, one per
    /// pixel value: `(emphasis2, emphasis1, pattern, background)`.
    /// Each index points into the PGC's 16-entry palette
    /// ([`crate::ifo::PaletteEntry`]).
    SetColor {
        /// Palette index for pixel value 3 ("emphasis 2").
        emphasis2: u8,
        /// Palette index for pixel value 2 ("emphasis 1").
        emphasis1: u8,
        /// Palette index for pixel value 1 ("pattern").
        pattern: u8,
        /// Palette index for pixel value 0 ("background").
        background: u8,
    },
    /// `0x04` — SET_CONTR. Four 4-bit alpha values, one per pixel
    /// value: `(emphasis2, emphasis1, pattern, background)`. `0x0`
    /// = fully transparent, `0xF` = fully opaque.
    SetContrast {
        /// Alpha for pixel value 3.
        emphasis2: u8,
        /// Alpha for pixel value 2.
        emphasis1: u8,
        /// Alpha for pixel value 1.
        pattern: u8,
        /// Alpha for pixel value 0.
        background: u8,
    },
    /// `0x05` — SET_DAREA. Three-byte X pair + three-byte Y pair:
    /// `(start_x, end_x, start_y, end_y)`. All four are 12-bit
    /// quantities packed as `sx sx | sx ex | ex ex | sy sy | sy ey | ey ey`.
    SetDisplayArea {
        /// Starting (left-most) X column.
        start_x: u16,
        /// Ending (right-most) X column.
        end_x: u16,
        /// Starting (top-most) Y line.
        start_y: u16,
        /// Ending (bottom-most) Y line.
        end_y: u16,
    },
    /// `0x06` — SET_DSPXA. Offsets within the SPU to the top-field
    /// (PXDtf) and bottom-field (PXDbf) pixel data.
    SetPixelDataAddresses {
        /// Offset to PXDtf (top-field pixel data).
        top_field_offset: u16,
        /// Offset to PXDbf (bottom-field pixel data).
        bottom_field_offset: u16,
    },
    /// `0x07` — CHG_COLCON. Change-color-contrast parameter block;
    /// its body is a hierarchy of `LN_CTLI` / `PX_CTLI` entries
    /// terminated by `0x0FFFFFFF`. The raw parameter bytes (including
    /// the leading 2-byte total-size word) are preserved here for
    /// callers that want to walk the hierarchy themselves.
    ChangeColorContrast {
        /// The full parameter blob, including its leading 2-byte
        /// total-size header word.
        raw: Vec<u8>,
    },
    /// `0xFF` — CMD_END. Marks the end of one SP_DCSQ command list.
    /// Stored as an explicit variant so a parser that walks a
    /// stream can stop on the byte it saw rather than relying on
    /// length-based truncation.
    EndOfSequence,
}

impl SpuCommand {
    /// On-wire opcode byte for this command, exactly as it appears
    /// at the head of the operand stream.
    pub fn opcode(&self) -> u8 {
        match self {
            Self::ForcedStartDisplay => 0x00,
            Self::StartDisplay => 0x01,
            Self::StopDisplay => 0x02,
            Self::SetColor { .. } => 0x03,
            Self::SetContrast { .. } => 0x04,
            Self::SetDisplayArea { .. } => 0x05,
            Self::SetPixelDataAddresses { .. } => 0x06,
            Self::ChangeColorContrast { .. } => 0x07,
            Self::EndOfSequence => 0xFF,
        }
    }
}

/// One `SP_DCSQ` block: a 4-byte header (delay + next-DCSQ pointer)
/// followed by a stream of [`SpuCommand`]s terminated by `0xFF`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpDcSq {
    /// `SP_DCSQ_STM` — delay before executing the commands. Units
    /// are 90 kHz/1024 ticks, so `delay * 1024 / 90` is the delay
    /// in microseconds and `delay * 1024 / 90_000` is the delay in
    /// milliseconds. See [`spdcsq_stm_to_ms`].
    pub start_time: u16,
    /// `SP_NXT_DCSQ_SA` — offset within the SPU to the next
    /// SP_DCSQ. For the terminal block this points back at the
    /// block's own offset.
    pub next_offset: u16,
    /// Decoded commands, ending with [`SpuCommand::EndOfSequence`].
    pub commands: Vec<SpuCommand>,
}

/// Convert an `SP_DCSQ_STM` 90 kHz/1024 delay into milliseconds.
///
/// Per the conversion table in mpucoder-spu.html the relationship is
/// `delay = floor(seconds * 90000 / 1024)`. Inverting that and
/// scaling to integer milliseconds: `ms = floor(delay * 1024 / 90)`.
pub fn spdcsq_stm_to_ms(stm: u16) -> u32 {
    (u32::from(stm) * 1024) / 90
}

/// One fully parsed Sub-Picture Unit.
#[derive(Debug, Clone)]
pub struct SubPictureUnit {
    /// SPUH at the head of the buffer.
    pub header: SpuHeader,
    /// All `SP_DCSQ` blocks, walked in order from `SP_DCSQTA`
    /// onward via the per-block `SP_NXT_DCSQ_SA` pointer.
    pub control_sequences: Vec<SpDcSq>,
}

impl SubPictureUnit {
    /// Parse an SPU from a contiguous byte slice. The slice must
    /// be the full unit (header through SP_DCSQT) — typically the
    /// concatenation of every subpicture PES packet payload for a
    /// given subpicture stream over one display interval.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let header = SpuHeader::parse(buf)?;
        let total = usize::from(header.size);
        let dcsqt_off = usize::from(header.dcsqt_offset);
        if total < 4 || total > buf.len() {
            return Err(Error::InvalidUdf("SPU size exceeds available buffer"));
        }
        if dcsqt_off < 4 || dcsqt_off >= total {
            return Err(Error::InvalidUdf("SPU SP_DCSQTA out of range"));
        }
        let mut control_sequences = Vec::new();
        let mut cursor = dcsqt_off;
        let mut seen_offsets = Vec::new();
        loop {
            if seen_offsets.contains(&cursor) {
                return Err(Error::InvalidUdf("SPU DCSQ chain loops back"));
            }
            seen_offsets.push(cursor);
            let dcsq = parse_dcsq(buf, cursor, total)?;
            let next_off = usize::from(dcsq.next_offset);
            let terminal = next_off == cursor;
            control_sequences.push(dcsq);
            if terminal {
                break;
            }
            // Defensive: next pointer must move forward inside the
            // SP_DCSQT. mpucoder-spu.html doesn't require monotonic
            // ordering, but every real SPU emits the DCSQs in
            // arrival-time order so a backward jump means the slice
            // is truncated or corrupted.
            if next_off <= cursor || next_off >= total {
                return Err(Error::InvalidUdf("SPU DCSQ next pointer out of range"));
            }
            cursor = next_off;
        }
        Ok(Self {
            header,
            control_sequences,
        })
    }

    /// Convenience: locate the `SetPixelDataAddresses` command in
    /// any DCSQ and return the `(top_field_offset, bottom_field_offset)`
    /// pair. Returns `None` if no SPU command set the pair (which
    /// would be a malformed SPU per mpucoder-spu.html — `SET_DSPXA`
    /// is one of the four mandatory commands of the first DCSQ).
    pub fn pixel_data_offsets(&self) -> Option<(u16, u16)> {
        for dcsq in &self.control_sequences {
            for cmd in &dcsq.commands {
                if let SpuCommand::SetPixelDataAddresses {
                    top_field_offset,
                    bottom_field_offset,
                } = cmd
                {
                    return Some((*top_field_offset, *bottom_field_offset));
                }
            }
        }
        None
    }

    /// Convenience: locate the `SetDisplayArea` command and return
    /// `(width, height)` of the rectangle in pixels (inclusive
    /// coordinates → +1 on each axis). `None` if no DCSQ set the
    /// area.
    pub fn display_dimensions(&self) -> Option<(u16, u16)> {
        for dcsq in &self.control_sequences {
            for cmd in &dcsq.commands {
                if let SpuCommand::SetDisplayArea {
                    start_x,
                    end_x,
                    start_y,
                    end_y,
                } = cmd
                {
                    let w = end_x.saturating_sub(*start_x).saturating_add(1);
                    let h = end_y.saturating_sub(*start_y).saturating_add(1);
                    return Some((w, h));
                }
            }
        }
        None
    }
}

fn parse_dcsq(buf: &[u8], off: usize, total: usize) -> Result<SpDcSq> {
    if off + 4 > total {
        return Err(Error::InvalidUdf("SPU DCSQ header truncated"));
    }
    let start_time = u16::from_be_bytes([buf[off], buf[off + 1]]);
    let next_offset = u16::from_be_bytes([buf[off + 2], buf[off + 3]]);
    let mut i = off + 4;
    let mut commands = Vec::new();
    while i < total {
        let opcode = buf[i];
        i += 1;
        let cmd = match opcode {
            0x00 => SpuCommand::ForcedStartDisplay,
            0x01 => SpuCommand::StartDisplay,
            0x02 => SpuCommand::StopDisplay,
            0x03 => {
                if i + 2 > total {
                    return Err(Error::InvalidUdf("SPU SET_COLOR truncated"));
                }
                let b0 = buf[i];
                let b1 = buf[i + 1];
                i += 2;
                SpuCommand::SetColor {
                    emphasis2: b0 >> 4,
                    emphasis1: b0 & 0x0F,
                    pattern: b1 >> 4,
                    background: b1 & 0x0F,
                }
            }
            0x04 => {
                if i + 2 > total {
                    return Err(Error::InvalidUdf("SPU SET_CONTR truncated"));
                }
                let b0 = buf[i];
                let b1 = buf[i + 1];
                i += 2;
                SpuCommand::SetContrast {
                    emphasis2: b0 >> 4,
                    emphasis1: b0 & 0x0F,
                    pattern: b1 >> 4,
                    background: b1 & 0x0F,
                }
            }
            0x05 => {
                if i + 6 > total {
                    return Err(Error::InvalidUdf("SPU SET_DAREA truncated"));
                }
                // Two 24-bit fields:
                //   sx sx | sx ex | ex ex  → start_x:12, end_x:12
                //   sy sy | sy ey | ey ey  → start_y:12, end_y:12
                let xp = u32::from_be_bytes([0, buf[i], buf[i + 1], buf[i + 2]]);
                let yp = u32::from_be_bytes([0, buf[i + 3], buf[i + 4], buf[i + 5]]);
                i += 6;
                let start_x = ((xp >> 12) & 0xFFF) as u16;
                let end_x = (xp & 0xFFF) as u16;
                let start_y = ((yp >> 12) & 0xFFF) as u16;
                let end_y = (yp & 0xFFF) as u16;
                SpuCommand::SetDisplayArea {
                    start_x,
                    end_x,
                    start_y,
                    end_y,
                }
            }
            0x06 => {
                if i + 4 > total {
                    return Err(Error::InvalidUdf("SPU SET_DSPXA truncated"));
                }
                let top = u16::from_be_bytes([buf[i], buf[i + 1]]);
                let bot = u16::from_be_bytes([buf[i + 2], buf[i + 3]]);
                i += 4;
                SpuCommand::SetPixelDataAddresses {
                    top_field_offset: top,
                    bottom_field_offset: bot,
                }
            }
            0x07 => {
                if i + 2 > total {
                    return Err(Error::InvalidUdf("SPU CHG_COLCON size truncated"));
                }
                let size = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
                if size < 2 || i + size > total {
                    return Err(Error::InvalidUdf(
                        "SPU CHG_COLCON parameter area out of range",
                    ));
                }
                let raw = buf[i..i + size].to_vec();
                i += size;
                SpuCommand::ChangeColorContrast { raw }
            }
            0xFF => {
                commands.push(SpuCommand::EndOfSequence);
                return Ok(SpDcSq {
                    start_time,
                    next_offset,
                    commands,
                });
            }
            _ => {
                return Err(Error::InvalidUdf("SPU unknown command opcode"));
            }
        };
        commands.push(cmd);
    }
    Err(Error::InvalidUdf(
        "SPU DCSQ ran past buffer without CMD_END",
    ))
}

/// One decoded run from a PXDtf / PXDbf stream: `count` pixels of
/// `code` (0..=3).
///
/// `count == 0` is the "until end of line" special encoding from
/// mpucoder-spu.html §PXDtf — the caller must clamp the run to the
/// remaining row width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRun {
    /// Number of identical pixels, or `0` for "until end of line".
    pub count: u16,
    /// Pixel value in `0..=3` (background / pattern / emphasis1 /
    /// emphasis2).
    pub code: u8,
}

/// A 1-bit-at-a-time reader over the PXDtf / PXDbf RLE stream.
///
/// The RLE is packed MSB-first inside each byte. At the end of a
/// line the parser zero-pads to the next byte boundary (per
/// mpucoder-spu.html §PXDtf "four fill bits of 0 are added") — that
/// reset is the caller's responsibility (typically by tracking the
/// emitted-pixels count and calling [`PxdReader::align_to_byte`] at
/// every row boundary).
struct PxdReader<'a> {
    bytes: &'a [u8],
    /// Current byte index.
    byte_idx: usize,
    /// Bit position within the current byte, counting MSB-first.
    /// `bit_idx == 0` means the next bit returned is the byte's
    /// most-significant bit; `bit_idx == 7` means the least.
    bit_idx: u8,
}

impl<'a> PxdReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            byte_idx: 0,
            bit_idx: 0,
        }
    }

    fn read_bits(&mut self, n: u8) -> Result<u32> {
        debug_assert!(n <= 16);
        let mut out: u32 = 0;
        for _ in 0..n {
            if self.byte_idx >= self.bytes.len() {
                return Err(Error::InvalidUdf("SPU pixel data ran out of bits"));
            }
            let b = self.bytes[self.byte_idx];
            let bit = (b >> (7 - self.bit_idx)) & 1;
            out = (out << 1) | u32::from(bit);
            self.bit_idx += 1;
            if self.bit_idx == 8 {
                self.bit_idx = 0;
                self.byte_idx += 1;
            }
        }
        Ok(out)
    }

    fn align_to_byte(&mut self) {
        if self.bit_idx != 0 {
            self.bit_idx = 0;
            self.byte_idx += 1;
        }
    }
}

/// Decode one `(count, code)` run from a PXD reader.
///
/// Per mpucoder-spu.html §PXDtf the four run forms are nested by
/// leading-zero count:
///
/// | bits | shape           | count range |
/// |------|-----------------|-------------|
/// | 4    | `n n c c`       | 1..=3       |
/// | 8    | `0 0 n n n n c c` | 4..=15    |
/// | 12   | `0 0 0 0 n n n n n n c c` | 16..=63 |
/// | 16   | `0 0 0 0 0 0 n n n n n n n n c c` | 64..=255 |
///
/// A 16-bit form with `count == 0` is the "until end of line"
/// terminator.
fn decode_one_run(rdr: &mut PxdReader<'_>) -> Result<PixelRun> {
    // Read the top nibble to figure out which form is in play. The
    // first bit pair distinguishes the 4-bit form (count 1..=3)
    // from longer forms (count 4..=255 OR end-of-line).
    let top = rdr.read_bits(2)? as u8;
    if top != 0 {
        // 4-bit form: `n n c c`. Top two bits already give n
        // (1..=3); read two more bits for the code.
        let count = u16::from(top);
        let code = rdr.read_bits(2)? as u8;
        return Ok(PixelRun { count, code });
    }
    // Read next two bits and inspect: if either is set, we have an
    // 8-bit form (count 4..=15).
    let next2 = rdr.read_bits(2)? as u8;
    if next2 != 0 {
        // 8-bit form: full layout `0 0 n n n n c c` — we have read
        // bits 0..=3 so far, where bits 2..=3 are the high nibble
        // half of n. Read bits 4..=5 (low half of n) and bits 6..=7
        // (code).
        let low2 = rdr.read_bits(2)? as u8;
        let code = rdr.read_bits(2)? as u8;
        let count = (u16::from(next2) << 2) | u16::from(low2);
        return Ok(PixelRun { count, code });
    }
    // We now know the leading prefix is `0 0 0 0`. Read two more
    // bits; if either is set we have a 12-bit form (count 16..=63).
    let then2 = rdr.read_bits(2)? as u8;
    if then2 != 0 {
        // 12-bit form: `0 0 0 0 n n n n n n c c`. We have read 6
        // bits so far (`0 0 0 0` prefix, then bits 4..=5 of n).
        // `then2` holds bits 4..=5 of n (the high two of the 6-bit
        // count). Read four more n-bits (low four of n) then two
        // code-bits.
        let mid4 = rdr.read_bits(4)? as u8;
        let code = rdr.read_bits(2)? as u8;
        let count = (u16::from(then2) << 4) | u16::from(mid4);
        return Ok(PixelRun { count, code });
    }
    // Prefix is now `0 0 0 0 0 0`. Read the final 10 bits: 8 for
    // count, 2 for code. A `count == 0` value here is the
    // end-of-line marker.
    let count8 = rdr.read_bits(8)? as u16;
    let code = rdr.read_bits(2)? as u8;
    Ok(PixelRun {
        count: count8,
        code,
    })
}

/// Decode one field's worth of RLE-compressed pixel data into row-
/// major runs.
///
/// `bytes` should be the PXDtf or PXDbf slice (the section between
/// `SET_DSPXA`'s top/bottom field offsets and the start of the
/// SP_DCSQT). `width` is the field's pixel width — needed to track
/// when a row ends so the parser can apply the four-zero-bit
/// padding from mpucoder-spu.html §PXDtf. `expected_lines` is the
/// number of rows in this field (half the display area's height,
/// rounded toward the appropriate field).
///
/// Returns one `Vec<PixelRun>` per output line, top to bottom.
pub fn decode_rle_field(
    bytes: &[u8],
    width: u16,
    expected_lines: u16,
) -> Result<Vec<Vec<PixelRun>>> {
    if width == 0 {
        return Ok(Vec::new());
    }
    let mut rdr = PxdReader::new(bytes);
    let mut out: Vec<Vec<PixelRun>> = Vec::with_capacity(usize::from(expected_lines));
    for _ in 0..expected_lines {
        let mut row = Vec::new();
        let mut written: u32 = 0;
        let row_width = u32::from(width);
        while written < row_width {
            let run = decode_one_run(&mut rdr)?;
            if run.count == 0 {
                // End-of-line: pad with code to fill the row. We
                // don't bump `written` after this because we exit
                // the loop unconditionally.
                let remaining = row_width - written;
                let clamped = u16::try_from(remaining).unwrap_or(u16::MAX);
                row.push(PixelRun {
                    count: clamped,
                    code: run.code,
                });
                break;
            }
            let take = u32::from(run.count).min(row_width - written);
            row.push(PixelRun {
                count: take as u16,
                code: run.code,
            });
            written += take;
        }
        // Per mpucoder-spu.html, "if at the end of a line the bit
        // count is not a multiple of 8, four fill bits of 0 are
        // added." The actual padding is "up to the next nibble"
        // boundary — interpreted in practice as: if the next bit
        // index is not at a nibble (mod 4) boundary, skip enough
        // bits to reach one. Concretely real encoders zero-pad to
        // the next byte; we align to the next byte here, which is
        // a strict superset of the nibble alignment.
        rdr.align_to_byte();
        out.push(row);
    }
    Ok(out)
}

/// Materialise one field's runs into a flat `Vec<u8>` of palette
/// indices (`0..=3`), `width` pixels per line and `expected_lines`
/// rows.
///
/// Returned buffer is row-major: `pixels[y * width + x]`. Useful for
/// callers that want to blend against a YCrCb palette without
/// walking the run vector themselves.
pub fn render_field(bytes: &[u8], width: u16, expected_lines: u16) -> Result<Vec<u8>> {
    let runs = decode_rle_field(bytes, width, expected_lines)?;
    let w = usize::from(width);
    let h = runs.len();
    let mut out = vec![0u8; w * h];
    for (y, row) in runs.iter().enumerate() {
        let mut x = 0usize;
        for run in row {
            let n = usize::from(run.count).min(w - x);
            for slot in out[y * w + x..y * w + x + n].iter_mut() {
                *slot = run.code;
            }
            x += n;
            if x >= w {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let bytes = [0x12, 0x34, 0x56, 0x78];
        let h = SpuHeader::parse(&bytes).unwrap();
        assert_eq!(h.size, 0x1234);
        assert_eq!(h.dcsqt_offset, 0x5678);
    }

    #[test]
    fn header_rejects_short() {
        assert!(SpuHeader::parse(&[0, 1, 2]).is_err());
    }

    #[test]
    fn delay_conversion_matches_table() {
        // Per mpucoder-spu.html the table lists `seconds = 1 →
        // SP_DCSQ_STM = 87`. Inverting: 87 * 1024 / 90 = 989 ms
        // (the conversion is asymmetric because the encoder
        // truncates seconds * 90000/1024 = 87, losing ~11 ms).
        assert_eq!(spdcsq_stm_to_ms(87), 989);
        // seconds = 10 → 878. 878 * 1024 / 90 = 9989 ms ≈ 10 s.
        assert_eq!(spdcsq_stm_to_ms(878), 9989);
    }

    #[test]
    fn one_run_4bit_form() {
        // `n n c c` = 0b01_10 = (count=1, code=2). One byte's MSB
        // nibble = 0x60, padded.
        let bytes = [0b0110_0000];
        let mut rdr = PxdReader::new(&bytes);
        let run = decode_one_run(&mut rdr).unwrap();
        assert_eq!(run.count, 1);
        assert_eq!(run.code, 2);
    }

    #[test]
    fn one_run_8bit_form() {
        // `0 0 n n n n c c` with n = 5 (0b0101), c = 3:
        // bits = 0 0 0 1 0 1 1 1 = 0x17.
        let bytes = [0x17];
        let mut rdr = PxdReader::new(&bytes);
        let run = decode_one_run(&mut rdr).unwrap();
        assert_eq!(run.count, 5);
        assert_eq!(run.code, 3);
    }

    #[test]
    fn one_run_12bit_form() {
        // `0 0 0 0 n n n n n n c c` with n = 20 (0b010100), c = 1:
        // bits = 0 0 0 0 0 1 0 1 0 0 0 1 → 0x05, 0x10 (zero-padded
        // to 16).
        let bytes = [0b0000_0101, 0b0001_0000];
        let mut rdr = PxdReader::new(&bytes);
        let run = decode_one_run(&mut rdr).unwrap();
        assert_eq!(run.count, 20);
        assert_eq!(run.code, 1);
    }

    #[test]
    fn one_run_16bit_form() {
        // `0 0 0 0 0 0 n n n n n n n n c c` with n = 200
        // (0b11001000), c = 2:
        // bits = 0 0 0 0 0 0 1 1   0 0 1 0 0 0 1 0 → 0x03, 0x22.
        let bytes = [0x03, 0x22];
        let mut rdr = PxdReader::new(&bytes);
        let run = decode_one_run(&mut rdr).unwrap();
        assert_eq!(run.count, 200);
        assert_eq!(run.code, 2);
    }

    #[test]
    fn one_run_end_of_line() {
        // 16-bit form with n == 0 marks "until EOL". Pick c = 0.
        // bits = 0000_0000_0000_0000 → 0x00, 0x00.
        let bytes = [0x00, 0x00];
        let mut rdr = PxdReader::new(&bytes);
        let run = decode_one_run(&mut rdr).unwrap();
        assert_eq!(run.count, 0);
        assert_eq!(run.code, 0);
    }

    #[test]
    fn decode_rle_field_pads_eol_run_to_width() {
        // One row, width 10. Encode "run code=0 length 3" then
        // "end-of-line code=1".
        //   4-bit: n=3 (0b11), c=0 (0b00) → 0b1100 = 0xC, then nibble
        //   16-bit EOL: 0000_0000_0000_0001 starting after the 4 bits.
        // Combined bit sequence:
        //   1100  0000 0000 0000 0001  pad
        //   = 1100_0000  0000_0000  0001_<pad to byte>_  → 0xC0, 0x00, 0x10? Let's lay it out.
        //
        // bit index 0  1  2  3 | 4  5  6  7 | 8  9 10 11 |12 13 14 15 |16 17 ...
        //           1  1  0  0   0  0  0  0   0  0  0  0   0  0  0  1   pad zeros
        //   byte 0: 11000000 = 0xC0
        //   byte 1: 00000000 = 0x00
        //   byte 2: 0001 + 4 padding zeros = 0001_0000 = 0x10
        let bytes = [0xC0, 0x00, 0x10];
        let rows = decode_rle_field(&bytes, 10, 1).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row[0], PixelRun { count: 3, code: 0 });
        assert_eq!(row[1], PixelRun { count: 7, code: 1 });
    }

    #[test]
    fn render_field_materialises_pixels() {
        // Same as decode_rle_field_pads_eol_run_to_width — verify
        // the flat buffer ends up `[0,0,0, 1,1,1,1,1,1,1]`.
        let bytes = [0xC0, 0x00, 0x10];
        let pixels = render_field(&bytes, 10, 1).unwrap();
        assert_eq!(pixels.len(), 10);
        assert_eq!(&pixels[0..3], &[0, 0, 0]);
        assert_eq!(&pixels[3..10], &[1, 1, 1, 1, 1, 1, 1]);
    }

    fn build_minimal_spu() -> Vec<u8> {
        // Build a tiny but well-formed SPU:
        //   SPUH: SPDSZ = 0x28 (40), SP_DCSQTA = 0x10 (16).
        //   PXD slack at 0x04..0x10 (12 bytes, unused for the
        //     parser).
        //   DCSQ at 0x10:
        //     start_time = 0x0000, next_offset = 0x0010 (terminal,
        //     pointing at itself).
        //     SET_COLOR(opc=0x03, e2=0xF, e1=0xE, p=0xD, b=0xC)
        //       → 3 bytes (0x14..0x17)
        //     SET_CONTR(opc=0x04, e2=0xF, e1=0xF, p=0x8, b=0x0)
        //       → 3 bytes (0x17..0x1A)
        //     SET_DAREA(opc=0x05, x:0..3, y:0..3) → 7 bytes
        //       (0x1A..0x21)
        //     SET_DSPXA(opc=0x06, top=0x0004, bot=0x0004) → 5
        //       bytes (0x21..0x26)
        //     STA_DSP(opc=0x01) → 1 byte (0x26)
        //     CMD_END(opc=0xFF) → 1 byte (0x27)
        //   → SPDSZ = 0x28 (40).
        let mut buf = vec![0u8; 0x28];
        // SPUH.
        buf[0..2].copy_from_slice(&0x0028u16.to_be_bytes());
        buf[2..4].copy_from_slice(&0x0010u16.to_be_bytes());
        // PXD slack (4 bytes at 0x04..0x08 — unused by parse, the
        // actual PXD content is referenced via SET_DSPXA below).
        // DCSQ at 0x10.
        let dcsq = 0x10;
        buf[dcsq..dcsq + 2].copy_from_slice(&0x0000u16.to_be_bytes());
        buf[dcsq + 2..dcsq + 4].copy_from_slice(&(dcsq as u16).to_be_bytes());
        let mut o = dcsq + 4;
        // SET_COLOR
        buf[o] = 0x03;
        buf[o + 1] = 0xFE;
        buf[o + 2] = 0xDC;
        o += 3;
        // SET_CONTR
        buf[o] = 0x04;
        buf[o + 1] = 0xFF;
        buf[o + 2] = 0x80;
        o += 3;
        // SET_DAREA: start_x=0, end_x=3, start_y=0, end_y=3.
        // Each 24-bit field: (start << 12) | end. start=0 here so
        // the shift simplifies to just the end value.
        buf[o] = 0x05;
        let xp: u32 = 3; // (0 << 12) | 3 = 0x00_0003
        let yp: u32 = 3;
        let xb = xp.to_be_bytes();
        let yb = yp.to_be_bytes();
        buf[o + 1] = xb[1];
        buf[o + 2] = xb[2];
        buf[o + 3] = xb[3];
        buf[o + 4] = yb[1];
        buf[o + 5] = yb[2];
        buf[o + 6] = yb[3];
        o += 7;
        // SET_DSPXA
        buf[o] = 0x06;
        buf[o + 1..o + 3].copy_from_slice(&0x0004u16.to_be_bytes());
        buf[o + 3..o + 5].copy_from_slice(&0x0004u16.to_be_bytes());
        o += 5;
        // STA_DSP
        buf[o] = 0x01;
        o += 1;
        // CMD_END
        buf[o] = 0xFF;
        // o + 1 should land at 0x28 (= buf.len()).
        debug_assert_eq!(o + 1, buf.len());
        buf
    }

    #[test]
    fn parse_minimal_spu_full_unit() {
        let buf = build_minimal_spu();
        let spu = SubPictureUnit::parse(&buf).unwrap();
        assert_eq!(spu.header.size, 0x28);
        assert_eq!(spu.header.dcsqt_offset, 0x10);
        assert_eq!(spu.control_sequences.len(), 1);
        let dcsq = &spu.control_sequences[0];
        assert_eq!(dcsq.start_time, 0);
        assert_eq!(dcsq.next_offset, 0x10);
        assert_eq!(dcsq.commands.len(), 6);
        assert_eq!(
            dcsq.commands[0],
            SpuCommand::SetColor {
                emphasis2: 0xF,
                emphasis1: 0xE,
                pattern: 0xD,
                background: 0xC,
            }
        );
        assert_eq!(
            dcsq.commands[1],
            SpuCommand::SetContrast {
                emphasis2: 0xF,
                emphasis1: 0xF,
                pattern: 0x8,
                background: 0x0,
            }
        );
        assert_eq!(
            dcsq.commands[2],
            SpuCommand::SetDisplayArea {
                start_x: 0,
                end_x: 3,
                start_y: 0,
                end_y: 3,
            }
        );
        assert_eq!(
            dcsq.commands[3],
            SpuCommand::SetPixelDataAddresses {
                top_field_offset: 4,
                bottom_field_offset: 4,
            }
        );
        assert_eq!(dcsq.commands[4], SpuCommand::StartDisplay);
        assert_eq!(dcsq.commands[5], SpuCommand::EndOfSequence);

        // Convenience accessors.
        assert_eq!(spu.pixel_data_offsets(), Some((4, 4)));
        assert_eq!(spu.display_dimensions(), Some((4, 4)));
    }

    #[test]
    fn parse_rejects_dcsqta_out_of_range() {
        // SPDSZ = 0x10, but SP_DCSQTA = 0x20 (off the end).
        let mut buf = vec![0u8; 0x10];
        buf[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
        buf[2..4].copy_from_slice(&0x0020u16.to_be_bytes());
        assert!(SubPictureUnit::parse(&buf).is_err());
    }

    #[test]
    fn parse_rejects_dcsq_without_end() {
        // 0x10 bytes, DCSQ at 0x04, no CMD_END before end of unit.
        let mut buf = vec![0u8; 0x10];
        buf[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
        buf[2..4].copy_from_slice(&0x0004u16.to_be_bytes());
        // DCSQ header: start_time = 0, next_offset = 0x04 (terminal).
        buf[4..6].copy_from_slice(&0x0000u16.to_be_bytes());
        buf[6..8].copy_from_slice(&0x0004u16.to_be_bytes());
        // Fill the rest with STA_DSP (0x01) — never terminates.
        for b in &mut buf[8..] {
            *b = 0x01;
        }
        assert!(SubPictureUnit::parse(&buf).is_err());
    }

    #[test]
    fn change_color_contrast_round_trips_raw() {
        // SPDSZ = 0x18, SP_DCSQTA = 0x04.
        let mut buf = vec![0u8; 0x18];
        buf[0..2].copy_from_slice(&0x0018u16.to_be_bytes());
        buf[2..4].copy_from_slice(&0x0004u16.to_be_bytes());
        // DCSQ at 0x04: start_time = 0x0042, next_offset = 0x0004
        // (terminal).
        buf[4..6].copy_from_slice(&0x0042u16.to_be_bytes());
        buf[6..8].copy_from_slice(&0x0004u16.to_be_bytes());
        // CHG_COLCON at 0x08: opcode + size = 6 + body of 4 bytes
        // (LN_CTLI terminator 0F FF FF FF) → total parameter area = 6.
        // Layout: [0x07] [0x00 0x06] [0x0F 0xFF 0xFF 0xFF]
        buf[8] = 0x07;
        buf[9..11].copy_from_slice(&0x0006u16.to_be_bytes());
        buf[11] = 0x0F;
        buf[12] = 0xFF;
        buf[13] = 0xFF;
        buf[14] = 0xFF;
        // CMD_END at 0x0F.
        buf[15] = 0xFF;
        let spu = SubPictureUnit::parse(&buf).unwrap();
        assert_eq!(spu.control_sequences.len(), 1);
        let cmds = &spu.control_sequences[0].commands;
        assert_eq!(cmds.len(), 2);
        match &cmds[0] {
            SpuCommand::ChangeColorContrast { raw } => {
                assert_eq!(raw, &[0x00, 0x06, 0x0F, 0xFF, 0xFF, 0xFF]);
            }
            other => panic!("expected ChangeColorContrast, got {other:?}"),
        }
        assert_eq!(cmds[1], SpuCommand::EndOfSequence);
    }

    #[test]
    fn opcodes_match_table() {
        assert_eq!(SpuCommand::ForcedStartDisplay.opcode(), 0x00);
        assert_eq!(SpuCommand::StartDisplay.opcode(), 0x01);
        assert_eq!(SpuCommand::StopDisplay.opcode(), 0x02);
        assert_eq!(
            SpuCommand::SetColor {
                emphasis2: 0,
                emphasis1: 0,
                pattern: 0,
                background: 0
            }
            .opcode(),
            0x03
        );
        assert_eq!(
            SpuCommand::SetContrast {
                emphasis2: 0,
                emphasis1: 0,
                pattern: 0,
                background: 0
            }
            .opcode(),
            0x04
        );
        assert_eq!(
            SpuCommand::SetDisplayArea {
                start_x: 0,
                end_x: 0,
                start_y: 0,
                end_y: 0
            }
            .opcode(),
            0x05
        );
        assert_eq!(
            SpuCommand::SetPixelDataAddresses {
                top_field_offset: 0,
                bottom_field_offset: 0
            }
            .opcode(),
            0x06
        );
        assert_eq!(
            SpuCommand::ChangeColorContrast { raw: Vec::new() }.opcode(),
            0x07
        );
        assert_eq!(SpuCommand::EndOfSequence.opcode(), 0xFF);
    }
}
