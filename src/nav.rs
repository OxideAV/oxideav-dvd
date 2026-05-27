//! DVD-Video VM **instruction decoder** — Phase 3c precursor.
//!
//! This module turns a raw 8-byte [`NavCommand`] word into a typed
//! [`NavInstruction`] tree describing the opcode family, the GPRM /
//! SPRM register operands, the inline 16-bit values, the comparison
//! and arithmetic sub-ops, and the link / jump / call target. **It
//! does not execute anything.** Execution (an interpreter that owns
//! GPRMs + SPRMs + PC + RSM stack) is the bulk of Phase 3c proper;
//! decoding the instruction stream is the prerequisite step a
//! debugger / analyser / future executor all share.
//!
//! Clean-room per:
//!
//! - `docs/container/dvd/application/mpucoder-vmi.html` — the full
//!   opcode table (Type 0..7) including the SET/CMP sub-op codes and
//!   the link-subset table.
//! - `docs/container/dvd/application/mpucoder-vmi-sum.html` — the
//!   plain-English instruction-family summary used to verify each
//!   variant's intent.
//! - `docs/container/dvd/application/mpucoder-vmi-jmp.html` — the
//!   jump-target table for `JumpSS` / `CallSS`.
//! - `docs/container/dvd/application/mpucoder-sprm.html` — the SPRM
//!   numbering for the [`Register`] enum.
//!
//! No external implementation source consulted — clean-room from the
//! `docs/container/dvd/` references listed above.

use crate::ifo::NavCommand;

// =====================================================================
// Registers — GPRM / SPRM addressing.
// =====================================================================

/// Register identifier referenced by a VM operand byte.
///
/// Per the asterisk note on `mpucoder-vmi.html`:
/// `0x00..=0x0F` are general-purpose registers (GPRM 0..15);
/// `0x80..=0x97` are system-parameter registers (SPRM 0..23);
/// everything else is invalid and must be reported as
/// [`Register::Invalid`] — we preserve the raw byte so a future
/// auditor can still see what the encoder put on disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Register {
    /// General-purpose register (writable; persists across PGCs).
    /// Index is 0..=15.
    Gprm(u8),
    /// System-parameter register (largely read-only state, see
    /// `mpucoder-sprm.html` for the per-index meaning). Index is
    /// 0..=23 (raw byte minus `0x80`).
    Sprm(u8),
    /// The raw byte was outside the two valid ranges. We surface it
    /// verbatim rather than refusing to decode — malformed PGC
    /// command tables in the wild often carry junk here.
    Invalid(u8),
}

impl Register {
    /// Classify an 8-bit register field.
    pub fn decode(byte: u8) -> Self {
        match byte {
            0x00..=0x0F => Self::Gprm(byte),
            0x80..=0x97 => Self::Sprm(byte - 0x80),
            _ => Self::Invalid(byte),
        }
    }
}

// =====================================================================
// SET / CMP sub-op codes.
// =====================================================================

/// 4-bit SET sub-op — assignment / arithmetic / bitwise operation.
///
/// Codes 0..=0x0B per `mpucoder-vmi.html` "SET and CMP operations"
/// table; codes 0x0C..=0x0F are listed as invalid in the same row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `0` — no SET operation (the encoded command is pure compare /
    /// link).
    None,
    /// `1` — `mov` — `dst = src`.
    Mov,
    /// `2` — `swp` — `dst <-> src`.
    Swp,
    /// `3` — `add` — `dst += src`.
    Add,
    /// `4` — `sub` — `dst -= src`.
    Sub,
    /// `5` — `mul` — `dst *= src`.
    Mul,
    /// `6` — `div` — `dst /= src`.
    Div,
    /// `7` — `mod` — `dst %= src`.
    Mod,
    /// `8` — `rnd` — random in `[0, src)` per common interpretation
    /// (the spec page leaves the operand column blank; we don't act
    /// on the operand here).
    Rnd,
    /// `9` — `and` — `dst &= src`.
    And,
    /// `A` — `or`  — `dst |= src`.
    Or,
    /// `B` — `xor` — `dst ^= src`.
    Xor,
    /// `C..F` — listed as invalid.
    Invalid(u8),
}

impl SetOp {
    /// Decode a 4-bit SET sub-op.
    pub fn decode(code: u8) -> Self {
        match code & 0x0F {
            0 => Self::None,
            1 => Self::Mov,
            2 => Self::Swp,
            3 => Self::Add,
            4 => Self::Sub,
            5 => Self::Mul,
            6 => Self::Div,
            7 => Self::Mod,
            8 => Self::Rnd,
            9 => Self::And,
            0xA => Self::Or,
            0xB => Self::Xor,
            other => Self::Invalid(other),
        }
    }
}

/// 3-bit CMP sub-op — comparison predicate.
///
/// Codes 0..=7 per `mpucoder-vmi.html` "SET and CMP operations".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `0` — no compare (unconditional).
    None,
    /// `1` — `BC` — bit-clear test (`(lhs & rhs) == 0`).
    Bc,
    /// `2` — `EQ` — equal.
    Eq,
    /// `3` — `NE` — not equal.
    Ne,
    /// `4` — `GE` — greater or equal.
    Ge,
    /// `5` — `GT` — greater than.
    Gt,
    /// `6` — `LE` — less or equal.
    Le,
    /// `7` — `LT` — less than.
    Lt,
}

impl CmpOp {
    /// Decode a 3-bit CMP sub-op.
    pub fn decode(code: u8) -> Self {
        match code & 0x07 {
            0 => Self::None,
            1 => Self::Bc,
            2 => Self::Eq,
            3 => Self::Ne,
            4 => Self::Ge,
            5 => Self::Gt,
            6 => Self::Le,
            _ => Self::Lt,
        }
    }
}

// =====================================================================
// Compare operand — register or immediate.
// =====================================================================

/// Operand for a compare or a Set right-hand side.
///
/// The "direct" bit of the encoding chooses between a register
/// operand (`Register`) and a 16-bit immediate (`Immediate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// Register reference (GPRM or SPRM — see [`Register`]).
    Register(Register),
    /// 16-bit big-endian immediate constant.
    Immediate(u16),
}

// =====================================================================
// Link subset — the type-1.0.1 "Link" inner table.
// =====================================================================

/// Inner code for a `0x20 0x01` "Link subset" command per the
/// `link_subset` table in `mpucoder-vmi.html`.
///
/// Codes 0x04, 0x08, 0x0E, 0x0F, and 0x11..0x1F are listed as
/// invalid; we surface the raw byte so a downstream auditor can
/// reason about non-conforming discs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSubset {
    /// `00` — NOP (no-op within the link group).
    Nop,
    /// `01` — `LinkTopCell` — restart current cell.
    LinkTopCell,
    /// `02` — `LinkNextCell` — proceed to next cell.
    LinkNextCell,
    /// `03` — `LinkPrevCell` — return to previous cell.
    LinkPrevCell,
    /// `05` — `LinkTopPG` — restart current Program.
    LinkTopPG,
    /// `06` — `LinkNextPG` — proceed to next Program.
    LinkNextPG,
    /// `07` — `LinkPrevPG` — return to previous Program.
    LinkPrevPG,
    /// `09` — `LinkTopPGC` — restart current PGC.
    LinkTopPGC,
    /// `0A` — `LinkNextPGC` — proceed to next-PGCN.
    LinkNextPGC,
    /// `0B` — `LinkPrevPGC` — return to prev-PGCN.
    LinkPrevPGC,
    /// `0C` — `LinkGoupPGC` — go up to group-PGCN.
    LinkGoupPGC,
    /// `0D` — `LinkTailPGC` — jump to PGC's post-commands.
    LinkTailPGC,
    /// `10` — `RSM` — resume from saved CallSS state.
    Rsm,
    /// Anything in `04, 08, 0E, 0F, 11..1F` per the spec's "invalid"
    /// row.
    Invalid(u8),
}

impl LinkSubset {
    /// Decode the bottom 5 bits of byte 7 (the `Lnk` field).
    pub fn decode(code: u8) -> Self {
        match code & 0x1F {
            0x00 => Self::Nop,
            0x01 => Self::LinkTopCell,
            0x02 => Self::LinkNextCell,
            0x03 => Self::LinkPrevCell,
            0x05 => Self::LinkTopPG,
            0x06 => Self::LinkNextPG,
            0x07 => Self::LinkPrevPG,
            0x09 => Self::LinkTopPGC,
            0x0A => Self::LinkNextPGC,
            0x0B => Self::LinkPrevPGC,
            0x0C => Self::LinkGoupPGC,
            0x0D => Self::LinkTailPGC,
            0x10 => Self::Rsm,
            other => Self::Invalid(other),
        }
    }
}

// =====================================================================
// JumpSS / CallSS targets.
// =====================================================================

/// `JumpSS` destination per the Type-1.1, CMD=6 family in
/// `mpucoder-vmi.html`.
///
/// The two-bit selector in byte 5 bits 5..4 picks the destination
/// kind; we surface each as a typed variant rather than the raw
/// selector word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpSSTarget {
    /// Selector `0` — `JumpSS FP` (jump to the First-Play PGC).
    FirstPlay,
    /// Selector `1` — `JumpSS VMGM menu, <menu_id>` (a VMG menu
    /// identified by the 4-bit menu index).
    VmgmMenu { menu: u8 },
    /// Selector `2` — `JumpSS VTSM <vts>, <ttn>, <menu>` (a VTS-menu
    /// PGC).
    VtsmMenu { vts: u8, ttn: u8, menu: u8 },
    /// Selector `3` — `JumpSS VMGM pgcn` (a specific VMG PGC by
    /// number — the 16-bit `pgcn` field from operand 1).
    VmgmPgcn { pgcn: u16 },
}

/// `CallSS` destination per the Type-1.1, CMD=8 family in
/// `mpucoder-vmi.html`. Adds a `rsm_cell` field common to all four
/// variants — the cell to resume to on a later `RSM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSSTarget {
    /// `CallSS FP` (with resume-cell).
    FirstPlay { rsm_cell: u8 },
    /// `CallSS VMGM menu, <menu>` (with resume-cell).
    VmgmMenu { menu: u8, rsm_cell: u8 },
    /// `CallSS VTSM menu, <menu>` (with resume-cell). Per the spec
    /// table, the VTS / TTN selectors aren't carried in CallSS the
    /// way they are in JumpSS — only the menu index.
    VtsmMenu { menu: u8, rsm_cell: u8 },
    /// `CallSS VMGM pgcn` (with resume-cell).
    VmgmPgcn { pgcn: u16, rsm_cell: u8 },
}

// =====================================================================
// NavInstruction — the typed decode tree.
// =====================================================================

/// Top-level VM instruction decoded from an 8-byte [`NavCommand`].
///
/// This is the "first pass" disassembly. The well-defined opcodes in
/// Types 0..3 (NOP, Goto, Break, SetTmpPML, the link / jump / call
/// family, SetSystem, plain Set) are returned as named variants; the
/// compound-operation families Type 4..6 (SetCLnk, CSetCLnk,
/// CmpSetLnk) and the rare conditional forms are surfaced as
/// [`NavInstruction::Compound`] with their classifier sub-fields
/// pre-decoded but the inner sub-operations left to a Phase-3c
/// executor.
///
/// All variants preserve the originating 8-byte word so a downstream
/// debugger can render the raw hex alongside the decoded form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavInstruction {
    // ---- Type 0 — special / compare-only ---------------------------
    /// `00 00` — `NOP`.
    Nop,
    /// `00 01 .. line` — `Goto line` (intra-PGC pre/post jump).
    Goto { line: u8 },
    /// `00 02` — `Break` (exits the current pre / post / cell list).
    Break,
    /// `00 03 .. lvl line` — `SetTmpPML lvl, line`.
    SetTmpPml { level: u8, line: u8 },

    // ---- Type 1 — link family (byte0[4] == 0, "Link") --------------
    /// `20 01` plus link-subset code in byte 7 — `Link<subset>
    /// [button=<hl_bn>]` (the 13 `Link*` / `RSM` inner forms).
    LinkSub { subset: LinkSubset, hl_bn: u8 },
    /// `20 04 .. pgcn` — `LinkPGCN pgcn`.
    LinkPgcn { pgcn: u16 },
    /// `20 05 .. (hl_bn|pttn)` — `LinkPTTN pttn [, button=hl_bn]`.
    LinkPttn { pttn: u16, hl_bn: u8 },
    /// `20 06 .. (hl_bn|pgn)` — `LinkPGN pgn [, button=hl_bn]`.
    LinkPgn { pgn: u8, hl_bn: u8 },
    /// `20 07 .. (hl_bn|cn)` — `LinkCN cn [, button=hl_bn]`.
    LinkCn { cn: u8, hl_bn: u8 },

    // ---- Type 1 — jump / call family (byte0[4] == 1) ---------------
    /// `30 01` — `Exit`.
    Exit,
    /// `30 02 .. ttn` — `JumpTT ttn`.
    JumpTT { ttn: u8 },
    /// `30 03 .. ttn` — `JumpVTS_TT ttn`.
    JumpVtsTt { ttn: u8 },
    /// `30 05 .. pttn .. ttn` — `JumpVTS_PTT ttn, pttn`.
    JumpVtsPtt { ttn: u8, pttn: u16 },
    /// `30 06 .. <target>` — `JumpSS <target>`.
    JumpSs(JumpSSTarget),
    /// `30 08 .. <target>` — `CallSS <target>`.
    CallSs(CallSSTarget),

    // ---- Type 2 — SetSystem family ---------------------------------
    /// `41 .. / 51 ..` — `SetSTN` (audio / sub-picture / angle).
    /// Raw fields preserved; consumers map `af`/`sf`/`nf` flag bits
    /// to act on the corresponding `src*` slot.
    SetStn {
        /// `0` = source is a register (`Operand::Register`); `1` =
        /// inline 16-bit immediate spread across the three 7-bit
        /// `aval`/`sval`/`nval` slots, per `mpucoder-vmi.html`
        /// row Type-2 SET=1 CMD=0.
        direct: bool,
        /// Audio-flag (`af` bit, byte 3 bit 7) — apply `audio_src`.
        af: bool,
        /// Audio source (register form: 4-bit `sr1`; immediate form:
        /// 7-bit `aval`).
        audio_src: u8,
        /// Sub-picture flag (`sf` bit).
        sf: bool,
        /// Sub-picture source.
        subpic_src: u8,
        /// Angle flag (`nf` bit).
        nf: bool,
        /// Angle source.
        angle_src: u8,
    },
    /// `42 .. / 52 ..` — `SetNVTMR srs, pgcn` (load nav timer +
    /// associated PGC number).
    SetNvtmr { src: Operand, pgcn: u16 },
    /// `43 .. / 53 ..` — `SetGPRMMD G<srd> = <src> [,COUNTER]`.
    SetGprmMd {
        src: Operand,
        dst: Register,
        /// `mf` bit — when set, the destination GPRM becomes a
        /// 1-Hz counter rather than a plain register.
        counter: bool,
    },
    /// `44 .. / 54 ..` — `SetAMXMD G<srs>` (karaoke mixing mode).
    SetAmxMd { src: Operand },
    /// `46 .. / 56 ..` — `SetHL_BTNN G<srs>` (force highlight to a
    /// specific button).
    SetHlBtnn { src: Operand },

    // ---- Type 3 — Set arithmetic -----------------------------------
    /// `6x .. / 7x ..` with sub-op `1..=B` — `Set G<srd> <set-op>
    /// <src>` (the plain Set / arithmetic / bitwise family).
    Set {
        op: SetOp,
        dst: Register,
        src: Operand,
    },

    // ---- Type 4..6 — compound CMP/SET/LNK families -----------------
    /// Type 4 — `SetCLnk` (Set then Compare & Link).
    SetCLnk {
        set_op: SetOp,
        cmp_op: CmpOp,
        /// Selector register (G<scr>) used as both SET destination
        /// and CMP left-hand side.
        scr: Register,
    },
    /// Type 5 — `CSetCLnk` (Compare then Set & Link).
    CSetCLnk { set_op: SetOp, cmp_op: CmpOp },
    /// Type 6 — `CmpSetLnk` (Compare then Set, followed by Link).
    CmpSetLnk { set_op: SetOp, cmp_op: CmpOp },

    // ---- Type 7 — undefined ----------------------------------------
    /// Type 7 — never observed in the wild per `mpucoder-vmi.html`;
    /// we surface it as `Unknown` rather than refusing to decode so
    /// disc-debug tooling keeps working.
    Unknown,

    // ---- Catch-all for malformed encodings -------------------------
    /// The byte 0 type field selected a known family but the
    /// inner CMD / SET / direct fields formed an "invalid" encoding
    /// per the spec's red rows. The 8 raw bytes are preserved.
    Invalid,
}

// =====================================================================
// Decoder.
// =====================================================================

impl NavCommand {
    /// Decode the 8-byte command word into a typed [`NavInstruction`].
    ///
    /// This is a single-pass classifier: it inspects byte 0's top
    /// three bits to pick the family, then dispatches to a per-type
    /// helper that pulls out the operand bytes. The well-defined
    /// instructions in Types 0..3 are returned in full; the compound
    /// families Type 4..6 surface their CMP/SET classifier sub-ops
    /// but leave the full operand decoding to a Phase-3c executor.
    ///
    /// **Pure function** — no side effects; the call is `O(1)`.
    pub fn decode(&self) -> NavInstruction {
        let b = &self.bytes;
        // byte 0: TTT D SSSS where TTT = type (bits 7..5), D = SET-direct flag,
        // SSSS = set-op or auxiliary nibble depending on family.
        let cmd_type = b[0] >> 5;
        let set_direct = (b[0] & 0x10) != 0;
        let set_nibble = b[0] & 0x0F;
        // byte 1: C HHH CCCC where C = CMP-direct flag (bit 7), HHH = cmp-op
        // (bits 6..4), CCCC = link/command nibble (bits 3..0).
        let cmd_nibble = b[1] & 0x0F;
        let cmp_op = CmpOp::decode((b[1] >> 4) & 0x07);

        match cmd_type {
            0 => decode_type0(b, cmd_nibble),
            1 => {
                if (b[0] & 0x10) == 0 {
                    decode_type1_link(b, cmd_nibble)
                } else {
                    decode_type1_jumpcall(b, cmd_nibble)
                }
            }
            2 => decode_type2_setsystem(b, set_direct, set_nibble),
            3 => decode_type3_set(b, set_direct, set_nibble),
            4 => NavInstruction::SetCLnk {
                set_op: SetOp::decode(set_nibble),
                cmp_op,
                scr: Register::Gprm(b[2] & 0x0F),
            },
            5 => NavInstruction::CSetCLnk {
                set_op: SetOp::decode(set_nibble),
                cmp_op,
            },
            6 => NavInstruction::CmpSetLnk {
                set_op: SetOp::decode(set_nibble),
                cmp_op,
            },
            _ => NavInstruction::Unknown,
        }
    }
}

fn decode_type0(b: &[u8; 8], cmd_nibble: u8) -> NavInstruction {
    // Compare-only sub-table (CMP-op != 0): per spec row "0 1-7 0-3"
    // these are conditional NOP / Goto / Break / SetTmpPML wrappers —
    // we decode the underlying instruction and ignore the compare
    // (a Phase-3c executor would honour it).
    match cmd_nibble {
        0 => NavInstruction::Nop,
        1 => NavInstruction::Goto { line: b[7] },
        2 => NavInstruction::Break,
        3 => NavInstruction::SetTmpPml {
            level: b[6] & 0x0F,
            line: b[7],
        },
        // 4..F per spec's red row: "invalid".
        _ => NavInstruction::Invalid,
    }
}

fn decode_type1_link(b: &[u8; 8], cmd_nibble: u8) -> NavInstruction {
    // hl_bn (highlight-button) is byte 6 bits 5..0 across the link
    // family per the operand-3 column.
    let hl_bn = b[6] & 0x3F;
    match cmd_nibble {
        0 => NavInstruction::Nop,
        1 => NavInstruction::LinkSub {
            subset: LinkSubset::decode(b[7]),
            hl_bn,
        },
        4 => NavInstruction::LinkPgcn {
            pgcn: u16::from_be_bytes([b[6], b[7]]),
        },
        5 => NavInstruction::LinkPttn {
            // 10-bit pttn occupies byte 6 bits 1..0 + byte 7.
            pttn: u16::from_be_bytes([b[6] & 0x03, b[7]]),
            hl_bn,
        },
        6 => NavInstruction::LinkPgn { pgn: b[7], hl_bn },
        7 => NavInstruction::LinkCn { cn: b[7], hl_bn },
        // 2, 3, 8..F: invalid Link nibbles per spec.
        _ => NavInstruction::Invalid,
    }
}

fn decode_type1_jumpcall(b: &[u8; 8], cmd_nibble: u8) -> NavInstruction {
    match cmd_nibble {
        0 => NavInstruction::Nop,
        1 => NavInstruction::Exit,
        2 => NavInstruction::JumpTT { ttn: b[5] },
        3 => NavInstruction::JumpVtsTt { ttn: b[5] },
        5 => NavInstruction::JumpVtsPtt {
            ttn: b[5],
            // 10-bit pttn — byte 2 bits 1..0 + byte 3.
            pttn: u16::from_be_bytes([b[2] & 0x03, b[3]]),
        },
        6 => NavInstruction::JumpSs(decode_jumpss_target(b)),
        8 => NavInstruction::CallSs(decode_callss_target(b)),
        // 4, 7, 9..F per spec: invalid.
        _ => NavInstruction::Invalid,
    }
}

fn decode_jumpss_target(b: &[u8; 8]) -> JumpSSTarget {
    // Selector at byte 5 bits 5..4. Operand layout per the four
    // `JumpSS` rows on the spec page.
    let selector = (b[5] >> 4) & 0x03;
    match selector {
        0 => JumpSSTarget::FirstPlay,
        1 => JumpSSTarget::VmgmMenu { menu: b[5] & 0x0F },
        2 => JumpSSTarget::VtsmMenu {
            ttn: b[3],
            vts: b[4],
            menu: b[5] & 0x0F,
        },
        _ => JumpSSTarget::VmgmPgcn {
            pgcn: u16::from_be_bytes([b[2], b[3]]),
        },
    }
}

fn decode_callss_target(b: &[u8; 8]) -> CallSSTarget {
    let selector = (b[5] >> 4) & 0x03;
    let rsm_cell = b[4];
    match selector {
        0 => CallSSTarget::FirstPlay { rsm_cell },
        1 => CallSSTarget::VmgmMenu {
            menu: b[5] & 0x0F,
            rsm_cell,
        },
        2 => CallSSTarget::VtsmMenu {
            menu: b[5] & 0x0F,
            rsm_cell,
        },
        _ => CallSSTarget::VmgmPgcn {
            pgcn: u16::from_be_bytes([b[2], b[3]]),
            rsm_cell,
        },
    }
}

fn decode_type2_setsystem(b: &[u8; 8], direct: bool, sub: u8) -> NavInstruction {
    // sub: byte 0 bits 3..0 picks the SetSystem opcode (1..6 valid
    // per `mpucoder-vmi.html`'s Type-2 SET=1 column).
    match sub {
        // SetSTN — sub-code 1.
        1 => {
            // Flag bits live at byte 3 bit 7 (af), byte 4 bit 7
            // (sf), byte 5 bit 7 (nf) in both register and immediate
            // forms; the source values live in bytes 3/4/5 either as
            // 4-bit register selectors (register form, low nibble)
            // or 7-bit immediates (immediate form, bits 6..0).
            let af = (b[3] & 0x80) != 0;
            let sf = (b[4] & 0x80) != 0;
            let nf = (b[5] & 0x80) != 0;
            let mask = if direct { 0x7F } else { 0x0F };
            NavInstruction::SetStn {
                direct,
                af,
                audio_src: b[3] & mask,
                sf,
                subpic_src: b[4] & mask,
                nf,
                angle_src: b[5] & mask,
            }
        }
        // SetNVTMR — sub-code 2.
        2 => NavInstruction::SetNvtmr {
            src: if direct {
                Operand::Immediate(u16::from_be_bytes([b[2], b[3]]))
            } else {
                Operand::Register(Register::decode(b[3]))
            },
            pgcn: u16::from_be_bytes([b[4], b[5]]),
        },
        // SetGPRMMD — sub-code 3. The 'mf' counter flag is byte 4
        // bit 7 in both forms.
        3 => NavInstruction::SetGprmMd {
            src: if direct {
                Operand::Immediate(u16::from_be_bytes([b[2], b[3]]))
            } else {
                Operand::Register(Register::decode(b[3]))
            },
            dst: Register::Gprm(b[5] & 0x0F),
            counter: (b[4] & 0x80) != 0,
        },
        // SetAMXMD — sub-code 4.
        4 => NavInstruction::SetAmxMd {
            src: if direct {
                Operand::Immediate(u16::from_be_bytes([b[4], b[5]]))
            } else {
                Operand::Register(Register::Gprm(b[5] & 0x0F))
            },
        },
        // SetHL_BTNN — sub-code 6.
        6 => NavInstruction::SetHlBtnn {
            src: if direct {
                Operand::Immediate(u16::from_be_bytes([b[4], b[5]]))
            } else {
                Operand::Register(Register::Gprm(b[5] & 0x0F))
            },
        },
        // 5 + 7..F per spec: invalid SetSystem.
        _ => NavInstruction::Invalid,
    }
}

fn decode_type3_set(b: &[u8; 8], direct: bool, sub: u8) -> NavInstruction {
    // sub: byte 0 bits 3..0 selects the SET sub-op; 1..=B valid.
    if !(1..=0x0B).contains(&sub) {
        return NavInstruction::Invalid;
    }
    let dst = Register::Gprm(b[3] & 0x0F);
    let src = if direct {
        Operand::Immediate(u16::from_be_bytes([b[4], b[5]]))
    } else {
        Operand::Register(Register::decode(b[5]))
    };
    NavInstruction::Set {
        op: SetOp::decode(sub),
        dst,
        src,
    }
}

// =====================================================================
// Tests.
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ifo::NavCommand;

    /// Helper — wrap 8 bytes as a [`NavCommand`] for decoding.
    fn cmd(bytes: [u8; 8]) -> NavCommand {
        NavCommand { bytes }
    }

    // -----------------------------------------------------------------
    // Register classifier.
    // -----------------------------------------------------------------

    #[test]
    fn register_decode_gprm_range() {
        for i in 0u8..=15 {
            assert_eq!(Register::decode(i), Register::Gprm(i));
        }
    }

    #[test]
    fn register_decode_sprm_range() {
        for i in 0u8..=23 {
            assert_eq!(Register::decode(0x80 + i), Register::Sprm(i));
        }
    }

    #[test]
    fn register_decode_invalid_holes() {
        // The mid-range hole (0x10..=0x7F) per the * note.
        assert_eq!(Register::decode(0x10), Register::Invalid(0x10));
        assert_eq!(Register::decode(0x7F), Register::Invalid(0x7F));
        // The upper hole (0x98..=0xFF).
        assert_eq!(Register::decode(0x98), Register::Invalid(0x98));
        assert_eq!(Register::decode(0xFF), Register::Invalid(0xFF));
    }

    // -----------------------------------------------------------------
    // SET / CMP op tables.
    // -----------------------------------------------------------------

    #[test]
    fn set_op_decodes_all_named_codes() {
        let table: &[(u8, SetOp)] = &[
            (0, SetOp::None),
            (1, SetOp::Mov),
            (2, SetOp::Swp),
            (3, SetOp::Add),
            (4, SetOp::Sub),
            (5, SetOp::Mul),
            (6, SetOp::Div),
            (7, SetOp::Mod),
            (8, SetOp::Rnd),
            (9, SetOp::And),
            (0xA, SetOp::Or),
            (0xB, SetOp::Xor),
        ];
        for (code, expect) in table {
            assert_eq!(SetOp::decode(*code), *expect);
        }
        assert_eq!(SetOp::decode(0xC), SetOp::Invalid(0xC));
        assert_eq!(SetOp::decode(0xF), SetOp::Invalid(0xF));
    }

    #[test]
    fn cmp_op_decodes_all_codes() {
        let table: &[(u8, CmpOp)] = &[
            (0, CmpOp::None),
            (1, CmpOp::Bc),
            (2, CmpOp::Eq),
            (3, CmpOp::Ne),
            (4, CmpOp::Ge),
            (5, CmpOp::Gt),
            (6, CmpOp::Le),
            (7, CmpOp::Lt),
        ];
        for (code, expect) in table {
            assert_eq!(CmpOp::decode(*code), *expect);
        }
    }

    // -----------------------------------------------------------------
    // Link-subset table.
    // -----------------------------------------------------------------

    #[test]
    fn link_subset_decodes_named_codes() {
        let table: &[(u8, LinkSubset)] = &[
            (0x00, LinkSubset::Nop),
            (0x01, LinkSubset::LinkTopCell),
            (0x02, LinkSubset::LinkNextCell),
            (0x03, LinkSubset::LinkPrevCell),
            (0x05, LinkSubset::LinkTopPG),
            (0x06, LinkSubset::LinkNextPG),
            (0x07, LinkSubset::LinkPrevPG),
            (0x09, LinkSubset::LinkTopPGC),
            (0x0A, LinkSubset::LinkNextPGC),
            (0x0B, LinkSubset::LinkPrevPGC),
            (0x0C, LinkSubset::LinkGoupPGC),
            (0x0D, LinkSubset::LinkTailPGC),
            (0x10, LinkSubset::Rsm),
        ];
        for (code, expect) in table {
            assert_eq!(LinkSubset::decode(*code), *expect);
        }
        // Spec's invalid bag.
        assert_eq!(LinkSubset::decode(0x04), LinkSubset::Invalid(0x04));
        assert_eq!(LinkSubset::decode(0x08), LinkSubset::Invalid(0x08));
        assert_eq!(LinkSubset::decode(0x0E), LinkSubset::Invalid(0x0E));
        assert_eq!(LinkSubset::decode(0x1F), LinkSubset::Invalid(0x1F));
    }

    // -----------------------------------------------------------------
    // Type 0 — NOP / Goto / Break / SetTmpPML.
    // -----------------------------------------------------------------

    #[test]
    fn decode_type0_nop() {
        assert_eq!(
            cmd([0x00, 0x00, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Nop
        );
    }

    #[test]
    fn decode_type0_goto() {
        // byte0 type=0, byte1 cmd-nibble=1 → Goto, line in byte 7.
        let i = cmd([0x00, 0x01, 0, 0, 0, 0, 0, 0x2A]).decode();
        assert_eq!(i, NavInstruction::Goto { line: 0x2A });
    }

    #[test]
    fn decode_type0_break() {
        assert_eq!(
            cmd([0x00, 0x02, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Break
        );
    }

    #[test]
    fn decode_type0_settmppml() {
        // Type 0, cmd-nibble=3 → SetTmpPML, lvl = byte6 low nibble,
        // line = byte 7.
        let i = cmd([0x00, 0x03, 0, 0, 0, 0, 0x05, 0x99]).decode();
        assert_eq!(
            i,
            NavInstruction::SetTmpPml {
                level: 5,
                line: 0x99
            }
        );
    }

    #[test]
    fn decode_type0_invalid_cmd_nibble() {
        // cmd-nibble = 4 → invalid per spec.
        assert_eq!(
            cmd([0x00, 0x04, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Invalid
        );
    }

    // -----------------------------------------------------------------
    // Type 1 link family.
    // -----------------------------------------------------------------

    #[test]
    fn decode_link_subset_with_button() {
        // 20 01 .. .. .. .. (hl_bn<<0=0x05) (subset=0x10 RSM)
        let i = cmd([0x20, 0x01, 0, 0, 0, 0, 0x05, 0x10]).decode();
        assert_eq!(
            i,
            NavInstruction::LinkSub {
                subset: LinkSubset::Rsm,
                hl_bn: 5,
            }
        );
    }

    #[test]
    fn decode_link_pgcn() {
        // 20 04 .. .. .. .. .. (pgcn = 0x1234 across bytes 6..7)
        let i = cmd([0x20, 0x04, 0, 0, 0, 0, 0x12, 0x34]).decode();
        assert_eq!(i, NavInstruction::LinkPgcn { pgcn: 0x1234 });
    }

    #[test]
    fn decode_link_pttn() {
        // 20 05 — pttn is 10-bit: byte6 bits 1..0 + byte7. hl_bn lives
        // in byte 6 bits 5..0 (and the top 2 bits of the pttn share
        // byte 6 with hl_bn — we treat the low 2 bits as pttn high
        // bits per the spec column).
        // pttn = 0x103 → byte6 low = 0b01, byte7 = 0x03.
        // hl_bn = 0x09.
        let i = cmd([0x20, 0x05, 0, 0, 0, 0, (0x09 & 0x3F) | 0x40 | 0x01, 0x03]).decode();
        match i {
            NavInstruction::LinkPttn { pttn, hl_bn } => {
                assert_eq!(pttn, 0x103);
                assert_eq!(hl_bn, 0x09);
            }
            _ => panic!("expected LinkPttn"),
        }
    }

    #[test]
    fn decode_link_pgn_and_cn() {
        let i = cmd([0x20, 0x06, 0, 0, 0, 0, 0x05, 0x11]).decode();
        assert_eq!(
            i,
            NavInstruction::LinkPgn {
                pgn: 0x11,
                hl_bn: 5
            }
        );
        let i = cmd([0x20, 0x07, 0, 0, 0, 0, 0x06, 0x42]).decode();
        assert_eq!(i, NavInstruction::LinkCn { cn: 0x42, hl_bn: 6 });
    }

    #[test]
    fn decode_link_invalid_nibble() {
        // cmd-nibble = 2 is invalid per Link spec.
        assert_eq!(
            cmd([0x20, 0x02, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Invalid
        );
    }

    // -----------------------------------------------------------------
    // Type 1 jump / call family.
    // -----------------------------------------------------------------

    #[test]
    fn decode_exit() {
        // byte0 = 0x30 (type=1, byte0[4]=1), byte1=0x01 → Exit.
        assert_eq!(
            cmd([0x30, 0x01, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Exit
        );
    }

    #[test]
    fn decode_jump_tt_and_vts_tt() {
        let i = cmd([0x30, 0x02, 0, 0, 0, 0x07, 0, 0]).decode();
        assert_eq!(i, NavInstruction::JumpTT { ttn: 7 });
        let i = cmd([0x30, 0x03, 0, 0, 0, 0x09, 0, 0]).decode();
        assert_eq!(i, NavInstruction::JumpVtsTt { ttn: 9 });
    }

    #[test]
    fn decode_jump_vts_ptt() {
        // ttn in byte 5; pttn 10-bit in byte 2 low 2 bits + byte 3.
        // pttn = 0x205 → byte2 low = 2, byte3 = 5.
        let i = cmd([0x30, 0x05, 0x02, 0x05, 0, 0x04, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::JumpVtsPtt {
                ttn: 4,
                pttn: 0x205
            }
        );
    }

    #[test]
    fn decode_jump_ss_first_play() {
        // selector byte5 bits 5..4 = 0 → FirstPlay.
        let i = cmd([0x30, 0x06, 0, 0, 0, 0x00, 0, 0]).decode();
        assert_eq!(i, NavInstruction::JumpSs(JumpSSTarget::FirstPlay));
    }

    #[test]
    fn decode_jump_ss_vmgm_menu() {
        // selector 1, menu = 0x0A in byte5 low nibble.
        let i = cmd([0x30, 0x06, 0, 0, 0, 0x1A, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::JumpSs(JumpSSTarget::VmgmMenu { menu: 0x0A })
        );
    }

    #[test]
    fn decode_jump_ss_vtsm() {
        // selector 2, vts in byte4, ttn in byte3, menu in byte5 low.
        let i = cmd([0x30, 0x06, 0, 0x07, 0x02, 0x23, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::JumpSs(JumpSSTarget::VtsmMenu {
                vts: 2,
                ttn: 7,
                menu: 3,
            })
        );
    }

    #[test]
    fn decode_jump_ss_vmgm_pgcn() {
        // selector 3 → VmgmPgcn; pgcn = u16 from bytes 2..3.
        let i = cmd([0x30, 0x06, 0xAB, 0xCD, 0, 0x30, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::JumpSs(JumpSSTarget::VmgmPgcn { pgcn: 0xABCD })
        );
    }

    #[test]
    fn decode_call_ss_first_play() {
        // CallSS: cmd-nibble=8, selector 0, rsm_cell in byte 4.
        let i = cmd([0x30, 0x08, 0, 0, 0x42, 0x00, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::CallSs(CallSSTarget::FirstPlay { rsm_cell: 0x42 })
        );
    }

    #[test]
    fn decode_call_ss_vmgm_pgcn() {
        // selector 3, pgcn in bytes 2..3, rsm_cell in byte 4.
        let i = cmd([0x30, 0x08, 0x11, 0x22, 0x07, 0x30, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::CallSs(CallSSTarget::VmgmPgcn {
                pgcn: 0x1122,
                rsm_cell: 0x07,
            })
        );
    }

    // -----------------------------------------------------------------
    // Type 2 SetSystem family.
    // -----------------------------------------------------------------

    #[test]
    fn decode_set_stn_register_form() {
        // byte0=0x41 (type=2, direct=0, sub=1) — SetSTN register form.
        // af set (byte3 bit 7), audio_src=4 (byte3 low nibble).
        // sf set, subpic_src=5. nf cleared, angle_src=0.
        let i = cmd([0x41, 0x00, 0, 0x84, 0x85, 0x00, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetStn {
                direct: false,
                af: true,
                audio_src: 4,
                sf: true,
                subpic_src: 5,
                nf: false,
                angle_src: 0,
            }
        );
    }

    #[test]
    fn decode_set_stn_immediate_form() {
        // byte0=0x51 (type=2, direct=1, sub=1) — SetSTN immediate.
        // 7-bit aval/sval/nval span byte 3/4/5 low 7 bits; flags in
        // bit 7 of each.
        let i = cmd([0x51, 0x00, 0, 0x80 | 0x12, 0x80 | 0x34, 0x80 | 0x56, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetStn {
                direct: true,
                af: true,
                audio_src: 0x12,
                sf: true,
                subpic_src: 0x34,
                nf: true,
                angle_src: 0x56,
            }
        );
    }

    #[test]
    fn decode_set_nvtmr_register_form() {
        // byte0=0x42 (type=2, direct=0, sub=2) — SetNVTMR register.
        // src = GPRM 3 (byte3); pgcn = 0x1234 (bytes 4..5).
        let i = cmd([0x42, 0, 0, 0x03, 0x12, 0x34, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetNvtmr {
                src: Operand::Register(Register::Gprm(3)),
                pgcn: 0x1234,
            }
        );
    }

    #[test]
    fn decode_set_nvtmr_immediate_form() {
        // byte0=0x52 (direct, sub=2); immediate from bytes 2..3.
        let i = cmd([0x52, 0, 0xAA, 0xBB, 0x12, 0x34, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetNvtmr {
                src: Operand::Immediate(0xAABB),
                pgcn: 0x1234,
            }
        );
    }

    #[test]
    fn decode_set_gprmmd_with_counter() {
        // byte0=0x43 (register form, sub=3); src=GPRM 6 (byte3);
        // dst=GPRM 9 (byte5 low nibble); mf set (byte4 bit 7).
        let i = cmd([0x43, 0, 0, 0x06, 0x80, 0x09, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetGprmMd {
                src: Operand::Register(Register::Gprm(6)),
                dst: Register::Gprm(9),
                counter: true,
            }
        );
    }

    #[test]
    fn decode_set_amxmd_immediate() {
        // byte0=0x54 (direct, sub=4); imm = bytes 4..5.
        let i = cmd([0x54, 0, 0, 0, 0x01, 0x23, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetAmxMd {
                src: Operand::Immediate(0x0123),
            }
        );
    }

    #[test]
    fn decode_set_hlbtnn_register() {
        // byte0=0x46 (register, sub=6); src=GPRM 7 (byte5 low nibble).
        let i = cmd([0x46, 0, 0, 0, 0, 0x07, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetHlBtnn {
                src: Operand::Register(Register::Gprm(7)),
            }
        );
    }

    #[test]
    fn decode_setsystem_invalid_subcode() {
        // sub=5 is reserved per the SetSystem column.
        assert_eq!(
            cmd([0x45, 0, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Invalid
        );
    }

    // -----------------------------------------------------------------
    // Type 3 Set family.
    // -----------------------------------------------------------------

    #[test]
    fn decode_set_add_register_form() {
        // byte0=0x63 (type=3, direct=0, sub=3=add); dst=GPRM 4 (byte3
        // low nibble); src=GPRM 9 (byte5).
        let i = cmd([0x63, 0, 0, 0x04, 0, 0x09, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::Set {
                op: SetOp::Add,
                dst: Register::Gprm(4),
                src: Operand::Register(Register::Gprm(9)),
            }
        );
    }

    #[test]
    fn decode_set_mov_immediate_form() {
        // byte0=0x71 (type=3, direct=1, sub=1=mov); dst=GPRM 0; imm
        // from bytes 4..5.
        let i = cmd([0x71, 0, 0, 0x00, 0xFF, 0xEE, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::Set {
                op: SetOp::Mov,
                dst: Register::Gprm(0),
                src: Operand::Immediate(0xFFEE),
            }
        );
    }

    #[test]
    fn decode_set_invalid_subcode() {
        // sub = 0 (None) and sub = C..F → invalid.
        assert_eq!(
            cmd([0x60, 0, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Invalid
        );
        assert_eq!(
            cmd([0x6C, 0, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Invalid
        );
        assert_eq!(
            cmd([0x6F, 0, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Invalid
        );
    }

    #[test]
    fn decode_set_src_routes_to_sprm() {
        // Register-form, src byte = 0x82 → SPRM 2 (SPSTN).
        let i = cmd([0x61, 0, 0, 0x05, 0, 0x82, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::Set {
                op: SetOp::Mov,
                dst: Register::Gprm(5),
                src: Operand::Register(Register::Sprm(2)),
            }
        );
    }

    // -----------------------------------------------------------------
    // Types 4..6 compound classifiers + Type 7.
    // -----------------------------------------------------------------

    #[test]
    fn decode_type4_setclnk_classifier() {
        // byte0=0x83 (type=4, direct=0, set-op=3=add); byte1=0x12
        // (cmp-direct=0, cmp-op=1=BC, link nibble 2).
        let i = cmd([0x83, 0x12, 0x07, 0, 0, 0, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::SetCLnk {
                set_op: SetOp::Add,
                cmp_op: CmpOp::Bc,
                scr: Register::Gprm(7),
            }
        );
    }

    #[test]
    fn decode_type5_csetclnk_classifier() {
        let i = cmd([0xA2, 0x20, 0, 0, 0, 0, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::CSetCLnk {
                set_op: SetOp::Swp,
                cmp_op: CmpOp::Eq,
            }
        );
    }

    #[test]
    fn decode_type6_cmpsetlnk_classifier() {
        let i = cmd([0xC4, 0x70, 0, 0, 0, 0, 0, 0]).decode();
        assert_eq!(
            i,
            NavInstruction::CmpSetLnk {
                set_op: SetOp::Sub,
                cmp_op: CmpOp::Lt,
            }
        );
    }

    #[test]
    fn decode_type7_unknown() {
        // Any byte0 with top three bits = 7 → Unknown.
        assert_eq!(
            cmd([0xE0, 0, 0, 0, 0, 0, 0, 0]).decode(),
            NavInstruction::Unknown
        );
        assert_eq!(
            cmd([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).decode(),
            NavInstruction::Unknown
        );
    }

    // -----------------------------------------------------------------
    // Round-trip integration — a NavCommand sourced from PgcCommandTable.
    // -----------------------------------------------------------------

    #[test]
    fn decoded_from_navcommand_default() {
        // Default NavCommand (all zeros) decodes to NOP — the
        // canonical no-op encoding per the spec's first row.
        let nc = NavCommand::default();
        assert_eq!(nc.decode(), NavInstruction::Nop);
    }
}
