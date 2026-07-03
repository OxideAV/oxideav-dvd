//! Playback-navigation engine — composes the parsed IFO structures,
//! the `nav` disassembler, and the `vm` interpreter into player-level
//! navigation decisions.
//!
//! The `vm` module executes one command list and surfaces a typed
//! [`VmAction`]; everything *around* that — which domain playback is
//! in, whether the surfaced jump/call is legal from that domain, how
//! a `Link` resolves against the current PGC's cell layout, and how a
//! title/chapter/menu number resolves to a concrete PGC — lives here.
//!
//! Clean-room per:
//!
//! - `docs/container/dvd/application/mpucoder-vmi-jmp.html` — the
//!   four domain-transition tables (rows 1..13 plus the explicit
//!   "not allowed" rows) encoded by [`transition_permitted`].
//! - `docs/container/dvd/application/mpucoder-vmi-sum.html` — the
//!   instruction-family summary ("Link instructions, used for going
//!   from one video segment to another *within the same domain*";
//!   "Jump and Call instructions, used for going to another domain").
//! - `docs/container/dvd/application/mpucoder-pgc.html` — the PGC
//!   header linkage fields (next / previous / goup PGCN) and the
//!   program-map / cell-table layout the link resolver walks.
//! - `docs/container/dvd/application/stnsoft-vmindx.html` — the
//!   instruction index naming the 13 `Link*` destinations.

use crate::nav::{CallSSTarget, JumpSSTarget};
use crate::vm::{LinkAction, Vm, VmAction};

// =====================================================================
// Domain — where playback currently is.
// =====================================================================

/// The four DVD-Video playback domains per the transition tables of
/// `mpucoder-vmi-jmp.html`.
///
/// Every PGC a player can execute lives in exactly one of these
/// domains, and the jump/call instruction families are only permitted
/// to cross between them along the table's arrows (see
/// [`transition_permitted`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// First-Play domain — the single `FP_PGC` a player enters at
    /// disc insertion (fetched via `DvdDisc::parse_fp_pgc`).
    FirstPlay,
    /// Video Manager (VMG) domain — the title menu and any other PGC
    /// carried by `VMGM_PGCI_UT` (or addressed by number via
    /// `JumpSS VMGM pgcn`).
    VideoManager,
    /// VTS Menu domain — the per-title-set menus carried by
    /// `VTSM_PGCI_UT` (root / sub-picture / audio / angle / PTT).
    VtsMenu,
    /// VTS Title domain — the feature content itself (`VTS_PGCI`
    /// program chains).
    VtsTitle,
}

impl Domain {
    /// `true` for the two menu-space domains (Video Manager + VTS
    /// Menu). Menu-domain PGCs draw button highlights and route user
    /// input; title-domain PGCs play feature content.
    #[inline]
    pub fn is_menu(self) -> bool {
        matches!(self, Domain::VideoManager | Domain::VtsMenu)
    }
}

/// The domain a transfer-class [`VmAction`] lands in, or `None` when
/// the action does not change domains (Continue / Break / Link /
/// SetNavTimer / …).
///
/// `Resume` reports `None` too: the destination of an `RSM` is
/// "wherever the matching `CallSS` was issued", which only the
/// engine's own resume bookkeeping knows — see
/// [`mpucoder-vmi-sum.html`]'s description of RSM ("resumes playback
/// from the point of interruption").
pub fn target_domain(action: &VmAction) -> Option<Domain> {
    match action {
        VmAction::JumpTitle { .. }
        | VmAction::JumpVtsTitle { .. }
        | VmAction::JumpVtsPtt { .. } => Some(Domain::VtsTitle),
        VmAction::JumpSs(t) => Some(match t {
            JumpSSTarget::FirstPlay => Domain::FirstPlay,
            JumpSSTarget::VmgmMenu { .. } | JumpSSTarget::VmgmPgcn { .. } => Domain::VideoManager,
            JumpSSTarget::VtsmMenu { .. } => Domain::VtsMenu,
        }),
        VmAction::CallSs(t) => Some(match t {
            CallSSTarget::FirstPlay { .. } => Domain::FirstPlay,
            CallSSTarget::VmgmMenu { .. } | CallSSTarget::VmgmPgcn { .. } => Domain::VideoManager,
            CallSSTarget::VtsmMenu { .. } => Domain::VtsMenu,
        }),
        _ => None,
    }
}

/// Check a transfer-class [`VmAction`] against the domain-transition
/// tables of `mpucoder-vmi-jmp.html`.
///
/// `from` is the domain of the PGC whose command list produced the
/// action; `current_vts` is the 1-based VTS number that PGC belongs
/// to (`0` when playback is in the First-Play or Video Manager
/// domain, which live outside any title set).
///
/// The encoding follows the four tables row by row:
///
/// | From        | Permitted                                                    |
/// |-------------|--------------------------------------------------------------|
/// | First Play  | `JumpSS` VMGM (menu/pgcn), `JumpSS` VTSM, `JumpTT` (rows 1–3)|
/// | VMG         | `JumpSS` FP, `JumpSS` VTSM, `JumpTT`, `RSM` (rows 4–6)       |
/// | VTS Menu    | `JumpSS` FP, `JumpSS` VMGM, `JumpVTS_TT`, `JumpVTS_PTT`, `RSM` (rows 7–9) |
/// | VTS Title   | `CallSS` FP, `CallSS` VMGM, `CallSS` VTSM, `JumpVTS_TT`, `JumpVTS_PTT` (rows 10–13) |
///
/// The two explicit "not allowed" rows are honoured: a `JumpSS VTSM`
/// naming a *different* VTS than `current_vts` is rejected from the
/// VTS Menu domain ("another VTS menu domain — not allowed"), while
/// the same-VTS form is treated as intra-domain movement and passes.
/// `JumpVTS_TT` / `JumpVTS_PTT` are same-VTS by construction (they
/// carry no VTS operand), so the "another VTS title domain — not
/// allowed" row needs no extra check.
///
/// Non-transfer actions (`Continue` / `Break` / `Exit` / `Link` /
/// `SetNavTimer` / `NoOpRaw`) are always permitted — they never cross
/// a domain boundary (`mpucoder-vmi-sum.html`: Link moves "within the
/// same domain").
pub fn transition_permitted(from: Domain, action: &VmAction, current_vts: u8) -> bool {
    match action {
        // Intra-domain / non-transfer actions are always fine.
        VmAction::Continue
        | VmAction::Break
        | VmAction::Exit
        | VmAction::Link(_)
        | VmAction::SetNavTimer { .. }
        | VmAction::NoOpRaw(_) => true,

        VmAction::JumpTitle { .. } => {
            // Rows 3 + 6 — JumpTT is legal from First Play and VMG
            // only. From the VTS domains the table routes through
            // JumpVTS_TT instead.
            matches!(from, Domain::FirstPlay | Domain::VideoManager)
        }

        VmAction::JumpVtsTitle { .. } | VmAction::JumpVtsPtt { .. } => {
            // Rows 9 + 13 — same-VTS title jumps from the VTS menu
            // or VTS title domain.
            matches!(from, Domain::VtsMenu | Domain::VtsTitle)
        }

        VmAction::JumpSs(t) => match from {
            Domain::FirstPlay => match t {
                // Rows 1 + 2.
                JumpSSTarget::VmgmMenu { .. }
                | JumpSSTarget::VmgmPgcn { .. }
                | JumpSSTarget::VtsmMenu { .. } => true,
                JumpSSTarget::FirstPlay => false,
            },
            Domain::VideoManager => match t {
                // Rows 4 + 5.
                JumpSSTarget::FirstPlay | JumpSSTarget::VtsmMenu { .. } => true,
                // Intra-VMG movement is Link territory, not JumpSS.
                JumpSSTarget::VmgmMenu { .. } | JumpSSTarget::VmgmPgcn { .. } => false,
            },
            Domain::VtsMenu => match t {
                // Rows 7 + 8.
                JumpSSTarget::FirstPlay
                | JumpSSTarget::VmgmMenu { .. }
                | JumpSSTarget::VmgmPgcn { .. } => true,
                // "another VTS menu domain — not allowed"; the
                // same-VTS form stays inside the current domain.
                JumpSSTarget::VtsmMenu { vts, .. } => *vts == current_vts,
            },
            // The VTS Title table (rows 10..13) has no JumpSS row at
            // all — title-domain exits are CallSS so playback can
            // resume.
            Domain::VtsTitle => false,
        },

        VmAction::CallSs(_) => {
            // Rows 10..12 — CallSS is exclusively a VTS-Title-domain
            // exit. None of the other three tables lists a CallSS.
            from == Domain::VtsTitle
        }

        VmAction::Resume(_) => {
            // Rows 6 + 9 — "(resume)" appears in the VMG and VTS
            // Menu tables only; there is nothing to resume *from*
            // within First Play, and the title domain is what RSM
            // resumes *to*.
            matches!(from, Domain::VideoManager | Domain::VtsMenu)
        }
    }
}

// =====================================================================
// Link resolution — turning a Type-1 LinkAction into a destination.
// =====================================================================

/// Where playback currently sits inside a PGC — the state a Type-1
/// `Link*` destination is computed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgcPosition {
    /// 1-based program-chain number of the current PGC within its
    /// domain's PGC table.
    pub pgcn: u16,
    /// 1-based program number within the PGC.
    pub program: u8,
    /// 1-based cell number within the PGC.
    pub cell: u8,
}

/// The resolved destination of a Type-1 [`LinkAction`].
///
/// Per `mpucoder-vmi-sum.html` the Link family moves playback
/// "within the same domain" — so every destination is expressed in
/// terms of the current domain's PGC table: a cell inside the
/// current PGC, the current PGC's post-command list, another PGC by
/// number, or a chapter (which the caller resolves through
/// `VTS_PTT_SRPT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    /// `LinkNoLink` (subset `Nop`) — no transfer; the surrounding
    /// command list keeps walking.
    Continue,
    /// Present cell `cell` of the **current** PGC (already
    /// angle-resolved via [`Pgc::cell_for_angle`]).
    PlayCell { cell: u8 },
    /// Enter the current PGC's post-command list (`LinkTailPGC`, or
    /// a next-cell / next-PG walk that ran off the end of the PGC).
    PostCommands,
    /// Load PGC `pgcn` of the current domain from its pre-commands
    /// (`LinkPGCN` / `LinkTopPGC` / `LinkNextPGC` / `LinkPrevPGC` /
    /// `LinkGoupPGC`).
    OtherPgc { pgcn: u16 },
    /// `LinkPTTN pttn` — a chapter of the current title; the caller
    /// resolves it to a `(PGCN, PGN)` pair via `VtsPttSrpt::ptt`.
    Chapter { pttn: u16 },
    /// The `RSM` subset — pop the engine's resume bookkeeping.
    Resume,
    /// The link names a destination the PGC doesn't carry (a zero
    /// next / previous / goup PGCN, an out-of-range program or cell,
    /// or one of the spec's invalid subset codes).
    NoDestination,
}

/// The highlight-button override a [`LinkAction`] carries, if any.
///
/// Per `mpucoder-vmi.html` most Link forms embed a 6-bit `hl_bn`
/// ("Most Link commands can also specify a new highlight selection"
/// — `mpucoder-vmi-sum.html`); `0` means "leave SPRM 8 alone". The
/// caller stores `Some(n)` into SPRM 8's button field (bits 15..10)
/// before transferring.
pub fn link_highlight_button(link: &LinkAction) -> Option<u8> {
    let hl = match link {
        LinkAction::Subset { hl_bn, .. } => *hl_bn,
        LinkAction::Pttn { hl_bn, .. } => *hl_bn,
        LinkAction::Pgn { hl_bn, .. } => *hl_bn,
        LinkAction::Cn { hl_bn, .. } => *hl_bn,
        // LinkPGCN carries no button field per the opcode table.
        LinkAction::Pgcn { .. } => 0,
    };
    (hl != 0).then_some(hl)
}

/// Resolve a Type-1 [`LinkAction`] against the current PGC.
///
/// `pgc` is the PGC `pos` points into, and `angle` is the current
/// SPRM 3 camera angle (used to angle-resolve any destination cell
/// that opens an angle block). Destination semantics per the 13
/// `Link*` rows of `stnsoft-vmindx.html` + `mpucoder-vmi.html`:
///
/// - The cell-level subsets (`Top` / `Next` / `PrevCell`) move
///   relative to `pos.cell`; a next-cell walk past the PGC's final
///   cell falls through to [`LinkOutcome::PostCommands`] (the same
///   place sequential playback goes), and a prev-cell at cell 1
///   clamps to cell 1.
/// - The program-level subsets move relative to `pos.program` and
///   land on the destination program's entry cell; `LinkNextPG`
///   past the final program falls through to post-commands and
///   `LinkPrevPG` clamps at program 1.
/// - The PGC-level subsets follow the PGC header's linkage words
///   (`next_pgcn` / `prev_pgcn` / `goup_pgcn`); an unauthored (zero)
///   word yields [`LinkOutcome::NoDestination`].
/// - The numbered forms (`LinkPGCN` / `LinkPTTN` / `LinkPGN` /
///   `LinkCN`) address their destination directly.
pub fn resolve_link(
    pgc: &crate::ifo::Pgc,
    pos: PgcPosition,
    link: &LinkAction,
    angle: u8,
) -> LinkOutcome {
    use crate::nav::LinkSubset;

    match link {
        LinkAction::Subset { subset, .. } => match subset {
            LinkSubset::Nop => LinkOutcome::Continue,
            LinkSubset::LinkTopCell => LinkOutcome::PlayCell { cell: pos.cell },
            LinkSubset::LinkNextCell => match pgc.next_cell(pos.cell, angle) {
                Some(cell) => LinkOutcome::PlayCell { cell },
                None => LinkOutcome::PostCommands,
            },
            LinkSubset::LinkPrevCell => {
                let prev = pos.cell.saturating_sub(1).max(1);
                LinkOutcome::PlayCell {
                    cell: pgc.cell_for_angle(prev, angle),
                }
            }
            LinkSubset::LinkTopPG => match pgc.program_entry_cell(pos.program) {
                Some(cell) => LinkOutcome::PlayCell {
                    cell: pgc.cell_for_angle(cell, angle),
                },
                None => LinkOutcome::NoDestination,
            },
            LinkSubset::LinkNextPG => {
                if pos.program >= pgc.number_of_programs {
                    LinkOutcome::PostCommands
                } else {
                    match pgc.program_entry_cell(pos.program + 1) {
                        Some(cell) => LinkOutcome::PlayCell {
                            cell: pgc.cell_for_angle(cell, angle),
                        },
                        None => LinkOutcome::NoDestination,
                    }
                }
            }
            LinkSubset::LinkPrevPG => {
                let prev = pos.program.saturating_sub(1).max(1);
                match pgc.program_entry_cell(prev) {
                    Some(cell) => LinkOutcome::PlayCell {
                        cell: pgc.cell_for_angle(cell, angle),
                    },
                    None => LinkOutcome::NoDestination,
                }
            }
            LinkSubset::LinkTopPGC => LinkOutcome::OtherPgc { pgcn: pos.pgcn },
            LinkSubset::LinkNextPGC => match pgc.next_pgcn {
                0 => LinkOutcome::NoDestination,
                pgcn => LinkOutcome::OtherPgc { pgcn },
            },
            LinkSubset::LinkPrevPGC => match pgc.prev_pgcn {
                0 => LinkOutcome::NoDestination,
                pgcn => LinkOutcome::OtherPgc { pgcn },
            },
            LinkSubset::LinkGoupPGC => match pgc.goup_pgcn {
                0 => LinkOutcome::NoDestination,
                pgcn => LinkOutcome::OtherPgc { pgcn },
            },
            LinkSubset::LinkTailPGC => LinkOutcome::PostCommands,
            LinkSubset::Rsm => LinkOutcome::Resume,
            LinkSubset::Invalid(_) => LinkOutcome::NoDestination,
        },
        LinkAction::Pgcn { pgcn } => match pgcn {
            0 => LinkOutcome::NoDestination,
            pgcn => LinkOutcome::OtherPgc { pgcn: *pgcn },
        },
        LinkAction::Pttn { pttn, .. } => match pttn {
            0 => LinkOutcome::NoDestination,
            pttn => LinkOutcome::Chapter { pttn: *pttn },
        },
        LinkAction::Pgn { pgn, .. } => match pgc.program_entry_cell(*pgn) {
            Some(cell) => LinkOutcome::PlayCell {
                cell: pgc.cell_for_angle(cell, angle),
            },
            None => LinkOutcome::NoDestination,
        },
        LinkAction::Cn { cn, .. } => {
            if *cn >= 1 && *cn <= pgc.number_of_cells {
                LinkOutcome::PlayCell {
                    cell: pgc.cell_for_angle(*cn, angle),
                }
            } else {
                LinkOutcome::NoDestination
            }
        }
    }
}

// =====================================================================
// PgcRunner — one PGC's playback state machine.
// =====================================================================

/// One player-visible step of a PGC's playback.
///
/// [`PgcRunner::next_event`] drives the spec's per-PGC flow — pre
/// commands → cells (each optionally followed by its cell command) →
/// post commands — and emits one of these each time something the
/// playback engine must act on happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackEvent {
    /// Present cell `cell` (1-based, already angle-resolved): demux
    /// the VOB-relative sector span `first_sector ..= last_sector`
    /// (the C_PBI `first VOBU start sector` / `last VOBU end sector`
    /// words) and honour the cell's still time afterwards.
    PlayCell {
        cell: u8,
        /// 1-based program the cell belongs to.
        program: u8,
        first_sector: u32,
        last_sector: u32,
        still: crate::ifo::StillTime,
    },
    /// All cells are done and the PGC header's still-time byte
    /// (offset `0x00A2`) is non-zero — freeze the final frame for
    /// the given duration before the post commands run.
    PgcStill { still: crate::ifo::StillTime },
    /// A `SetNVTMR` executed — arm a wall-clock timer that fires a
    /// `LinkPGCN(pgcn)` after `seconds`. The command list resumes on
    /// the next [`PgcRunner::next_event`] call.
    NavTimer { seconds: u16, pgcn: u16 },
    /// Control transfers to PGC `pgcn` of the **same domain** (a
    /// `LinkPGCN` / `LinkTopPGC` / next-PGCN chain / …). The engine
    /// builds a fresh [`PgcRunner`] for it.
    NextPgc { pgcn: u16 },
    /// Control transfers to chapter `pttn` of the current title
    /// (`LinkPTTN`) — resolve via `VtsPttSrpt::ptt`.
    Chapter { pttn: u16 },
    /// A cross-domain transfer (Jump / Call / Resume / Exit)
    /// surfaced out of a command list — the disc-level engine owns
    /// these (check it with [`transition_permitted`] first).
    Transfer(VmAction),
    /// The PGC ran to completion with no follow-on PGC (post
    /// commands fell off the end and the header's next-PGCN word is
    /// zero). Repeated calls keep returning this.
    Finished,
}

/// Where the runner currently is inside the PGC flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerState {
    /// Executing the pre-command list from index `pc`.
    Pre { pc: usize },
    /// About to present cell `cell`.
    PlayCell { cell: u8 },
    /// Cell `cell` has been presented; run its cell command (unless
    /// already done) and advance.
    AfterCell { cell: u8, command_done: bool },
    /// Executing the post-command list from index `pc`.
    Post { pc: usize },
    /// Terminal.
    Finished,
}

/// Drives one PGC through its spec-ordered playback flow against a
/// caller-owned [`Vm`].
///
/// The runner owns *intra-PGC* sequencing only: pre commands, the
/// angle-aware cell walk (camera angle = SPRM 3 at each step), cell
/// commands, the PGC still time, post commands, and the next-PGCN
/// chain. Everything that leaves the PGC — other PGCs, chapters,
/// cross-domain jumps — is surfaced as a [`PlaybackEvent`] for the
/// disc-level engine, so the runner never needs disc I/O.
///
/// Highlight-button overrides carried by Link forms (`hl_bn`) are
/// applied to SPRM 8 (button number in bits 15..10 per
/// `mpucoder-sprm.html`) before the transfer resolves.
#[derive(Debug)]
pub struct PgcRunner<'a> {
    pgc: &'a crate::ifo::Pgc,
    /// 1-based PGCN of `pgc` within its domain's table.
    pgcn: u16,
    state: RunnerState,
}

impl<'a> PgcRunner<'a> {
    /// Start a runner at the top of `pgc` (its pre-command list).
    /// `pgcn` is the 1-based number of `pgc` within its domain's PGC
    /// table (used to resolve `LinkTopPGC` back to itself).
    pub fn new(pgc: &'a crate::ifo::Pgc, pgcn: u16) -> Self {
        Self {
            pgc,
            pgcn,
            state: RunnerState::Pre { pc: 0 },
        }
    }

    /// Start a runner directly at cell `cell` (1-based), skipping the
    /// pre-command list — the entry shape for a chapter jump
    /// (`JumpVTS_PTT` lands on a program's entry cell, not on the
    /// PGC's pre commands) or a `LinkCN` re-entry.
    pub fn new_at_cell(pgc: &'a crate::ifo::Pgc, pgcn: u16, cell: u8) -> Self {
        Self {
            pgc,
            pgcn,
            state: if cell >= 1 && cell <= pgc.number_of_cells {
                RunnerState::PlayCell { cell }
            } else {
                RunnerState::Post { pc: 0 }
            },
        }
    }

    /// The current camera angle per SPRM 3, defaulting to 1 when the
    /// register holds an out-of-range value.
    fn angle(vm: &Vm) -> u8 {
        vm.regs.angle_number().unwrap_or(1)
    }

    /// Apply a Link's highlight-button override to SPRM 8 (button
    /// number lives in bits 15..10).
    fn apply_hl_btn(vm: &mut Vm, link: &LinkAction) {
        if let Some(btn) = link_highlight_button(link) {
            vm.regs
                .set_sprm(crate::vm::SPRM_HL_BTNN, u16::from(btn) << 10);
        }
    }

    /// The position a Link inside the current state resolves against.
    fn position(&self, cell: u8) -> PgcPosition {
        PgcPosition {
            pgcn: self.pgcn,
            program: self.pgc.program_containing_cell(cell).unwrap_or(1),
            cell,
        }
    }

    /// Advance the PGC flow until something player-visible happens.
    ///
    /// Each call returns exactly one [`PlaybackEvent`]. The caller
    /// keeps calling until it sees a transfer-class event
    /// ([`PlaybackEvent::NextPgc`] / [`PlaybackEvent::Chapter`] /
    /// [`PlaybackEvent::Transfer`]) or [`PlaybackEvent::Finished`].
    pub fn next_event(&mut self, vm: &mut Vm) -> PlaybackEvent {
        loop {
            match self.state {
                RunnerState::Pre { pc } => {
                    let list: &[crate::ifo::NavCommand] = self
                        .pgc
                        .commands
                        .as_ref()
                        .map(|c| c.pre.as_slice())
                        .unwrap_or(&[]);
                    let (action, npc) = vm.run_list_from(list, pc);
                    match self.handle_list_action(vm, action, npc, /*current_cell=*/ 1) {
                        ListVerdict::EnterCells => self.enter_cells(vm),
                        ListVerdict::Emit(ev) => return ev,
                        ListVerdict::Moved => {}
                    }
                }
                RunnerState::PlayCell { cell } => {
                    let Some(info) = usize::from(cell)
                        .checked_sub(1)
                        .and_then(|i| self.pgc.cells.get(i))
                    else {
                        // Malformed cell index — fall through to post.
                        self.state = RunnerState::Post { pc: 0 };
                        continue;
                    };
                    self.state = RunnerState::AfterCell {
                        cell,
                        command_done: false,
                    };
                    return PlaybackEvent::PlayCell {
                        cell,
                        program: self.pgc.program_containing_cell(cell).unwrap_or(1),
                        first_sector: info.first_vobu_start_sector,
                        last_sector: info.last_vobu_end_sector,
                        still: info.still(),
                    };
                }
                RunnerState::AfterCell { cell, command_done } => {
                    if !command_done {
                        self.state = RunnerState::AfterCell {
                            cell,
                            command_done: true,
                        };
                        // Run the cell command, if the cell names one.
                        let cmd_index = self
                            .pgc
                            .cells
                            .get(usize::from(cell) - 1)
                            .map(|c| u16::from(c.cell_command))
                            .unwrap_or(0);
                        if cmd_index != 0 {
                            if let Some(ins) = self
                                .pgc
                                .commands
                                .as_ref()
                                .and_then(|c| c.cell_instruction(cmd_index))
                            {
                                let action = vm.step(ins);
                                match self.handle_list_action(vm, action, 0, cell) {
                                    // A cell command has no list to
                                    // fall back into: "ran clean"
                                    // just means "advance to the
                                    // next cell".
                                    ListVerdict::EnterCells => {}
                                    ListVerdict::Emit(ev) => return ev,
                                    ListVerdict::Moved => {}
                                }
                                continue;
                            }
                        }
                        continue;
                    }
                    // Advance the angle-aware cell walk.
                    match self.pgc.next_cell(cell, Self::angle(vm)) {
                        Some(next) => self.state = RunnerState::PlayCell { cell: next },
                        None => {
                            self.state = RunnerState::Post { pc: 0 };
                            let still = self.pgc.still();
                            if still != crate::ifo::StillTime::None {
                                return PlaybackEvent::PgcStill { still };
                            }
                        }
                    }
                }
                RunnerState::Post { pc } => {
                    let list: &[crate::ifo::NavCommand] = self
                        .pgc
                        .commands
                        .as_ref()
                        .map(|c| c.post.as_slice())
                        .unwrap_or(&[]);
                    let (action, npc) = vm.run_list_from(list, pc);
                    match self.handle_list_action(vm, action, npc, self.pgc.number_of_cells.max(1))
                    {
                        ListVerdict::EnterCells => {
                            // Post list ran clean (or Break): follow
                            // the header's next-PGCN chain.
                            self.state = RunnerState::Finished;
                            match self.pgc.next_pgcn {
                                0 => return PlaybackEvent::Finished,
                                pgcn => return PlaybackEvent::NextPgc { pgcn },
                            }
                        }
                        ListVerdict::Emit(ev) => return ev,
                        ListVerdict::Moved => {}
                    }
                }
                RunnerState::Finished => return PlaybackEvent::Finished,
            }
        }
    }

    /// Enter the cell phase from the top (after pre commands).
    fn enter_cells(&mut self, vm: &Vm) {
        match self.pgc.first_cell(Self::angle(vm)) {
            Some(cell) => self.state = RunnerState::PlayCell { cell },
            None => self.state = RunnerState::Post { pc: 0 },
        }
    }

    /// Common handling for a [`VmAction`] surfaced by a command list
    /// (or a lone cell command). `npc` is the PC the list stopped at;
    /// `current_cell` anchors Link resolution.
    fn handle_list_action(
        &mut self,
        vm: &mut Vm,
        action: VmAction,
        npc: usize,
        current_cell: u8,
    ) -> ListVerdict {
        match action {
            // Ran off the end of the list, or Break = "exit the
            // pre / post command section" (mpucoder-vmi-sum.html).
            VmAction::Continue | VmAction::Break => ListVerdict::EnterCells,
            VmAction::SetNavTimer { seconds, pgcn } => {
                // Informational: surface it, then resume the list
                // after the SetNVTMR word.
                self.note_resume(npc.saturating_add(1));
                ListVerdict::Emit(PlaybackEvent::NavTimer { seconds, pgcn })
            }
            VmAction::Link(link) => {
                Self::apply_hl_btn(vm, &link);
                let pos = self.position(current_cell);
                match resolve_link(self.pgc, pos, &link, Self::angle(vm)) {
                    LinkOutcome::Continue | LinkOutcome::NoDestination => {
                        // No transfer (LinkNoLink) — or a link into an
                        // unauthored destination, which we skip over
                        // rather than halt on. Resume after the word.
                        self.note_resume(npc.saturating_add(1));
                        ListVerdict::Moved
                    }
                    LinkOutcome::PlayCell { cell } => {
                        self.state = RunnerState::PlayCell { cell };
                        ListVerdict::Moved
                    }
                    LinkOutcome::PostCommands => {
                        self.state = RunnerState::Post { pc: 0 };
                        ListVerdict::Moved
                    }
                    LinkOutcome::OtherPgc { pgcn } => {
                        self.state = RunnerState::Finished;
                        ListVerdict::Emit(PlaybackEvent::NextPgc { pgcn })
                    }
                    LinkOutcome::Chapter { pttn } => {
                        self.state = RunnerState::Finished;
                        ListVerdict::Emit(PlaybackEvent::Chapter { pttn })
                    }
                    LinkOutcome::Resume => {
                        // Unreachable through Vm::step (which pops the
                        // RSM stack itself), but map it faithfully.
                        self.state = RunnerState::Finished;
                        ListVerdict::Emit(PlaybackEvent::Transfer(action))
                    }
                }
            }
            // Cross-domain transfers + Exit + RSM end this runner.
            VmAction::Exit
            | VmAction::JumpTitle { .. }
            | VmAction::JumpVtsTitle { .. }
            | VmAction::JumpVtsPtt { .. }
            | VmAction::JumpSs(_)
            | VmAction::CallSs(_)
            | VmAction::Resume(_) => {
                self.state = RunnerState::Finished;
                ListVerdict::Emit(PlaybackEvent::Transfer(action))
            }
            // Structurally unknown word — skip it and resume.
            VmAction::NoOpRaw(_) => {
                self.note_resume(npc.saturating_add(1));
                ListVerdict::Moved
            }
        }
    }

    /// If we're inside a command list, arrange to resume it at `pc`.
    /// (A lone cell command has nowhere to resume into — the caller
    /// advances the cell walk instead.)
    fn note_resume(&mut self, pc: usize) {
        match self.state {
            RunnerState::Pre { .. } => self.state = RunnerState::Pre { pc },
            RunnerState::Post { .. } => self.state = RunnerState::Post { pc },
            _ => {}
        }
    }
}

// =====================================================================
// Menu interaction — D-pad navigation + button activation.
// =====================================================================

/// A D-pad direction over a menu's button table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonMove {
    Up,
    Down,
    Left,
    Right,
}

/// The result of [`select_button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonPress {
    /// The button became the highlighted one (SPRM 8 updated); the
    /// user still has to action it.
    Selected,
    /// The button carries the auto-action flag — selecting it
    /// executed its command immediately, yielding this [`VmAction`].
    AutoAction(VmAction),
    /// No such button in this highlight (0 or past the table).
    Invalid,
}

/// The button number a D-pad move lands on, per the `BTN_IT`
/// adjacency fields (`AJBTN_POSI_UP/DN/LT/RT`,
/// `mpucoder-pci_pkt.html`). An unauthored (zero) or out-of-range
/// adjacency keeps the current selection.
pub fn navigate_button(hli: &crate::vob::HighlightInfo, current: u8, mv: ButtonMove) -> u8 {
    let Some(btn) = usize::from(current)
        .checked_sub(1)
        .and_then(|i| hli.buttons.get(i))
    else {
        return current;
    };
    let dest = match mv {
        ButtonMove::Up => btn.up,
        ButtonMove::Down => btn.down,
        ButtonMove::Left => btn.left,
        ButtonMove::Right => btn.right,
    };
    if dest >= 1 && usize::from(dest) <= hli.buttons.len() {
        dest
    } else {
        current
    }
}

/// The button to highlight when a VOBU's highlight comes up:
/// a non-zero `fosl_btnn` (force-select, `HLI_GI` per
/// `mpucoder-pci_pkt.html`) overrides everything; otherwise the
/// current SPRM 8 selection is kept when it names a real button and
/// clamped to button 1 (the SPRM 8 default) when it doesn't.
pub fn initial_button(hli: &crate::vob::HighlightInfo, current: u8) -> u8 {
    if hli.fosl_btnn != 0 {
        return hli.fosl_btnn;
    }
    if current >= 1 && usize::from(current) <= hli.buttons.len() {
        current
    } else {
        1
    }
}

/// Highlight button `btn`: store it in SPRM 8 (button number in
/// bits 15..10 per `mpucoder-sprm.html`) and, when the `BTN_IT`
/// entry carries the auto-action flag, immediately execute its
/// command ([`activate_button`]).
pub fn select_button(vm: &mut Vm, hli: &crate::vob::HighlightInfo, btn: u8) -> ButtonPress {
    let Some(info) = usize::from(btn)
        .checked_sub(1)
        .and_then(|i| hli.buttons.get(i))
    else {
        return ButtonPress::Invalid;
    };
    vm.regs
        .set_sprm(crate::vm::SPRM_HL_BTNN, u16::from(btn) << 10);
    if info.auto_action {
        match activate_button(vm, hli, btn) {
            Some(action) => ButtonPress::AutoAction(action),
            None => ButtonPress::Selected,
        }
    } else {
        ButtonPress::Selected
    }
}

/// Action button `btn`: decode its 8-byte `BTN_IT` command (the
/// same VM encoding as a PGC command, `mpucoder-pci_pkt.html` +
/// `mpucoder-vmi.html`) and execute it on `vm`, returning the
/// surfaced [`VmAction`]. `None` when the button doesn't exist.
pub fn activate_button(vm: &mut Vm, hli: &crate::vob::HighlightInfo, btn: u8) -> Option<VmAction> {
    let info = usize::from(btn)
        .checked_sub(1)
        .and_then(|i| hli.buttons.get(i))?;
    Some(vm.step(info.command_instruction()))
}

/// Execute the highlight's force-action button (`foac_btnn`,
/// non-zero = the player must action it without user input), if
/// declared. Returns the resulting [`VmAction`].
pub fn forced_action(vm: &mut Vm, hli: &crate::vob::HighlightInfo) -> Option<VmAction> {
    if hli.foac_btnn == 0 {
        return None;
    }
    activate_button(vm, hli, hli.foac_btnn)
}

// =====================================================================
// Static title plan — the non-interactive cell schedule of a title.
// =====================================================================

/// One entry of a static title playback plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedCell {
    /// 1-based PGCN the cell lives in.
    pub pgcn: u16,
    /// 1-based program within that PGC.
    pub program: u8,
    /// 1-based cell number within that PGC.
    pub cell: u8,
    /// C_PBI `first VOBU start sector` (VOB-relative).
    pub first_sector: u32,
    /// C_PBI `last VOBU end sector` (VOB-relative).
    pub last_sector: u32,
}

/// Compute the **static** cell schedule of a VTS-internal title —
/// the sector spans a ripper / muxer demuxes, in presentation order,
/// without executing any navigation commands.
///
/// Starting at the title's entry PGC (per the VTS_PGCI category
/// dword's entry flag + title number), each PGC contributes its
/// angle-`angle` cell walk ([`Pgc::cell_walk`] — angle blocks
/// contribute exactly one cell each), then the walk follows the PGC
/// header's next-PGCN chain (`mpucoder-pgc.html` offset `0x009C`)
/// until it ends (zero) or would revisit a PGC (a navigation loop a
/// static plan must break). PGCs belonging to a *different* title
/// per their category dword stop the chain too — a next-PGCN link
/// out of the title's own PGC set is menu/authoring territory the
/// command-aware [`PgcRunner`] handles.
///
/// `pgci_srp` and `pgcs` are the parallel arrays of `VTS_PGCI`
/// (`Pgci::srp` / `Pgci::pgcs`, or `VtsIfo::pgci_srp` /
/// `VtsIfo::pgcs`). Returns `None` when the title has no entry PGC.
///
/// The interactive path — pre/post/cell commands, stills, menu
/// calls — is [`PgcRunner`]; this plan deliberately ignores them.
pub fn plan_title_cells(
    pgci_srp: &[crate::ifo::PgciSrp],
    pgcs: &[crate::ifo::Pgc],
    vts_ttn: u8,
    angle: u8,
) -> Option<Vec<PlannedCell>> {
    let entry = pgci_srp
        .iter()
        .position(|s| s.is_entry_pgc() && s.title_number() == vts_ttn)
        .map(|i| (i as u16) + 1)?;

    let mut plan = Vec::new();
    let mut visited = vec![false; pgcs.len()];
    let mut pgcn = entry;
    loop {
        let idx = usize::from(pgcn.checked_sub(1)?);
        let (Some(pgc), Some(srp)) = (pgcs.get(idx), pgci_srp.get(idx)) else {
            break;
        };
        // Stop on loops and on PGCs of another title.
        if visited[idx] || srp.title_number() != vts_ttn {
            break;
        }
        visited[idx] = true;
        for cell in pgc.cell_walk(angle) {
            let Some(info) = usize::from(cell)
                .checked_sub(1)
                .and_then(|i| pgc.cells.get(i))
            else {
                continue;
            };
            plan.push(PlannedCell {
                pgcn,
                program: pgc.program_containing_cell(cell).unwrap_or(1),
                cell,
                first_sector: info.first_vobu_start_sector,
                last_sector: info.last_vobu_end_sector,
            });
        }
        if pgc.next_pgcn == 0 {
            break;
        }
        pgcn = pgc.next_pgcn;
    }
    Some(plan)
}

// =====================================================================
// Jump resolution — turning a transfer VmAction into a destination.
// =====================================================================

/// A cross-list transfer resolved into the concrete structure the
/// disc-level engine must load next.
///
/// [`resolve_action`] maps the transfer-class [`VmAction`]s onto
/// these; the caller then parses / looks up the named structure
/// (VTS IFO, entry PGC, menu LU) and builds a fresh [`PgcRunner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpResolution {
    /// `JumpTT ttn` resolved through TT_SRPT: parse title set `vts`
    /// (its first sector is `vts_start_sector`) and enter the entry
    /// PGC of its VTS-internal title `vts_ttn`
    /// (`Pgci::entry_pgcn_for_title`). Set SPRM 4 = `ttn` and
    /// SPRM 5 = `vts_ttn` on arrival ([`note_title_position`]).
    TitleEntry {
        ttn: u8,
        vts: u8,
        vts_ttn: u8,
        vts_start_sector: u32,
    },
    /// `JumpVTS_TT ttn` — the entry PGC of VTS-internal title `ttn`
    /// of the **current** title set (the instruction may not cross
    /// title sets, per `mpucoder-vmi-jmp.html` rows 9 / 13).
    VtsTitleEntry { vts_ttn: u8 },
    /// `JumpVTS_PTT ttn, pttn` — chapter `pttn` of VTS-internal
    /// title `ttn` of the current title set; resolve to a
    /// `(PGCN, PGN)` pair via `VtsPttSrpt::ptt` and enter at the
    /// program's entry cell (`PgcRunner::new_at_cell`).
    VtsChapter { vts_ttn: u8, pttn: u16 },
    /// `JumpSS FP` / `CallSS FP` — the First-Play PGC
    /// (`DvdDisc::parse_fp_pgc`).
    FirstPlay,
    /// `JumpSS VMGM menu` / `CallSS VMGM menu` — a VMG-domain menu
    /// by type; resolve through `VMGM_PGCI_UT`
    /// (`PgciUt::resolve_menu` with SPRM 0's language).
    VmgMenu { menu: crate::ifo::MenuType },
    /// `JumpSS VMGM pgcn` / `CallSS VMGM pgcn` — a VMG-domain PGC
    /// addressed by number.
    VmgPgc { pgcn: u16 },
    /// `JumpSS VTSM vts, ttn, menu` — a VTS-menu-domain menu of
    /// title set `vts`; resolve through that VTS's `VTSM_PGCI_UT`.
    VtsMenu {
        vts: u8,
        vts_ttn: u8,
        menu: crate::ifo::MenuType,
    },
    /// `CallSS VTSM menu` — a menu of the **current** title set
    /// (the CallSS form carries no VTS operand).
    SameVtsMenu { menu: crate::ifo::MenuType },
    /// `RSM` — pop the engine's [`ResumeContext`] stack.
    Resume,
    /// `Exit` — stop playback entirely.
    Stop,
}

/// Resolve a transfer-class [`VmAction`] into a [`JumpResolution`].
///
/// `tt_srpt` is the VMG title table (needed only for `JumpTT`).
/// Returns `None` for non-transfer actions (`Continue` / `Break` /
/// `Link` / `SetNavTimer` / `NoOpRaw`) and for a `JumpTT` whose
/// title number is absent from TT_SRPT (a malformed disc).
///
/// The 4-bit `menu` operand of the `JumpSS` / `CallSS` menu forms is
/// decoded through [`crate::ifo::MenuType::from_nibble`] — the menu
/// selector uses the same code space as the PGC-category menu-type
/// nibble (`3` = root, `4` = sub-picture, `5` = audio, `6` = angle,
/// `7` = PTT per `mpucoder-ifo_vts.html`, plus `2` = title on the
/// VMGM side per `mpucoder-ifo_vmg.html`), which is how the
/// destination "Root Menu" rows of `mpucoder-vmi-jmp.html` name it.
pub fn resolve_action(
    action: &VmAction,
    tt_srpt: Option<&crate::ifo::TtSrpt>,
) -> Option<JumpResolution> {
    use crate::ifo::MenuType;
    match action {
        VmAction::JumpTitle { ttn } => {
            let entry = tt_srpt?.title(*ttn)?;
            Some(JumpResolution::TitleEntry {
                ttn: *ttn,
                vts: entry.vts_number,
                vts_ttn: entry.vts_title_number,
                vts_start_sector: entry.vts_start_sector,
            })
        }
        VmAction::JumpVtsTitle { ttn } => Some(JumpResolution::VtsTitleEntry { vts_ttn: *ttn }),
        VmAction::JumpVtsPtt { ttn, pttn } => Some(JumpResolution::VtsChapter {
            vts_ttn: *ttn,
            pttn: *pttn,
        }),
        VmAction::JumpSs(t) => Some(match t {
            JumpSSTarget::FirstPlay => JumpResolution::FirstPlay,
            JumpSSTarget::VmgmMenu { menu } => JumpResolution::VmgMenu {
                menu: MenuType::from_nibble(*menu),
            },
            JumpSSTarget::VmgmPgcn { pgcn } => JumpResolution::VmgPgc { pgcn: *pgcn },
            JumpSSTarget::VtsmMenu { vts, ttn, menu } => JumpResolution::VtsMenu {
                vts: *vts,
                vts_ttn: *ttn,
                menu: MenuType::from_nibble(*menu),
            },
        }),
        VmAction::CallSs(t) => Some(match t {
            CallSSTarget::FirstPlay { .. } => JumpResolution::FirstPlay,
            CallSSTarget::VmgmMenu { menu, .. } => JumpResolution::VmgMenu {
                menu: MenuType::from_nibble(*menu),
            },
            CallSSTarget::VmgmPgcn { pgcn, .. } => JumpResolution::VmgPgc { pgcn: *pgcn },
            CallSSTarget::VtsmMenu { menu, .. } => JumpResolution::SameVtsMenu {
                menu: MenuType::from_nibble(*menu),
            },
        }),
        VmAction::Resume(_) => Some(JumpResolution::Resume),
        VmAction::Exit => Some(JumpResolution::Stop),
        VmAction::Continue
        | VmAction::Break
        | VmAction::Link(_)
        | VmAction::SetNavTimer { .. }
        | VmAction::NoOpRaw(_) => None,
    }
}

/// Record an arrival in the title domain on the SPRM file — the
/// position registers a disc's conditional navigation branches on:
/// SPRM 4 (`TTN`, volume-wide title), SPRM 5 (`VTS_TTN`), SPRM 6
/// (`TT_PGCN`), and SPRM 7 (`PTTN`, when the arrival was a chapter
/// jump). Allocation per `mpucoder-sprm.html`.
pub fn note_title_position(vm: &mut Vm, ttn: u8, vts_ttn: u8, pgcn: u16, pttn: Option<u16>) {
    vm.regs.set_sprm(crate::vm::SPRM_TITLE, u16::from(ttn));
    vm.regs
        .set_sprm(crate::vm::SPRM_VTS_TITLE, u16::from(vts_ttn));
    vm.regs.set_sprm(crate::vm::SPRM_PGCN, pgcn);
    if let Some(pttn) = pttn {
        vm.regs.set_sprm(crate::vm::SPRM_PTT, pttn);
    }
}

/// Everything the disc-level engine must remember at a `CallSS` to
/// honour a later `RSM`.
///
/// The [`Vm`]'s own RSM stack carries only what the instruction word
/// encodes (the optional `rsm_cell` override + highlight button);
/// *where* playback was — domain, title set, PGC, cell — is engine
/// state, mirrored here. Push one of these when a
/// [`PlaybackEvent::Transfer`] carries a `CallSs`; pop it when the
/// VM surfaces [`VmAction::Resume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeContext {
    /// Domain the CallSS left (per `mpucoder-vmi-jmp.html` rows
    /// 10..12 this is always the VTS Title domain on a conforming
    /// disc).
    pub domain: Domain,
    /// 1-based title set the call left (`0` outside any VTS).
    pub vts: u8,
    /// 1-based PGCN of the interrupted PGC.
    pub pgcn: u16,
    /// 1-based cell that was being presented when the call fired.
    pub cell: u8,
}

impl ResumeContext {
    /// The cell to re-enter on resume: the `CallSS` word's non-zero
    /// `rsm_cell` overrides the interrupted cell, per
    /// `mpucoder-vmi-sum.html` ("CallSS commands may optionally
    /// specify a cell different from the current to be entered on
    /// RSM") — the zero value means "the cell that was active".
    pub fn effective_cell(&self, rp: &crate::vm::ResumePoint) -> u8 {
        if rp.resume_cell != 0 {
            rp.resume_cell
        } else {
            self.cell
        }
    }
}

// =====================================================================
// Still-time playback — the freeze-frame hold after a cell / PGC.
// =====================================================================

/// Where a still hold currently stands — see [`StillClock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StillPhase {
    /// No still was authored (`StillTime::None`) — playback proceeds
    /// immediately.
    NotStill,
    /// A finite still is holding the last frame; `remaining_ms`
    /// milliseconds are left on the authored duration.
    Timed { remaining_ms: u64 },
    /// A `255`-authored still (`StillTime::Infinite`) — holds until
    /// something releases it (user "Still off" when permitted, or a
    /// menu button activation that transfers control).
    Infinite,
    /// The hold ended — timer expired or it was released.
    Released,
}

/// Playback-clock model of one DVD still: the freeze-frame hold a
/// cell's still-time byte or the PGC header's still-time byte
/// (offset `0x00A2`, `255` = infinite, per `mpucoder-pgc.html`)
/// imposes after the video runs out.
///
/// The engine builds one `StillClock` per still-carrying
/// [`PlaybackEvent`] (see [`PlaybackEvent::still_time`]), feeds it
/// wall-clock progress via [`advance_ms`](Self::advance_ms), and
/// forwards the user's "still off" keypress through
/// [`try_user_release`](Self::try_user_release) — which honours the
/// UOP 18 *Still off* prohibition bit exactly as
/// `mpucoder-uops.html` specifies it ("a set bit in any mask
/// inhibits the associated control", across the ORed
/// title / PGC / VOBU mask levels the caller merges with
/// [`crate::uops::UopMask::merge_or`]).
///
/// A *menu* still (still menu = infinite still + highlight buttons)
/// ends through button activation instead: the activated command
/// transfers control, so the engine drops the clock. That path is
/// [`release`](Self::release) — unconditional, for control transfers
/// that are not the UOP-18-gated user operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StillClock {
    phase: StillPhase,
}

impl StillClock {
    /// Start the hold an authored [`StillTime`] calls for.
    pub fn start(still: crate::ifo::StillTime) -> Self {
        let phase = match still {
            crate::ifo::StillTime::None => StillPhase::NotStill,
            crate::ifo::StillTime::Seconds(s) => StillPhase::Timed {
                remaining_ms: u64::from(s) * 1000,
            },
            crate::ifo::StillTime::Infinite => StillPhase::Infinite,
        };
        Self { phase }
    }

    /// Current phase.
    pub fn phase(&self) -> StillPhase {
        self.phase
    }

    /// `true` while the last frame must stay frozen (timed hold with
    /// time left, or an infinite hold).
    pub fn is_holding(&self) -> bool {
        matches!(self.phase, StillPhase::Timed { .. } | StillPhase::Infinite)
    }

    /// Advance the hold by `elapsed_ms` of wall-clock time. Returns
    /// `true` when this call ended a timed hold (the caller resumes
    /// playback exactly once). Infinite holds never expire through
    /// the clock; `NotStill` / `Released` are no-ops.
    pub fn advance_ms(&mut self, elapsed_ms: u64) -> bool {
        if let StillPhase::Timed { remaining_ms } = self.phase {
            let left = remaining_ms.saturating_sub(elapsed_ms);
            if left == 0 {
                self.phase = StillPhase::Released;
                return true;
            }
            self.phase = StillPhase::Timed { remaining_ms: left };
        }
        false
    }

    /// The user pressed "still off". Permitted only while the merged
    /// UOP mask leaves [`crate::uops::UserOp::StillOff`] (bit 18)
    /// clear — a set bit at any of the three mask levels inhibits
    /// the control. Returns `true` when the hold was released.
    pub fn try_user_release(&mut self, uops: crate::uops::UopMask) -> bool {
        if !self.is_holding() || !uops.is_allowed(crate::uops::UserOp::StillOff) {
            return false;
        }
        self.phase = StillPhase::Released;
        true
    }

    /// Unconditional release — for still-menu button activations and
    /// other control transfers that end the hold without going
    /// through the UOP-gated user operation.
    pub fn release(&mut self) {
        if self.is_holding() {
            self.phase = StillPhase::Released;
        }
    }
}

impl PlaybackEvent {
    /// The still hold this event asks the player to honour, if any:
    /// the after-cell still for [`PlaybackEvent::PlayCell`] and the
    /// PGC-level still for [`PlaybackEvent::PgcStill`]. `None` for
    /// every event that carries no freeze-frame semantics.
    pub fn still_time(&self) -> Option<crate::ifo::StillTime> {
        match self {
            PlaybackEvent::PlayCell { still, .. } | PlaybackEvent::PgcStill { still } => {
                Some(*still)
            }
            _ => None,
        }
    }

    /// Convenience: a [`StillClock`] pre-armed for this event's
    /// still, or `None` when the event carries none (equivalent to
    /// `self.still_time().map(StillClock::start)`).
    pub fn still_clock(&self) -> Option<StillClock> {
        self.still_time().map(StillClock::start)
    }
}

// =====================================================================
// Stream selection — logical → physical audio / sub-picture routing.
// =====================================================================

/// A resolved audio-stream choice — see [`select_audio_stream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSelection {
    /// Play logical stream `logical` (the SPRM 1 value), demuxing the
    /// physical MPEG-audio stream / private_stream_1 substream number
    /// `physical` (the PGC_AST_CTL mapping).
    Selected {
        logical: u8,
        physical: u8,
        /// `false` when SPRM 1 named this stream directly; `true`
        /// when it was chosen by the preference fallback (SPRM 16/17
        /// language matching or first-available).
        via_preference: bool,
    },
    /// No audio: SPRM 1 carries the `15` sentinel, or the PGC offers
    /// no stream this player can play.
    NoAudio,
}

/// `true` when the player's SPRM 15 capability bitmap can play a
/// stream with these attributes.
///
/// The SPRM 15 page allocates one plain bit and one karaoke bit per
/// optional codec (`bit10` SDDS / `bit11` DTS / `bit12` MPEG /
/// `bit14` Dolby, karaoke variants at `bit2..bit7`); a stream whose
/// VTS attributes declare [`crate::ifo::AudioApplicationMode::Karaoke`]
/// is checked against the karaoke bit, anything else against the
/// plain bit. LPCM has no plain capability bit on the page (it is
/// the one format every player must decode), so non-karaoke LPCM is
/// always playable; karaoke LPCM checks `bit7` (PCM karaoke). A
/// reserved coding mode is never playable.
pub fn audio_stream_playable(
    caps: crate::vm::AudioCapabilities,
    attrs: &crate::ifo::AudioAttributes,
) -> bool {
    use crate::ifo::{AudioApplicationMode, AudioCodingMode};
    let karaoke = matches!(attrs.application_mode, AudioApplicationMode::Karaoke);
    match attrs.coding_mode {
        AudioCodingMode::Ac3 => {
            if karaoke {
                caps.dolby_karaoke
            } else {
                caps.dolby
            }
        }
        AudioCodingMode::Mpeg1 | AudioCodingMode::Mpeg2Ext => {
            if karaoke {
                caps.mpeg_karaoke
            } else {
                caps.mpeg
            }
        }
        AudioCodingMode::Dts => {
            if karaoke {
                caps.dts_karaoke
            } else {
                caps.dts
            }
        }
        AudioCodingMode::Lpcm => !karaoke || caps.pcm_karaoke,
        AudioCodingMode::Reserved(_) => false,
    }
}

/// Pick the audio stream to play: the logical → physical routing
/// decision that combines SPRM 1 (`ASTN`), the PGC's `PGC_AST_CTL`
/// availability table, the VTS audio attributes, and the player
/// preference SPRMs.
///
/// Selection policy (each rule from the documented field semantics;
/// the *ordering* is this engine's policy):
///
/// 1. SPRM 1 `15` sentinel ⇒ [`AudioSelection::NoAudio`].
/// 2. SPRM 1 `0..=7` naming an available slot whose attributes the
///    player can play (SPRM 15, [`audio_stream_playable`]) ⇒ that
///    stream, `via_preference: false`.
/// 3. Otherwise scan logical streams `0..=7` that are available and
///    playable: first preference goes to a stream whose ISO-639
///    attribute code matches SPRM 16 *and* whose code extension
///    matches a non-zero SPRM 17; then a plain SPRM 16 language
///    match; then the lowest available stream.
///
/// `attrs` is indexed by logical stream number (the VTS_MAT audio
/// attribute list order); a missing entry is treated as playable so
/// structurally sparse IFOs still resolve.
pub fn select_audio_stream(
    vm: &Vm,
    ast_ctl: &[crate::ifo::AudioStreamControl; 8],
    attrs: &[crate::ifo::AudioAttributes],
) -> AudioSelection {
    let caps = vm.regs.audio_capabilities();
    let usable = |n: usize| -> bool {
        // (map_or, not is_none_or: MSRV 1.80.)
        ast_ctl[n].available
            && attrs
                .get(n)
                .map_or(true, |a| audio_stream_playable(caps, a))
    };
    match vm.regs.audio_stream() {
        crate::vm::AudioStreamSelector::None => return AudioSelection::NoAudio,
        crate::vm::AudioStreamSelector::Stream(n) => {
            if usable(usize::from(n)) {
                return AudioSelection::Selected {
                    logical: n,
                    physical: ast_ctl[usize::from(n)].stream_number,
                    via_preference: false,
                };
            }
        }
        crate::vm::AudioStreamSelector::Invalid(_) => {}
    }
    // Preference fallback over the available + playable set.
    let pref_lang = vm.regs.preferred_audio_language();
    let pref_ext = vm.regs.sprm(crate::vm::SPRM_PREF_AUDIO_LANG_EXT) as u8;
    let lang_matches = |n: usize| -> bool {
        pref_lang
            .ascii_bytes()
            .zip(attrs.get(n))
            .is_some_and(|(code, a)| a.language_code == code)
    };
    let ext_matches = |n: usize| -> bool {
        pref_ext != 0 && attrs.get(n).is_some_and(|a| a.code_extension == pref_ext)
    };
    let candidates: Vec<usize> = (0..8).filter(|&n| usable(n)).collect();
    let pick = candidates
        .iter()
        .copied()
        .find(|&n| lang_matches(n) && ext_matches(n))
        .or_else(|| candidates.iter().copied().find(|&n| lang_matches(n)))
        .or_else(|| candidates.first().copied());
    match pick {
        Some(n) => AudioSelection::Selected {
            logical: n as u8,
            physical: ast_ctl[n].stream_number,
            via_preference: true,
        },
        None => AudioSelection::NoAudio,
    }
}

/// Write an [`AudioSelection`] back into SPRM 1 so later `SetSystem`
/// reads and resume snapshots observe the stream the engine actually
/// picked (`15` for [`AudioSelection::NoAudio`], per the SPRM table's
/// sentinel).
pub fn note_audio_selection(vm: &mut Vm, sel: AudioSelection) {
    let value = match sel {
        AudioSelection::Selected { logical, .. } => u16::from(logical),
        AudioSelection::NoAudio => 15,
    };
    vm.regs.set_sprm(crate::vm::SPRM_AUDIO_STREAM, value);
}

/// Map the player's SPRM 14 video preference / current mode onto the
/// [`crate::ifo::SubpictureDisplay`] column a `PGC_SPST_CTL` entry is
/// resolved with.
///
/// The pan&scan / letterbox current-mode codes name their columns
/// directly. `Normal` picks between the `4:3` and `wide` columns by
/// the preferred-aspect bits: `16:9` ⇒ wide, everything else
/// (`4:3` / not-specified / reserved) ⇒ the 4:3 column, which is the
/// only column a 4:3 title authors.
pub fn subpicture_display_mode(pref: crate::vm::VideoPreference) -> crate::ifo::SubpictureDisplay {
    use crate::ifo::SubpictureDisplay;
    use crate::vm::{AspectRatio, DisplayMode};
    match pref.mode {
        DisplayMode::PanScan => SubpictureDisplay::PanScan,
        DisplayMode::Letterbox => SubpictureDisplay::Letterbox,
        DisplayMode::Normal | DisplayMode::Reserved => match pref.aspect {
            AspectRatio::Ar16x9 => SubpictureDisplay::Wide,
            _ => SubpictureDisplay::Ratio4x3,
        },
    }
}

/// A resolved sub-picture choice — see [`select_subpicture_stream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubpictureSelection {
    /// Route logical stream `logical` to physical sub-stream
    /// `physical` (`0x20 | physical` on the wire).
    Selected {
        logical: u8,
        physical: u8,
        /// SPRM 2 bit 6 — `false` means decode but do not display
        /// (except forced-display SPUs, which ignore the flag).
        display: bool,
        /// `true` when SPRM 2 carried the `63` "forced" sentinel —
        /// only forced-display sub-picture units may be shown.
        forced_only: bool,
    },
    /// No sub-picture stream is selected / available.
    None,
}

/// Pick the sub-picture stream: combines SPRM 2 (`SPSTN` — stream
/// bits, display bit 6, `62` none / `63` forced sentinels), the
/// PGC's `PGC_SPST_CTL` table resolved through the SPRM 14 display
/// mode ([`subpicture_display_mode`]), and — for the fallback — the
/// SPRM 18 preferred sub-picture language over the VTS sub-picture
/// attributes.
///
/// Policy mirror of [`select_audio_stream`]: an explicit SPRM 2
/// stream that is available wins; the `62` sentinel selects nothing;
/// the `63` sentinel and any dangling explicit stream fall back to
/// the language preference (then the lowest available stream, but
/// only when the disc *forces* a pick — for `63` — since an
/// unspecified preference must not spontaneously enable subtitles).
pub fn select_subpicture_stream(
    vm: &Vm,
    spst_ctl: &[crate::ifo::SubpictureStreamControl; 32],
    attrs: &[crate::ifo::SubpictureAttributes],
) -> SubpictureSelection {
    let view = vm.regs.subpicture_stream();
    let mode = subpicture_display_mode(vm.regs.video_preference());
    if view.is_none_sentinel() {
        return SubpictureSelection::None;
    }
    let forced_only = view.is_forced_sentinel();
    if !forced_only {
        let n = usize::from(view.stream);
        if let Some(physical) = spst_ctl.get(n).and_then(|c| c.resolve(mode)) {
            return SubpictureSelection::Selected {
                logical: view.stream,
                physical,
                display: view.display,
                forced_only: false,
            };
        }
    }
    // Fallback: SPRM 18 language preference over the available set.
    let pref = vm.regs.preferred_subpicture_language();
    let available: Vec<usize> = (0..32).filter(|&n| spst_ctl[n].available).collect();
    let lang_pick = available.iter().copied().find(|&n| {
        pref.ascii_bytes()
            .zip(attrs.get(n))
            .is_some_and(|(code, a)| a.language_code == code)
    });
    // Without a language hit, only the forced sentinel may force the
    // lowest available stream into service.
    let pick = lang_pick.or_else(|| {
        if forced_only {
            available.first().copied()
        } else {
            None
        }
    });
    match pick.and_then(|n| spst_ctl[n].resolve(mode).map(|p| (n, p))) {
        Some((n, physical)) => SubpictureSelection::Selected {
            logical: n as u8,
            physical,
            display: view.display,
            forced_only,
        },
        None => SubpictureSelection::None,
    }
}

// =====================================================================
// Karaoke downmix routing — SPRM 11 × the VTS multichannel extension.
// =====================================================================

/// What one karaoke source channel carries, unified across the
/// per-channel flag names of the `McExtensionEntry` (channel 2
/// carries guide melodies 1/2; channels 3/4 carry guide melody A/B
/// plus sound effects A/B).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KaraokeChannelContent {
    /// Guide vocal 1 present on this channel.
    pub guide_vocal_1: bool,
    /// Guide vocal 2 present on this channel.
    pub guide_vocal_2: bool,
    /// Primary guide melody (melody 1 on channel 2, melody A on
    /// channel 3, melody B on channel 4).
    pub guide_melody_primary: bool,
    /// Secondary guide melody (melody 2 — channel 2 only).
    pub guide_melody_secondary: bool,
    /// Sound effect (effect A on channel 3, effect B on channel 4).
    pub sound_effect: bool,
}

/// One row of the karaoke downmix plan: whether SPRM 11 currently
/// mixes source channel [`channel`](Self::channel) into the front /
/// rear destinations, and what content the title says lives there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KaraokeChannelRoute {
    /// Source audio channel (`2..=4` — the only channels SPRM 11
    /// allocates mix bits for).
    pub channel: u8,
    /// SPRM 11 bit `channel` — mix into the front destination.
    pub to_front: bool,
    /// SPRM 11 bit `channel + 8` — mix into the rear destination.
    pub to_rear: bool,
    /// Content flags from the stream's multichannel-extension entry.
    pub content: KaraokeChannelContent,
}

/// Combine the SPRM 11 karaoke mixing mode with a stream's
/// multichannel-extension entry into the three-channel routing plan a
/// karaoke mixer executes: for each source channel 2 / 3 / 4, the
/// current user mix destinations plus the authored content labels
/// (guide vocals / melodies / effects). Channels 0 / 1 are the base
/// stereo pair and are always presented; their optional guide-melody
/// flags remain readable on the raw [`crate::ifo::McExtensionEntry`].
pub fn karaoke_routing(
    mix: crate::vm::AudioMixMode,
    mc: &crate::ifo::McExtensionEntry,
) -> [KaraokeChannelRoute; 3] {
    [
        KaraokeChannelRoute {
            channel: 2,
            to_front: mix.mix_2_to_front,
            to_rear: mix.mix_2_to_rear,
            content: KaraokeChannelContent {
                guide_vocal_1: mc.ach2_guide_vocal_1,
                guide_vocal_2: mc.ach2_guide_vocal_2,
                guide_melody_primary: mc.ach2_guide_melody_1,
                guide_melody_secondary: mc.ach2_guide_melody_2,
                sound_effect: false,
            },
        },
        KaraokeChannelRoute {
            channel: 3,
            to_front: mix.mix_3_to_front,
            to_rear: mix.mix_3_to_rear,
            content: KaraokeChannelContent {
                guide_vocal_1: mc.ach3_guide_vocal_1,
                guide_vocal_2: mc.ach3_guide_vocal_2,
                guide_melody_primary: mc.ach3_guide_melody_a,
                guide_melody_secondary: false,
                sound_effect: mc.ach3_sound_effect_a,
            },
        },
        KaraokeChannelRoute {
            channel: 4,
            to_front: mix.mix_4_to_front,
            to_rear: mix.mix_4_to_rear,
            content: KaraokeChannelContent {
                guide_vocal_1: mc.ach4_guide_vocal_1,
                guide_vocal_2: mc.ach4_guide_vocal_2,
                guide_melody_primary: mc.ach4_guide_melody_b,
                guide_melody_secondary: false,
                sound_effect: mc.ach4_sound_effect_b,
            },
        },
    ]
}

// =====================================================================
// Trick play — VOBU-level scan stepping over the DSI search tables.
// =====================================================================

/// Scan direction for VOBU-level trick play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDirection {
    /// Fast-forward — walks the VOBU_SRI forward spans / `sri_nvwv`.
    Forward,
    /// Rewind — walks the backward spans / `sri_pvwv`.
    Backward,
}

/// The outcome of one trick-play step — see [`scan_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrickStep {
    /// Seek to the nav pack at absolute logical block `lbn` and
    /// decode its first reference frame (see
    /// [`reference_frame_span`]). `finer_steps_available` mirrors the
    /// span pointer's bit 30 — one or more VOBUs lie between the
    /// current position and the target, so a slower scan speed has
    /// intermediate frames to show.
    Jump {
        lbn: u32,
        finer_steps_available: bool,
    },
    /// The `sri_nvwv` / `sri_pvwv` bracket reports no further VOBU
    /// *containing video* in this direction (`0xBFFF_FFFF`) — the
    /// scan ran out of pictures before the cell ended.
    NoMoreVideo,
    /// No VOBU within this cell serves the requested span (the
    /// `0x3FFF_FFFF` sentinel / an unauthored table) — the engine
    /// must move to the neighbouring cell via the PGC cell walk and
    /// re-enter the scan there.
    CellBoundary,
}

/// `true` when trick play may run at all from the current position:
/// the cell's C_PBI *restricted* flag ("stops trick play",
/// `mpucoder-pgc.html` cell-playback byte 1) must be clear and the
/// merged UOP mask must allow the direction's scan operation (UOP 8
/// *Forward scan* / UOP 9 *Backward scan*, one prohibition bit at
/// any of the ORed mask levels inhibits it).
pub fn scan_permitted(
    direction: ScanDirection,
    uops: crate::uops::UopMask,
    cell_restricted: bool,
) -> bool {
    if cell_restricted {
        return false;
    }
    let op = match direction {
        ScanDirection::Forward => crate::uops::UserOp::ForwardScan,
        ScanDirection::Backward => crate::uops::UserOp::BackwardScan,
    };
    uops.is_allowed(op)
}

/// Resolve one trick-play step from the current VOBU's DSI.
///
/// `seconds_per_step` is the nominal scrub distance the playback
/// cadence wants per displayed step — e.g. an 8× scan showing one
/// frame every 250 ms asks for 2-second steps. Steps of 0.5 s or
/// less use the `sri_nvwv` / `sri_pvwv` "next/previous VOBU with
/// video" bracket pointers (the finest video-to-video stride the DSI
/// authors); anything coarser resolves through the 19-bucket span
/// tables with the non-overshooting policy of
/// [`crate::vob::VobuSri::seek_forward`] /
/// [`seek_backward`](crate::vob::VobuSri::seek_backward), falling
/// back to the bracket pointer when no span bucket is authored.
///
/// The returned [`TrickStep::Jump`] carries the **absolute** LBN
/// (current `nv_pck_lbn` ± the pointer's relative sector offset).
pub fn scan_step(
    dsi: &crate::vob::DsiPacket,
    direction: ScanDirection,
    seconds_per_step: f32,
) -> TrickStep {
    use crate::vob::SriPointer;
    let sri = &dsi.vobu_sri;
    let bracket = match direction {
        ScanDirection::Forward => sri.next_video(),
        ScanDirection::Backward => sri.prev_video(),
    };
    let pointer = if seconds_per_step > 0.5 {
        match direction {
            ScanDirection::Forward => sri.seek_forward(seconds_per_step),
            ScanDirection::Backward => sri.seek_backward(seconds_per_step),
        }
        .unwrap_or(bracket)
    } else {
        bracket
    };
    match pointer.offset_sectors() {
        Some(offset) => {
            let lbn = dsi.general_info.nv_pck_lbn;
            let target = match direction {
                ScanDirection::Forward => lbn.saturating_add(offset),
                ScanDirection::Backward => lbn.saturating_sub(offset),
            };
            TrickStep::Jump {
                lbn: target,
                finer_steps_available: pointer.has_intermediate_vobus(),
            }
        }
        None if pointer.raw == SriPointer::NO_VIDEO_VOBU => TrickStep::NoMoreVideo,
        None => TrickStep::CellBoundary,
    }
}

/// The absolute sector span a fast-play pass reads from a VOBU to
/// decode its first `count` reference frames, per the DSI_GI
/// `vobu_1stref_ea` / `vobu_2ndref_ea` / `vobu_3rdref_ea` fields
/// ("reference frame end block, relative — used for fast playing").
///
/// Returns the inclusive `(first_sector, last_sector)` absolute LBN
/// range starting at the nav pack: fast play reads exactly these
/// sectors, decodes the `count` reference pictures they end with,
/// and jumps on via [`scan_step`]. `None` when `count` is outside
/// `1..=3` or the requested end-address field is unauthored (zero —
/// a VOBU with fewer reference frames than asked for).
pub fn reference_frame_span(dsi: &crate::vob::DsiPacket, count: u8) -> Option<(u32, u32)> {
    let gi = &dsi.general_info;
    let ea = match count {
        1 => gi.vobu_1stref_ea,
        2 => gi.vobu_2ndref_ea,
        3 => gi.vobu_3rdref_ea,
        _ => return None,
    };
    if ea == 0 {
        return None;
    }
    Some((gi.nv_pck_lbn, gi.nv_pck_lbn.saturating_add(ea)))
}

/// What [`PgcRunner::handle_list_action`] tells the state loop.
#[derive(Debug)]
enum ListVerdict {
    /// The list completed — proceed to the next phase (cells after
    /// pre, next-PGCN chain after post).
    EnterCells,
    /// Return this event to the caller.
    Emit(PlaybackEvent),
    /// `self.state` was already updated — loop.
    Moved,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ifo::NavCommand;
    use crate::vm::{LinkAction, ResumePoint};

    fn jump_ss(t: JumpSSTarget) -> VmAction {
        VmAction::JumpSs(t)
    }

    fn call_ss(t: CallSSTarget) -> VmAction {
        VmAction::CallSs(t)
    }

    // ---- target_domain --------------------------------------------

    #[test]
    fn target_domain_of_transfers() {
        assert_eq!(
            target_domain(&VmAction::JumpTitle { ttn: 3 }),
            Some(Domain::VtsTitle)
        );
        assert_eq!(
            target_domain(&VmAction::JumpVtsTitle { ttn: 1 }),
            Some(Domain::VtsTitle)
        );
        assert_eq!(
            target_domain(&VmAction::JumpVtsPtt { ttn: 1, pttn: 4 }),
            Some(Domain::VtsTitle)
        );
        assert_eq!(
            target_domain(&jump_ss(JumpSSTarget::FirstPlay)),
            Some(Domain::FirstPlay)
        );
        assert_eq!(
            target_domain(&jump_ss(JumpSSTarget::VmgmMenu { menu: 2 })),
            Some(Domain::VideoManager)
        );
        assert_eq!(
            target_domain(&jump_ss(JumpSSTarget::VmgmPgcn { pgcn: 7 })),
            Some(Domain::VideoManager)
        );
        assert_eq!(
            target_domain(&jump_ss(JumpSSTarget::VtsmMenu {
                vts: 1,
                ttn: 1,
                menu: 3
            })),
            Some(Domain::VtsMenu)
        );
        assert_eq!(
            target_domain(&call_ss(CallSSTarget::FirstPlay { rsm_cell: 0 })),
            Some(Domain::FirstPlay)
        );
        assert_eq!(
            target_domain(&call_ss(CallSSTarget::VtsmMenu {
                menu: 3,
                rsm_cell: 0
            })),
            Some(Domain::VtsMenu)
        );
        assert_eq!(
            target_domain(&call_ss(CallSSTarget::VmgmPgcn {
                pgcn: 9,
                rsm_cell: 0
            })),
            Some(Domain::VideoManager)
        );
    }

    #[test]
    fn target_domain_none_for_non_transfers() {
        assert_eq!(target_domain(&VmAction::Continue), None);
        assert_eq!(target_domain(&VmAction::Break), None);
        assert_eq!(target_domain(&VmAction::Exit), None);
        assert_eq!(
            target_domain(&VmAction::Link(LinkAction::Pgcn { pgcn: 2 })),
            None
        );
        assert_eq!(
            target_domain(&VmAction::Resume(ResumePoint {
                resume_cell: 0,
                hl_btn: 0
            })),
            None
        );
    }

    // ---- transition_permitted: First Play (rows 1..3) --------------

    #[test]
    fn first_play_rows() {
        let d = Domain::FirstPlay;
        // Row 1 — VMG title menu or any PGC in VMG.
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VmgmMenu { menu: 2 }),
            0
        ));
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VmgmPgcn { pgcn: 3 }),
            0
        ));
        // Row 2 — VTS menu domain root menu.
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VtsmMenu {
                vts: 2,
                ttn: 1,
                menu: 3
            }),
            0
        ));
        // Row 3 — VTS title domain.
        assert!(transition_permitted(d, &VmAction::JumpTitle { ttn: 1 }, 0));
        // Not in the table: self-jump, CallSS, VTS-internal jumps, RSM.
        assert!(!transition_permitted(
            d,
            &jump_ss(JumpSSTarget::FirstPlay),
            0
        ));
        assert!(!transition_permitted(
            d,
            &call_ss(CallSSTarget::FirstPlay { rsm_cell: 0 }),
            0
        ));
        assert!(!transition_permitted(
            d,
            &VmAction::JumpVtsTitle { ttn: 1 },
            0
        ));
        assert!(!transition_permitted(
            d,
            &VmAction::Resume(ResumePoint {
                resume_cell: 0,
                hl_btn: 0
            }),
            0
        ));
    }

    // ---- transition_permitted: Video Manager (rows 4..6) -----------

    #[test]
    fn vmg_rows() {
        let d = Domain::VideoManager;
        // Row 4 — First Play.
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::FirstPlay),
            0
        ));
        // Row 5 — VTS menu root.
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VtsmMenu {
                vts: 1,
                ttn: 1,
                menu: 3
            }),
            0
        ));
        // Row 6 — VTS title + resume.
        assert!(transition_permitted(d, &VmAction::JumpTitle { ttn: 2 }, 0));
        assert!(transition_permitted(
            d,
            &VmAction::Resume(ResumePoint {
                resume_cell: 0,
                hl_btn: 0
            }),
            0
        ));
        // Intra-VMG JumpSS is not in the table (Link territory).
        assert!(!transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VmgmMenu { menu: 2 }),
            0
        ));
        assert!(!transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VmgmPgcn { pgcn: 5 }),
            0
        ));
        // CallSS is a title-domain exit only.
        assert!(!transition_permitted(
            d,
            &call_ss(CallSSTarget::VtsmMenu {
                menu: 3,
                rsm_cell: 0
            }),
            0
        ));
        // JumpVTS_* need a current VTS.
        assert!(!transition_permitted(
            d,
            &VmAction::JumpVtsTitle { ttn: 1 },
            0
        ));
    }

    // ---- transition_permitted: VTS Menu (rows 7..9) ------------------

    #[test]
    fn vts_menu_rows() {
        let d = Domain::VtsMenu;
        // Row 7 — First Play.
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::FirstPlay),
            2
        ));
        // Row 8 — VMG menu / PGC.
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VmgmMenu { menu: 2 }),
            2
        ));
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VmgmPgcn { pgcn: 4 }),
            2
        ));
        // Row 9 — same-VTS title / PTT / resume.
        assert!(transition_permitted(
            d,
            &VmAction::JumpVtsTitle { ttn: 1 },
            2
        ));
        assert!(transition_permitted(
            d,
            &VmAction::JumpVtsPtt { ttn: 1, pttn: 3 },
            2
        ));
        assert!(transition_permitted(
            d,
            &VmAction::Resume(ResumePoint {
                resume_cell: 0,
                hl_btn: 0
            }),
            2
        ));
        // Same-VTS menu jump = intra-domain movement, passes.
        assert!(transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VtsmMenu {
                vts: 2,
                ttn: 1,
                menu: 5
            }),
            2
        ));
        // "another VTS menu domain — not allowed".
        assert!(!transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VtsmMenu {
                vts: 3,
                ttn: 1,
                menu: 3
            }),
            2
        ));
        // JumpTT is a FP/VMG instruction.
        assert!(!transition_permitted(d, &VmAction::JumpTitle { ttn: 1 }, 2));
        // CallSS is a title-domain exit only.
        assert!(!transition_permitted(
            d,
            &call_ss(CallSSTarget::FirstPlay { rsm_cell: 0 }),
            2
        ));
    }

    // ---- transition_permitted: VTS Title (rows 10..13) ---------------

    #[test]
    fn vts_title_rows() {
        let d = Domain::VtsTitle;
        // Row 10 — CallSS First Play.
        assert!(transition_permitted(
            d,
            &call_ss(CallSSTarget::FirstPlay { rsm_cell: 0 }),
            1
        ));
        // Row 11 — CallSS VMG menu / PGC.
        assert!(transition_permitted(
            d,
            &call_ss(CallSSTarget::VmgmMenu {
                menu: 2,
                rsm_cell: 1
            }),
            1
        ));
        assert!(transition_permitted(
            d,
            &call_ss(CallSSTarget::VmgmPgcn {
                pgcn: 6,
                rsm_cell: 0
            }),
            1
        ));
        // Row 12 — CallSS same-VTS root menu (CallSS VTSM carries no
        // VTS operand; it is same-VTS by construction).
        assert!(transition_permitted(
            d,
            &call_ss(CallSSTarget::VtsmMenu {
                menu: 3,
                rsm_cell: 4
            }),
            1
        ));
        // Row 13 — same-VTS title / PTT.
        assert!(transition_permitted(
            d,
            &VmAction::JumpVtsTitle { ttn: 2 },
            1
        ));
        assert!(transition_permitted(
            d,
            &VmAction::JumpVtsPtt { ttn: 2, pttn: 1 },
            1
        ));
        // No JumpSS row in the title-domain table.
        assert!(!transition_permitted(
            d,
            &jump_ss(JumpSSTarget::FirstPlay),
            1
        ));
        assert!(!transition_permitted(
            d,
            &jump_ss(JumpSSTarget::VmgmMenu { menu: 2 }),
            1
        ));
        // No direct JumpTT either — the table routes via JumpVTS_TT.
        assert!(!transition_permitted(d, &VmAction::JumpTitle { ttn: 2 }, 1));
        // RSM resumes *to* the title domain, never from it.
        assert!(!transition_permitted(
            d,
            &VmAction::Resume(ResumePoint {
                resume_cell: 0,
                hl_btn: 0
            }),
            1
        ));
    }

    // ---- non-transfer actions are always permitted -------------------

    #[test]
    fn non_transfers_always_pass() {
        for d in [
            Domain::FirstPlay,
            Domain::VideoManager,
            Domain::VtsMenu,
            Domain::VtsTitle,
        ] {
            assert!(transition_permitted(d, &VmAction::Continue, 0));
            assert!(transition_permitted(d, &VmAction::Break, 0));
            assert!(transition_permitted(d, &VmAction::Exit, 0));
            assert!(transition_permitted(
                d,
                &VmAction::Link(LinkAction::Pgcn { pgcn: 1 }),
                0
            ));
            assert!(transition_permitted(
                d,
                &VmAction::SetNavTimer {
                    seconds: 30,
                    pgcn: 1
                },
                0
            ));
            assert!(transition_permitted(
                d,
                &VmAction::NoOpRaw(NavCommand { bytes: [0u8; 8] }),
                0
            ));
        }
    }

    #[test]
    fn domain_is_menu() {
        assert!(Domain::VideoManager.is_menu());
        assert!(Domain::VtsMenu.is_menu());
        assert!(!Domain::FirstPlay.is_menu());
        assert!(!Domain::VtsTitle.is_menu());
    }

    // ---- resolve_link ------------------------------------------------

    use crate::ifo::{CellPlaybackInfo, Pgc, PgcTime};
    use crate::nav::LinkSubset;

    fn test_cell(category: u8) -> CellPlaybackInfo {
        CellPlaybackInfo {
            category_byte0: category,
            restricted: false,
            still_time: 0,
            cell_command: 0,
            playback_time: PgcTime::from_bytes([0, 1, 0, 0xE0]),
            first_vobu_start_sector: 0,
            first_ilvu_end_sector: 0,
            last_vobu_start_sector: 0,
            last_vobu_end_sector: 0,
        }
    }

    /// 5-cell / 2-program PGC: cells 1..=2 plain, cells 3..=5 a
    /// 3-angle block. Program 1 → cell 1, program 2 → cell 3.
    /// next PGCN = 7, prev PGCN unauthored, goup PGCN = 9.
    fn link_test_pgc() -> Pgc {
        let mut pgc = Pgc::parse(&[0u8; 0xEC]).unwrap();
        pgc.number_of_programs = 2;
        pgc.number_of_cells = 5;
        pgc.program_map = vec![1, 3];
        pgc.cells = vec![
            test_cell(0x00),
            test_cell(0x00),
            test_cell(0x50), // first of angle block
            test_cell(0x90), // middle
            test_cell(0xD0), // last
        ];
        pgc.next_pgcn = 7;
        pgc.prev_pgcn = 0;
        pgc.goup_pgcn = 9;
        pgc
    }

    fn subset(subset: LinkSubset) -> LinkAction {
        LinkAction::Subset { subset, hl_bn: 0 }
    }

    #[test]
    fn resolve_link_cell_subsets() {
        let pgc = link_test_pgc();
        let pos = PgcPosition {
            pgcn: 2,
            program: 1,
            cell: 2,
        };
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::Nop), 1),
            LinkOutcome::Continue
        );
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkTopCell), 1),
            LinkOutcome::PlayCell { cell: 2 }
        );
        // Next cell enters the angle block, angle-resolved.
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkNextCell), 1),
            LinkOutcome::PlayCell { cell: 3 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkNextCell), 2),
            LinkOutcome::PlayCell { cell: 4 }
        );
        // From inside the block, next-cell runs off the PGC → post.
        let in_block = PgcPosition {
            pgcn: 2,
            program: 2,
            cell: 4,
        };
        assert_eq!(
            resolve_link(&pgc, in_block, &subset(LinkSubset::LinkNextCell), 1),
            LinkOutcome::PostCommands
        );
        // Prev cell, and the clamp at cell 1.
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkPrevCell), 1),
            LinkOutcome::PlayCell { cell: 1 }
        );
        let at_first = PgcPosition {
            pgcn: 2,
            program: 1,
            cell: 1,
        };
        assert_eq!(
            resolve_link(&pgc, at_first, &subset(LinkSubset::LinkPrevCell), 1),
            LinkOutcome::PlayCell { cell: 1 }
        );
    }

    #[test]
    fn resolve_link_program_subsets() {
        let pgc = link_test_pgc();
        let p1 = PgcPosition {
            pgcn: 2,
            program: 1,
            cell: 2,
        };
        let p2 = PgcPosition {
            pgcn: 2,
            program: 2,
            cell: 4,
        };
        // TopPG restarts the current program at its entry cell.
        assert_eq!(
            resolve_link(&pgc, p2, &subset(LinkSubset::LinkTopPG), 1),
            LinkOutcome::PlayCell { cell: 3 }
        );
        // …angle-resolved when the entry cell opens an angle block.
        assert_eq!(
            resolve_link(&pgc, p2, &subset(LinkSubset::LinkTopPG), 2),
            LinkOutcome::PlayCell { cell: 4 }
        );
        assert_eq!(
            resolve_link(&pgc, p1, &subset(LinkSubset::LinkNextPG), 1),
            LinkOutcome::PlayCell { cell: 3 }
        );
        // Next-PG past the final program → post commands.
        assert_eq!(
            resolve_link(&pgc, p2, &subset(LinkSubset::LinkNextPG), 1),
            LinkOutcome::PostCommands
        );
        assert_eq!(
            resolve_link(&pgc, p2, &subset(LinkSubset::LinkPrevPG), 1),
            LinkOutcome::PlayCell { cell: 1 }
        );
        // Prev-PG clamps at program 1.
        assert_eq!(
            resolve_link(&pgc, p1, &subset(LinkSubset::LinkPrevPG), 1),
            LinkOutcome::PlayCell { cell: 1 }
        );
    }

    #[test]
    fn resolve_link_pgc_subsets() {
        let pgc = link_test_pgc();
        let pos = PgcPosition {
            pgcn: 2,
            program: 1,
            cell: 1,
        };
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkTopPGC), 1),
            LinkOutcome::OtherPgc { pgcn: 2 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkNextPGC), 1),
            LinkOutcome::OtherPgc { pgcn: 7 }
        );
        // prev PGCN is unauthored (zero).
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkPrevPGC), 1),
            LinkOutcome::NoDestination
        );
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkGoupPGC), 1),
            LinkOutcome::OtherPgc { pgcn: 9 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::LinkTailPGC), 1),
            LinkOutcome::PostCommands
        );
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::Rsm), 1),
            LinkOutcome::Resume
        );
        assert_eq!(
            resolve_link(&pgc, pos, &subset(LinkSubset::Invalid(0x04)), 1),
            LinkOutcome::NoDestination
        );
    }

    #[test]
    fn resolve_link_numbered_forms() {
        let pgc = link_test_pgc();
        let pos = PgcPosition {
            pgcn: 2,
            program: 1,
            cell: 1,
        };
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Pgcn { pgcn: 3 }, 1),
            LinkOutcome::OtherPgc { pgcn: 3 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Pgcn { pgcn: 0 }, 1),
            LinkOutcome::NoDestination
        );
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Pttn { pttn: 5, hl_bn: 0 }, 1),
            LinkOutcome::Chapter { pttn: 5 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Pttn { pttn: 0, hl_bn: 0 }, 1),
            LinkOutcome::NoDestination
        );
        // LinkPGN lands on the program's entry cell, angle-resolved.
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Pgn { pgn: 2, hl_bn: 0 }, 3),
            LinkOutcome::PlayCell { cell: 5 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Pgn { pgn: 3, hl_bn: 0 }, 1),
            LinkOutcome::NoDestination
        );
        // LinkCN addresses a cell directly (angle-resolved when it
        // opens a block; a non-first block cell passes through).
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Cn { cn: 3, hl_bn: 0 }, 2),
            LinkOutcome::PlayCell { cell: 4 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Cn { cn: 5, hl_bn: 0 }, 1),
            LinkOutcome::PlayCell { cell: 5 }
        );
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Cn { cn: 6, hl_bn: 0 }, 1),
            LinkOutcome::NoDestination
        );
        assert_eq!(
            resolve_link(&pgc, pos, &LinkAction::Cn { cn: 0, hl_bn: 0 }, 1),
            LinkOutcome::NoDestination
        );
    }

    // ---- PgcRunner -----------------------------------------------------

    use crate::ifo::{NavCommand as NC, PgcCommandTable, StillTime};
    use crate::vm::Vm;

    fn nc(bytes: [u8; 8]) -> NC {
        NC { bytes }
    }

    fn with_commands(mut pgc: Pgc, pre: Vec<NC>, post: Vec<NC>, cell: Vec<NC>) -> Pgc {
        pgc.commands = Some(PgcCommandTable {
            pre,
            post,
            cell,
            end_address: 0,
        });
        pgc
    }

    /// Plain n-cell PGC (no angle blocks, one program, no commands).
    fn plain_pgc(cells: u8) -> Pgc {
        let mut pgc = Pgc::parse(&[0u8; 0xEC]).unwrap();
        pgc.number_of_programs = 1;
        pgc.number_of_cells = cells;
        pgc.program_map = vec![1];
        pgc.cells = (0..cells).map(|_| test_cell(0x00)).collect();
        for (i, c) in pgc.cells.iter_mut().enumerate() {
            c.first_vobu_start_sector = (i as u32 + 1) * 100;
            c.last_vobu_end_sector = (i as u32 + 1) * 100 + 99;
        }
        pgc
    }

    #[test]
    fn runner_plays_cells_then_follows_next_pgcn() {
        let mut pgc = plain_pgc(2);
        pgc.next_pgcn = 5;
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert_eq!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell {
                cell: 1,
                program: 1,
                first_sector: 100,
                last_sector: 199,
                still: StillTime::None,
            }
        );
        assert_eq!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell {
                cell: 2,
                program: 1,
                first_sector: 200,
                last_sector: 299,
                still: StillTime::None,
            }
        );
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::NextPgc { pgcn: 5 });
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::Finished);
    }

    #[test]
    fn runner_empty_pgc_finishes() {
        let pgc = Pgc::parse(&[0u8; 0xEC]).unwrap();
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::Finished);
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::Finished);
    }

    #[test]
    fn runner_pre_jump_tt_transfers() {
        // pre = [JumpTT 3] — the disc-insertion FP_PGC shape.
        let pgc = with_commands(
            plain_pgc(1),
            vec![nc([0x30, 0x02, 0, 0, 0, 3, 0, 0])],
            vec![],
            vec![],
        );
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert_eq!(
            r.next_event(&mut vm),
            PlaybackEvent::Transfer(VmAction::JumpTitle { ttn: 3 })
        );
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::Finished);
    }

    #[test]
    fn runner_pre_break_enters_cells() {
        // pre = [Break, JumpTT 9] — the jump must never run.
        let pgc = with_commands(
            plain_pgc(1),
            vec![
                nc([0x00, 0x02, 0, 0, 0, 0, 0, 0]),
                nc([0x30, 0x02, 0, 0, 0, 9, 0, 0]),
            ],
            vec![],
            vec![],
        );
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 1, .. }
        ));
    }

    #[test]
    fn runner_cell_command_links_to_other_pgc() {
        // Cell 2 carries cell command #1 = LinkPGCN 9.
        let mut pgc = plain_pgc(2);
        pgc.cells[1].cell_command = 1;
        let pgc = with_commands(
            pgc,
            vec![],
            vec![],
            vec![nc([0x20, 0x04, 0, 0, 0, 0, 0, 9])],
        );
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 1, .. }
        ));
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 2, .. }
        ));
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::NextPgc { pgcn: 9 });
    }

    #[test]
    fn runner_pgc_still_event_before_post() {
        let mut pgc = plain_pgc(1);
        pgc.still_time = 10;
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 1, .. }
        ));
        assert_eq!(
            r.next_event(&mut vm),
            PlaybackEvent::PgcStill {
                still: StillTime::Seconds(10)
            }
        );
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::Finished);
    }

    #[test]
    fn runner_nav_timer_resumes_pre_list() {
        // pre = [SetNVTMR 30s → PGC 2 (immediate form), Break].
        let pgc = with_commands(
            plain_pgc(1),
            vec![
                nc([0x52, 0, 0x00, 0x1E, 0x00, 0x02, 0, 0]),
                nc([0x00, 0x02, 0, 0, 0, 0, 0, 0]),
            ],
            vec![],
            vec![],
        );
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert_eq!(
            r.next_event(&mut vm),
            PlaybackEvent::NavTimer {
                seconds: 30,
                pgcn: 2
            }
        );
        // The list resumes after the SetNVTMR word: Break → cells.
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 1, .. }
        ));
    }

    #[test]
    fn runner_post_link_applies_highlight_button() {
        // post = [LinkNextPGC, button=4]; next PGCN = 7.
        let mut pgc = plain_pgc(1);
        pgc.next_pgcn = 7;
        let pgc = with_commands(
            pgc,
            vec![],
            vec![nc([0x20, 0x01, 0, 0, 0, 0, 0x04, 0x0A])],
            vec![],
        );
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 1, .. }
        ));
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::NextPgc { pgcn: 7 });
        // The hl_bn override landed in SPRM 8 bits 15..10.
        assert_eq!(vm.regs.highlight_button(), 4);
    }

    #[test]
    fn runner_angle_walk_follows_sprm3() {
        // link_test_pgc: cells 1..=2 plain, 3..=5 a 3-angle block,
        // next PGCN = 7. Angle 2 presents cell 4 for the block.
        let pgc = link_test_pgc();
        let mut vm = Vm::new();
        vm.regs.set_sprm(crate::vm::SPRM_ANGLE, 2);
        let mut r = PgcRunner::new(&pgc, 2);
        let cells: Vec<u8> = std::iter::from_fn(|| match r.next_event(&mut vm) {
            PlaybackEvent::PlayCell { cell, .. } => Some(cell),
            _ => None,
        })
        .collect();
        assert_eq!(cells, vec![1, 2, 4]);
    }

    #[test]
    fn runner_new_at_cell_skips_pre() {
        // pre = [JumpTT 9] must not run on a chapter entry.
        let mut pgc = plain_pgc(3);
        pgc.next_pgcn = 0;
        let pgc = with_commands(
            pgc,
            vec![nc([0x30, 0x02, 0, 0, 0, 9, 0, 0])],
            vec![],
            vec![],
        );
        let mut vm = Vm::new();
        let mut r = PgcRunner::new_at_cell(&pgc, 1, 2);
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 2, .. }
        ));
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { cell: 3, .. }
        ));
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::Finished);
        // Out-of-range entry cell falls through to post → Finished.
        let mut r2 = PgcRunner::new_at_cell(&pgc, 1, 9);
        assert_eq!(r2.next_event(&mut vm), PlaybackEvent::Finished);
    }

    // ---- menu interaction ----------------------------------------------

    use crate::vob::{ButtonInfo, HighlightInfo, SlColi};

    fn button(up: u8, down: u8, left: u8, right: u8, auto: bool, command: [u8; 8]) -> ButtonInfo {
        ButtonInfo {
            btn_coln: 1,
            start_x: 0,
            end_x: 10,
            start_y: 0,
            end_y: 10,
            auto_action: auto,
            up,
            down,
            left,
            right,
            command,
        }
    }

    /// Three-button row: 1 <-> 2 <-> 3. Button 2 auto-actions a
    /// LinkPGCN 5; button 3 actions a JumpTT 2.
    fn test_hli() -> HighlightInfo {
        HighlightInfo {
            hli_s_ptm: 0,
            hli_e_ptm: 0,
            btn_sl_e_ptm: 0,
            btn_md: 0,
            btn_sn: 1,
            btn_ns: 3,
            nsl_btn_ns: 3,
            fosl_btnn: 0,
            foac_btnn: 0,
            sl_coli: [SlColi::default(); 3],
            buttons: vec![
                button(1, 1, 1, 2, false, [0; 8]),
                button(2, 2, 1, 3, true, [0x20, 0x04, 0, 0, 0, 0, 0, 5]),
                button(3, 3, 2, 0, false, [0x30, 0x02, 0, 0, 0, 2, 0, 0]),
            ],
        }
    }

    #[test]
    fn navigate_button_adjacency() {
        let hli = test_hli();
        assert_eq!(navigate_button(&hli, 1, ButtonMove::Right), 2);
        assert_eq!(navigate_button(&hli, 2, ButtonMove::Right), 3);
        // Unauthored (zero) adjacency stays put.
        assert_eq!(navigate_button(&hli, 3, ButtonMove::Right), 3);
        assert_eq!(navigate_button(&hli, 3, ButtonMove::Left), 2);
        assert_eq!(navigate_button(&hli, 1, ButtonMove::Up), 1);
        // Out-of-range current button stays put.
        assert_eq!(navigate_button(&hli, 9, ButtonMove::Left), 9);
        assert_eq!(navigate_button(&hli, 0, ButtonMove::Down), 0);
    }

    #[test]
    fn initial_button_force_select_and_clamp() {
        let mut hli = test_hli();
        assert_eq!(initial_button(&hli, 2), 2); // keep a valid selection
        assert_eq!(initial_button(&hli, 9), 1); // clamp an invalid one
        assert_eq!(initial_button(&hli, 0), 1);
        hli.fosl_btnn = 3;
        assert_eq!(initial_button(&hli, 2), 3); // force-select wins
    }

    #[test]
    fn select_and_activate_buttons() {
        let hli = test_hli();
        let mut vm = Vm::new();
        // Plain selection updates SPRM 8 only.
        assert_eq!(select_button(&mut vm, &hli, 3), ButtonPress::Selected);
        assert_eq!(vm.regs.highlight_button(), 3);
        // Auto-action button executes its LinkPGCN immediately.
        assert_eq!(
            select_button(&mut vm, &hli, 2),
            ButtonPress::AutoAction(VmAction::Link(LinkAction::Pgcn { pgcn: 5 }))
        );
        assert_eq!(vm.regs.highlight_button(), 2);
        // Invalid button numbers.
        assert_eq!(select_button(&mut vm, &hli, 0), ButtonPress::Invalid);
        assert_eq!(select_button(&mut vm, &hli, 4), ButtonPress::Invalid);
        // Explicit activation runs the button command through the VM.
        assert_eq!(
            activate_button(&mut vm, &hli, 3),
            Some(VmAction::JumpTitle { ttn: 2 })
        );
        assert_eq!(activate_button(&mut vm, &hli, 4), None);
    }

    #[test]
    fn forced_action_button() {
        let mut hli = test_hli();
        let mut vm = Vm::new();
        assert_eq!(forced_action(&mut vm, &hli), None);
        hli.foac_btnn = 3;
        assert_eq!(
            forced_action(&mut vm, &hli),
            Some(VmAction::JumpTitle { ttn: 2 })
        );
    }

    // ---- plan_title_cells ---------------------------------------------

    use crate::ifo::PgciSrp;

    fn srp(category: u32) -> PgciSrp {
        PgciSrp {
            category,
            offset: 0,
        }
    }

    #[test]
    fn plan_title_follows_next_pgcn_chain() {
        // PGC 1: entry for title 1, 2 cells, chains to PGC 2.
        // PGC 2: continuation of title 1, 1 cell, chain ends.
        // PGC 3: entry for title 2 (must not appear in the plan).
        let mut pgc1 = plain_pgc(2);
        pgc1.next_pgcn = 2;
        let pgc2 = plain_pgc(1);
        let pgc3 = plain_pgc(1);
        let srps = [srp(0x8100_0000), srp(0x0100_0000), srp(0x8200_0000)];
        let pgcs = [pgc1, pgc2, pgc3];

        let plan = plan_title_cells(&srps, &pgcs, 1, 1).unwrap();
        let flat: Vec<(u16, u8, u32, u32)> = plan
            .iter()
            .map(|p| (p.pgcn, p.cell, p.first_sector, p.last_sector))
            .collect();
        assert_eq!(
            flat,
            vec![(1, 1, 100, 199), (1, 2, 200, 299), (2, 1, 100, 199)]
        );

        // Title 2's plan is just PGC 3.
        let plan2 = plan_title_cells(&srps, &pgcs, 2, 1).unwrap();
        assert_eq!(plan2.len(), 1);
        assert_eq!(plan2[0].pgcn, 3);

        // Unknown title -> None.
        assert!(plan_title_cells(&srps, &pgcs, 3, 1).is_none());
    }

    #[test]
    fn plan_title_breaks_navigation_loops_and_foreign_pgcs() {
        // PGC 1 chains to PGC 2; PGC 2 chains back to PGC 1 (loop).
        let mut pgc1 = plain_pgc(1);
        pgc1.next_pgcn = 2;
        let mut pgc2 = plain_pgc(1);
        pgc2.next_pgcn = 1;
        let srps = [srp(0x8100_0000), srp(0x0100_0000)];
        let plan = plan_title_cells(&srps, &[pgc1, pgc2], 1, 1).unwrap();
        assert_eq!(plan.len(), 2); // each PGC contributes once

        // A chain into another title's PGC stops the plan.
        let mut a = plain_pgc(1);
        a.next_pgcn = 2;
        let b = plain_pgc(1);
        let srps2 = [srp(0x8100_0000), srp(0x8200_0000)];
        let plan2 = plan_title_cells(&srps2, &[a, b], 1, 1).unwrap();
        assert_eq!(plan2.len(), 1);
        assert_eq!(plan2[0].pgcn, 1);
    }

    #[test]
    fn plan_title_respects_angle() {
        // link_test_pgc: cells 1..=2 plain + 3..=5 angle block; make
        // it a self-contained title (no next chain).
        let mut pgc = link_test_pgc();
        pgc.next_pgcn = 0;
        let srps = [srp(0x8100_0000)];
        let plan = plan_title_cells(&srps, &[pgc], 1, 2).unwrap();
        let cells: Vec<u8> = plan.iter().map(|p| p.cell).collect();
        assert_eq!(cells, vec![1, 2, 4]);
    }

    // ---- resolve_action / note_title_position / ResumeContext ---------

    use crate::ifo::{DvdTitleEntry, MenuType, TtSrpt};

    fn one_title_srpt() -> TtSrpt {
        TtSrpt {
            title_count: 1,
            end_address: 19,
            entries: vec![DvdTitleEntry {
                title_type: 0,
                angle_count: 1,
                chapter_count: 8,
                parental_mask: 0,
                vts_number: 2,
                vts_title_number: 1,
                vts_start_sector: 12345,
            }],
        }
    }

    #[test]
    fn resolve_action_title_jumps() {
        let srpt = one_title_srpt();
        assert_eq!(
            resolve_action(&VmAction::JumpTitle { ttn: 1 }, Some(&srpt)),
            Some(JumpResolution::TitleEntry {
                ttn: 1,
                vts: 2,
                vts_ttn: 1,
                vts_start_sector: 12345,
            })
        );
        // Missing title / missing table -> unresolvable.
        assert_eq!(
            resolve_action(&VmAction::JumpTitle { ttn: 2 }, Some(&srpt)),
            None
        );
        assert_eq!(resolve_action(&VmAction::JumpTitle { ttn: 1 }, None), None);
        // Same-VTS forms need no table.
        assert_eq!(
            resolve_action(&VmAction::JumpVtsTitle { ttn: 3 }, None),
            Some(JumpResolution::VtsTitleEntry { vts_ttn: 3 })
        );
        assert_eq!(
            resolve_action(&VmAction::JumpVtsPtt { ttn: 3, pttn: 7 }, None),
            Some(JumpResolution::VtsChapter {
                vts_ttn: 3,
                pttn: 7
            })
        );
    }

    #[test]
    fn resolve_action_menu_forms() {
        assert_eq!(
            resolve_action(&VmAction::JumpSs(JumpSSTarget::FirstPlay), None),
            Some(JumpResolution::FirstPlay)
        );
        assert_eq!(
            resolve_action(&VmAction::JumpSs(JumpSSTarget::VmgmMenu { menu: 2 }), None),
            Some(JumpResolution::VmgMenu {
                menu: MenuType::Title
            })
        );
        assert_eq!(
            resolve_action(&VmAction::JumpSs(JumpSSTarget::VmgmPgcn { pgcn: 12 }), None),
            Some(JumpResolution::VmgPgc { pgcn: 12 })
        );
        assert_eq!(
            resolve_action(
                &VmAction::JumpSs(JumpSSTarget::VtsmMenu {
                    vts: 2,
                    ttn: 1,
                    menu: 3
                }),
                None
            ),
            Some(JumpResolution::VtsMenu {
                vts: 2,
                vts_ttn: 1,
                menu: MenuType::Root
            })
        );
        // CallSS VTSM has no VTS operand -> current title set.
        assert_eq!(
            resolve_action(
                &VmAction::CallSs(CallSSTarget::VtsmMenu {
                    menu: 5,
                    rsm_cell: 3
                }),
                None
            ),
            Some(JumpResolution::SameVtsMenu {
                menu: MenuType::Audio
            })
        );
        assert_eq!(
            resolve_action(
                &VmAction::CallSs(CallSSTarget::VmgmPgcn {
                    pgcn: 4,
                    rsm_cell: 0
                }),
                None
            ),
            Some(JumpResolution::VmgPgc { pgcn: 4 })
        );
        assert_eq!(
            resolve_action(&VmAction::Exit, None),
            Some(JumpResolution::Stop)
        );
        assert_eq!(
            resolve_action(
                &VmAction::Resume(ResumePoint {
                    resume_cell: 0,
                    hl_btn: 0
                }),
                None
            ),
            Some(JumpResolution::Resume)
        );
        // Non-transfers resolve to None.
        assert_eq!(resolve_action(&VmAction::Continue, None), None);
        assert_eq!(resolve_action(&VmAction::Break, None), None);
        assert_eq!(
            resolve_action(&VmAction::Link(LinkAction::Pgcn { pgcn: 1 }), None),
            None
        );
    }

    #[test]
    fn note_title_position_sets_sprms() {
        let mut vm = Vm::new();
        note_title_position(&mut vm, 3, 1, 5, Some(7));
        assert_eq!(vm.regs.sprm(crate::vm::SPRM_TITLE), 3);
        assert_eq!(vm.regs.sprm(crate::vm::SPRM_VTS_TITLE), 1);
        assert_eq!(vm.regs.sprm(crate::vm::SPRM_PGCN), 5);
        assert_eq!(vm.regs.sprm(crate::vm::SPRM_PTT), 7);
        // Without a chapter, SPRM 7 stays put.
        note_title_position(&mut vm, 4, 2, 6, None);
        assert_eq!(vm.regs.sprm(crate::vm::SPRM_TITLE), 4);
        assert_eq!(vm.regs.sprm(crate::vm::SPRM_PTT), 7);
    }

    #[test]
    fn resume_context_effective_cell() {
        let ctx = ResumeContext {
            domain: Domain::VtsTitle,
            vts: 2,
            pgcn: 3,
            cell: 6,
        };
        // rsm_cell 0 = "the cell that was active".
        assert_eq!(
            ctx.effective_cell(&ResumePoint {
                resume_cell: 0,
                hl_btn: 0
            }),
            6
        );
        // Non-zero rsm_cell overrides.
        assert_eq!(
            ctx.effective_cell(&ResumePoint {
                resume_cell: 9,
                hl_btn: 0
            }),
            9
        );
    }

    #[test]
    fn link_highlight_button_extraction() {
        assert_eq!(
            link_highlight_button(&LinkAction::Subset {
                subset: LinkSubset::LinkTopCell,
                hl_bn: 5
            }),
            Some(5)
        );
        assert_eq!(
            link_highlight_button(&LinkAction::Subset {
                subset: LinkSubset::LinkTopCell,
                hl_bn: 0
            }),
            None
        );
        assert_eq!(
            link_highlight_button(&LinkAction::Pttn { pttn: 1, hl_bn: 9 }),
            Some(9)
        );
        assert_eq!(link_highlight_button(&LinkAction::Pgcn { pgcn: 1 }), None);
        assert_eq!(
            link_highlight_button(&LinkAction::Cn { cn: 1, hl_bn: 2 }),
            Some(2)
        );
    }

    // ---- StillClock -----------------------------------------------

    use crate::uops::{UopMask, UserOp};

    #[test]
    fn still_clock_not_still_passes_through() {
        let mut c = StillClock::start(StillTime::None);
        assert_eq!(c.phase(), StillPhase::NotStill);
        assert!(!c.is_holding());
        assert!(!c.advance_ms(10_000));
        assert!(!c.try_user_release(UopMask::NONE));
        assert_eq!(c.phase(), StillPhase::NotStill);
    }

    #[test]
    fn still_clock_timed_expires_once() {
        let mut c = StillClock::start(StillTime::Seconds(2));
        assert_eq!(c.phase(), StillPhase::Timed { remaining_ms: 2000 });
        assert!(c.is_holding());
        assert!(!c.advance_ms(1500));
        assert_eq!(c.phase(), StillPhase::Timed { remaining_ms: 500 });
        // The call that crosses zero reports the release exactly once.
        assert!(c.advance_ms(600));
        assert_eq!(c.phase(), StillPhase::Released);
        assert!(!c.is_holding());
        assert!(!c.advance_ms(1000));
    }

    #[test]
    fn still_clock_infinite_never_expires_by_time() {
        let mut c = StillClock::start(StillTime::Infinite);
        assert_eq!(c.phase(), StillPhase::Infinite);
        assert!(!c.advance_ms(u64::MAX));
        assert!(c.is_holding());
        // …but the user can release it when UOP 18 is clear.
        assert!(c.try_user_release(UopMask::NONE));
        assert_eq!(c.phase(), StillPhase::Released);
    }

    #[test]
    fn still_clock_user_release_gated_on_uop_18() {
        // A set "Still off" bit at any merged level inhibits release.
        let mut c = StillClock::start(StillTime::Infinite);
        let vobu_mask = UopMask::NONE.with(UserOp::StillOff);
        let merged = UopMask::merge_or(UopMask::NONE, UopMask::NONE, vobu_mask);
        assert!(!c.try_user_release(merged));
        assert!(c.is_holding());
        // A mask that prohibits *other* ops but leaves bit 18 clear
        // does not inhibit the release.
        let other = UopMask::NONE.with(UserOp::PauseOn).with(UserOp::Stop);
        assert!(c.try_user_release(other));
        assert!(!c.is_holding());
        // Released is terminal: further attempts are no-ops.
        assert!(!c.try_user_release(UopMask::NONE));
    }

    #[test]
    fn still_clock_unconditional_release_for_menu_transfer() {
        // Still-menu path: a button activation transfers control, so
        // the engine releases the hold without consulting UOP 18.
        let mut c = StillClock::start(StillTime::Infinite);
        c.release();
        assert_eq!(c.phase(), StillPhase::Released);
        // NotStill stays NotStill (release only ends real holds).
        let mut n = StillClock::start(StillTime::None);
        n.release();
        assert_eq!(n.phase(), StillPhase::NotStill);
    }

    #[test]
    fn playback_event_still_accessors() {
        let cell_ev = PlaybackEvent::PlayCell {
            cell: 1,
            program: 1,
            first_sector: 0,
            last_sector: 9,
            still: StillTime::Seconds(5),
        };
        assert_eq!(cell_ev.still_time(), Some(StillTime::Seconds(5)));
        assert_eq!(
            cell_ev.still_clock().unwrap().phase(),
            StillPhase::Timed { remaining_ms: 5000 }
        );
        let pgc_ev = PlaybackEvent::PgcStill {
            still: StillTime::Infinite,
        };
        assert_eq!(pgc_ev.still_time(), Some(StillTime::Infinite));
        assert!(pgc_ev.still_clock().unwrap().is_holding());
        assert_eq!(PlaybackEvent::Finished.still_time(), None);
        assert!(PlaybackEvent::NextPgc { pgcn: 2 }.still_clock().is_none());
    }

    #[test]
    fn runner_still_event_drives_still_clock() {
        // End-to-end: a PGC whose header authors a 3-second still —
        // the runner's PgcStill event arms a clock that holds for
        // exactly 3000 ms.
        let mut pgc = plain_pgc(1);
        pgc.still_time = 3;
        let mut vm = Vm::new();
        let mut r = PgcRunner::new(&pgc, 1);
        assert!(matches!(
            r.next_event(&mut vm),
            PlaybackEvent::PlayCell { .. }
        ));
        let ev = r.next_event(&mut vm);
        assert_eq!(ev.still_time(), Some(StillTime::Seconds(3)));
        let mut clock = ev.still_clock().unwrap();
        assert!(!clock.advance_ms(2999));
        assert!(clock.advance_ms(1));
        assert!(!clock.is_holding());
        assert_eq!(r.next_event(&mut vm), PlaybackEvent::Finished);
    }

    // ---- Stream selection ------------------------------------------

    use crate::ifo::{
        AudioAttributes, AudioStreamControl, McExtensionEntry, SubpictureAttributes,
        SubpictureDisplay, SubpictureStreamControl,
    };
    use crate::vm::{
        SPRM_AUDIO_CAPS, SPRM_AUDIO_STREAM, SPRM_PREF_AUDIO_LANG, SPRM_PREF_AUDIO_LANG_EXT,
        SPRM_PREF_SUBP_LANG, SPRM_SUBPICTURE_STREAM, SPRM_VIDEO_PREF,
    };

    /// Build an 8-byte audio-attribute field: coding mode, application
    /// mode, ISO-639 language, code extension.
    fn audio_attrs(coding: u8, app: u8, lang: [u8; 2], ext: u8) -> AudioAttributes {
        let lang_type = if lang == [0, 0] { 0u8 } else { 1 };
        AudioAttributes::parse(&[
            (coding << 5) | (lang_type << 2) | app,
            0b0000_0001, // 48 kHz, stereo
            lang[0],
            lang[1],
            0,
            ext,
            0,
            0,
        ])
    }

    fn ast_ctl_with(entries: &[(usize, u8)]) -> [AudioStreamControl; 8] {
        let mut ctl = [AudioStreamControl {
            available: false,
            stream_number: 0,
        }; 8];
        for &(i, phys) in entries {
            ctl[i] = AudioStreamControl {
                available: true,
                stream_number: phys,
            };
        }
        ctl
    }

    /// All-capabilities player (every SPRM 15 bit that matters set).
    fn all_caps_vm() -> Vm {
        let mut vm = Vm::new();
        vm.regs.set_sprm(SPRM_AUDIO_CAPS, 0b0100_1100_1101_1100);
        vm
    }

    #[test]
    fn audio_playable_per_codec_and_karaoke_bit() {
        let caps = |raw: u16| crate::vm::AudioCapabilities {
            sdds_karaoke: (raw >> 2) & 1 == 1,
            dts_karaoke: (raw >> 3) & 1 == 1,
            mpeg_karaoke: (raw >> 4) & 1 == 1,
            dolby_karaoke: (raw >> 6) & 1 == 1,
            pcm_karaoke: (raw >> 7) & 1 == 1,
            sdds: (raw >> 10) & 1 == 1,
            dts: (raw >> 11) & 1 == 1,
            mpeg: (raw >> 12) & 1 == 1,
            dolby: (raw >> 14) & 1 == 1,
            raw,
        };
        // AC-3 normal ↔ bit 14; AC-3 karaoke ↔ bit 6.
        let ac3 = audio_attrs(0, 0, *b"en", 0);
        let ac3_kar = audio_attrs(0, 1, *b"en", 0);
        assert!(audio_stream_playable(caps(1 << 14), &ac3));
        assert!(!audio_stream_playable(caps(1 << 6), &ac3));
        assert!(audio_stream_playable(caps(1 << 6), &ac3_kar));
        assert!(!audio_stream_playable(caps(1 << 14), &ac3_kar));
        // DTS ↔ bit 11 / bit 3; MPEG ↔ bit 12 / bit 4.
        assert!(audio_stream_playable(
            caps(1 << 11),
            &audio_attrs(6, 0, *b"en", 0)
        ));
        assert!(audio_stream_playable(
            caps(1 << 3),
            &audio_attrs(6, 1, *b"en", 0)
        ));
        assert!(audio_stream_playable(
            caps(1 << 12),
            &audio_attrs(2, 0, *b"en", 0)
        ));
        assert!(audio_stream_playable(
            caps(1 << 4),
            &audio_attrs(3, 1, *b"en", 0)
        ));
        // LPCM: always playable unless karaoke without bit 7.
        assert!(audio_stream_playable(
            caps(0),
            &audio_attrs(4, 0, *b"en", 0)
        ));
        assert!(!audio_stream_playable(
            caps(0),
            &audio_attrs(4, 1, *b"en", 0)
        ));
        assert!(audio_stream_playable(
            caps(1 << 7),
            &audio_attrs(4, 1, *b"en", 0)
        ));
        // Reserved coding mode: never playable.
        assert!(!audio_stream_playable(
            caps(u16::MAX),
            &audio_attrs(1, 0, *b"en", 0)
        ));
    }

    #[test]
    fn select_audio_prefers_explicit_sprm1() {
        let mut vm = all_caps_vm();
        vm.regs.set_sprm(SPRM_AUDIO_STREAM, 2);
        let ctl = ast_ctl_with(&[(0, 0), (2, 5)]);
        let attrs = vec![audio_attrs(0, 0, *b"en", 0); 3];
        assert_eq!(
            select_audio_stream(&vm, &ctl, &attrs),
            AudioSelection::Selected {
                logical: 2,
                physical: 5,
                via_preference: false,
            }
        );
    }

    #[test]
    fn select_audio_none_sentinel_short_circuits() {
        let mut vm = all_caps_vm();
        vm.regs.set_sprm(SPRM_AUDIO_STREAM, 15);
        let ctl = ast_ctl_with(&[(0, 0)]);
        assert_eq!(
            select_audio_stream(&vm, &ctl, &[audio_attrs(0, 0, *b"en", 0)]),
            AudioSelection::NoAudio
        );
    }

    #[test]
    fn select_audio_falls_back_to_language_preference() {
        // SPRM 1 names stream 5, which the PGC does not carry;
        // streams 0 (en) and 1 (ja) are available; SPRM 16 = "ja".
        let mut vm = all_caps_vm();
        vm.regs.set_sprm(SPRM_AUDIO_STREAM, 5);
        vm.regs
            .set_sprm(SPRM_PREF_AUDIO_LANG, u16::from_be_bytes(*b"ja"));
        let ctl = ast_ctl_with(&[(0, 0), (1, 1)]);
        let attrs = vec![audio_attrs(0, 0, *b"en", 0), audio_attrs(0, 0, *b"ja", 0)];
        assert_eq!(
            select_audio_stream(&vm, &ctl, &attrs),
            AudioSelection::Selected {
                logical: 1,
                physical: 1,
                via_preference: true,
            }
        );
    }

    #[test]
    fn select_audio_language_extension_tiebreak() {
        // Two "en" streams; SPRM 17 = 3 (director comments) matches
        // stream 1's code extension.
        let mut vm = all_caps_vm();
        vm.regs.set_sprm(SPRM_AUDIO_STREAM, 7); // dangling
        vm.regs
            .set_sprm(SPRM_PREF_AUDIO_LANG, u16::from_be_bytes(*b"en"));
        vm.regs.set_sprm(SPRM_PREF_AUDIO_LANG_EXT, 3);
        let ctl = ast_ctl_with(&[(0, 0), (1, 1)]);
        let attrs = vec![audio_attrs(0, 0, *b"en", 1), audio_attrs(0, 0, *b"en", 3)];
        assert_eq!(
            select_audio_stream(&vm, &ctl, &attrs),
            AudioSelection::Selected {
                logical: 1,
                physical: 1,
                via_preference: true,
            }
        );
    }

    #[test]
    fn select_audio_skips_unplayable_codec() {
        // Stream 0 is DTS but the player has no DTS bit; stream 1 is
        // AC-3 and playable — the fallback lands on 1 even though 0
        // is lower.
        let mut vm = Vm::new();
        vm.regs.set_sprm(SPRM_AUDIO_CAPS, 1 << 14); // Dolby only
        vm.regs.set_sprm(SPRM_AUDIO_STREAM, 0);
        let ctl = ast_ctl_with(&[(0, 0), (1, 1)]);
        let attrs = vec![audio_attrs(6, 0, *b"en", 0), audio_attrs(0, 0, *b"en", 0)];
        assert_eq!(
            select_audio_stream(&vm, &ctl, &attrs),
            AudioSelection::Selected {
                logical: 1,
                physical: 1,
                via_preference: true,
            }
        );
        // No playable stream at all → NoAudio.
        let dts_only = vec![audio_attrs(6, 0, *b"en", 0), audio_attrs(6, 0, *b"en", 0)];
        assert_eq!(
            select_audio_stream(&vm, &ctl, &dts_only),
            AudioSelection::NoAudio
        );
    }

    #[test]
    fn note_audio_selection_writes_sprm1() {
        let mut vm = Vm::new();
        note_audio_selection(
            &mut vm,
            AudioSelection::Selected {
                logical: 3,
                physical: 7,
                via_preference: true,
            },
        );
        assert_eq!(vm.regs.sprm(SPRM_AUDIO_STREAM), 3);
        note_audio_selection(&mut vm, AudioSelection::NoAudio);
        assert_eq!(vm.regs.sprm(SPRM_AUDIO_STREAM), 15);
    }

    #[test]
    fn subpicture_display_mode_mapping() {
        let pref = |aspect: u16, mode: u16| {
            let mut vm = Vm::new();
            vm.regs
                .set_sprm(SPRM_VIDEO_PREF, (aspect << 10) | (mode << 8));
            vm.regs.video_preference()
        };
        // Pan&scan / letterbox modes name their columns directly.
        assert_eq!(
            subpicture_display_mode(pref(3, 1)),
            SubpictureDisplay::PanScan
        );
        assert_eq!(
            subpicture_display_mode(pref(0, 2)),
            SubpictureDisplay::Letterbox
        );
        // Normal: 16:9 ⇒ wide, 4:3 / not-specified ⇒ 4:3 column.
        assert_eq!(subpicture_display_mode(pref(3, 0)), SubpictureDisplay::Wide);
        assert_eq!(
            subpicture_display_mode(pref(0, 0)),
            SubpictureDisplay::Ratio4x3
        );
        assert_eq!(
            subpicture_display_mode(pref(1, 0)),
            SubpictureDisplay::Ratio4x3
        );
    }

    fn spst_ctl_with(entries: &[(usize, [u8; 4])]) -> [SubpictureStreamControl; 32] {
        let mut ctl = [SubpictureStreamControl {
            available: false,
            stream_4x3: 0,
            stream_wide: 0,
            stream_letterbox: 0,
            stream_pan_scan: 0,
        }; 32];
        for &(i, [s43, sw, slb, sps]) in entries {
            ctl[i] = SubpictureStreamControl {
                available: true,
                stream_4x3: s43,
                stream_wide: sw,
                stream_letterbox: slb,
                stream_pan_scan: sps,
            };
        }
        ctl
    }

    fn subp_attrs(lang: [u8; 2]) -> SubpictureAttributes {
        SubpictureAttributes::parse(&[0b0000_0001, 0, lang[0], lang[1], 0, 0])
    }

    #[test]
    fn select_subpicture_explicit_stream_resolves_display_column() {
        let mut vm = Vm::new();
        // Stream 1, display on; 16:9 wide output.
        vm.regs.set_sprm(SPRM_SUBPICTURE_STREAM, (1 << 6) | 1);
        vm.regs.set_sprm(SPRM_VIDEO_PREF, 3 << 10);
        let ctl = spst_ctl_with(&[(1, [4, 5, 6, 7])]);
        assert_eq!(
            select_subpicture_stream(&vm, &ctl, &[]),
            SubpictureSelection::Selected {
                logical: 1,
                physical: 5, // the wide column
                display: true,
                forced_only: false,
            }
        );
        // Same slot on a letterboxed output routes column 6.
        vm.regs.set_sprm(SPRM_VIDEO_PREF, 2 << 8);
        assert_eq!(
            select_subpicture_stream(&vm, &ctl, &[]),
            SubpictureSelection::Selected {
                logical: 1,
                physical: 6,
                display: true,
                forced_only: false,
            }
        );
    }

    #[test]
    fn select_subpicture_none_sentinel() {
        let mut vm = Vm::new();
        vm.regs.set_sprm(SPRM_SUBPICTURE_STREAM, 62);
        let ctl = spst_ctl_with(&[(0, [0, 0, 0, 0])]);
        assert_eq!(
            select_subpicture_stream(&vm, &ctl, &[]),
            SubpictureSelection::None
        );
    }

    #[test]
    fn select_subpicture_forced_sentinel_falls_back() {
        // SPRM 2 = 63 (forced): language preference picks stream 1
        // ("ja"); without a match the lowest available serves.
        let mut vm = Vm::new();
        vm.regs.set_sprm(SPRM_SUBPICTURE_STREAM, 63);
        vm.regs
            .set_sprm(SPRM_PREF_SUBP_LANG, u16::from_be_bytes(*b"ja"));
        let ctl = spst_ctl_with(&[(0, [2, 2, 2, 2]), (1, [9, 9, 9, 9])]);
        let attrs = vec![subp_attrs(*b"en"), subp_attrs(*b"ja")];
        assert_eq!(
            select_subpicture_stream(&vm, &ctl, &attrs),
            SubpictureSelection::Selected {
                logical: 1,
                physical: 9,
                display: false,
                forced_only: true,
            }
        );
        // No language hit → lowest available, still forced-only.
        vm.regs.set_sprm(SPRM_PREF_SUBP_LANG, 0xFFFF);
        assert_eq!(
            select_subpicture_stream(&vm, &ctl, &attrs),
            SubpictureSelection::Selected {
                logical: 0,
                physical: 2,
                display: false,
                forced_only: true,
            }
        );
    }

    #[test]
    fn select_subpicture_dangling_stream_needs_language_match() {
        // SPRM 2 names stream 9 (not authored). With no language
        // preference match the engine must NOT spontaneously enable a
        // subtitle stream.
        let mut vm = Vm::new();
        vm.regs.set_sprm(SPRM_SUBPICTURE_STREAM, (1 << 6) | 9);
        let ctl = spst_ctl_with(&[(0, [2, 2, 2, 2])]);
        let attrs = vec![subp_attrs(*b"en")];
        assert_eq!(
            select_subpicture_stream(&vm, &ctl, &attrs),
            SubpictureSelection::None
        );
        // A language match rescues it.
        vm.regs
            .set_sprm(SPRM_PREF_SUBP_LANG, u16::from_be_bytes(*b"en"));
        assert_eq!(
            select_subpicture_stream(&vm, &ctl, &attrs),
            SubpictureSelection::Selected {
                logical: 0,
                physical: 2,
                display: true,
                forced_only: false,
            }
        );
    }

    // ---- Karaoke routing -------------------------------------------

    #[test]
    fn karaoke_routing_combines_amxmd_and_mc_entry() {
        let mut vm = Vm::new();
        // Mix ch2 → front, ch3 → rear, ch4 → both.
        vm.regs.set_sprm(
            crate::vm::SPRM_AMXMD,
            (1 << 2) | (1 << 11) | (1 << 4) | (1 << 12),
        );
        // MC entry: ch2 carries guide vocal 1 + melody 2; ch3 carries
        // sound effect A; ch4 carries guide melody B.
        let mc = McExtensionEntry::parse(&[0, 0, 0b0000_1001, 0b0000_0001, 0b0000_0010, 0, 0, 0]);
        let routes = karaoke_routing(vm.regs.audio_mix_mode(), &mc);
        assert_eq!(routes[0].channel, 2);
        assert!(routes[0].to_front && !routes[0].to_rear);
        assert!(routes[0].content.guide_vocal_1);
        assert!(routes[0].content.guide_melody_secondary);
        assert!(!routes[0].content.sound_effect);
        assert_eq!(routes[1].channel, 3);
        assert!(!routes[1].to_front && routes[1].to_rear);
        assert!(routes[1].content.sound_effect);
        assert!(!routes[1].content.guide_vocal_1);
        assert_eq!(routes[2].channel, 4);
        assert!(routes[2].to_front && routes[2].to_rear);
        assert!(routes[2].content.guide_melody_primary);
        assert!(!routes[2].content.guide_melody_secondary);
    }

    // ---- Trick play -------------------------------------------------

    use crate::vob::{DsiPacket, SriPointer, VobuSri};

    /// Build a DSI body (0x222 bytes, all zero) with the given
    /// nav-pack LBN, reference-frame end addresses, and VOBU_SRI
    /// entry patches (`(sri_relative_offset, raw_word)`).
    fn dsi_with(lbn: u32, ref_eas: [u32; 3], sri: &[(usize, u32)]) -> DsiPacket {
        let mut buf = vec![0u8; DsiPacket::BODY_SIZE];
        buf[0x04..0x08].copy_from_slice(&lbn.to_be_bytes());
        buf[0x0C..0x10].copy_from_slice(&ref_eas[0].to_be_bytes());
        buf[0x10..0x14].copy_from_slice(&ref_eas[1].to_be_bytes());
        buf[0x14..0x18].copy_from_slice(&ref_eas[2].to_be_bytes());
        for &(off, word) in sri {
            let at = VobuSri::PACKET_OFFSET + off;
            buf[at..at + 4].copy_from_slice(&word.to_be_bytes());
        }
        DsiPacket::parse(&buf).expect("synthetic DSI parses")
    }

    #[test]
    fn scan_permitted_gates_on_restriction_and_uops() {
        use crate::uops::{UopMask, UserOp};
        // Restricted cell stops trick play in both directions.
        assert!(!scan_permitted(ScanDirection::Forward, UopMask::NONE, true));
        // UOP 8 blocks forward only; UOP 9 backward only.
        let fwd_banned = UopMask::NONE.with(UserOp::ForwardScan);
        assert!(!scan_permitted(ScanDirection::Forward, fwd_banned, false));
        assert!(scan_permitted(ScanDirection::Backward, fwd_banned, false));
        let bwd_banned = UopMask::NONE.with(UserOp::BackwardScan);
        assert!(scan_permitted(ScanDirection::Forward, bwd_banned, false));
        assert!(!scan_permitted(ScanDirection::Backward, bwd_banned, false));
        assert!(scan_permitted(ScanDirection::Forward, UopMask::NONE, false));
    }

    #[test]
    fn scan_step_fine_stride_uses_video_brackets() {
        // sri_nvwv (SRI +0x00) says next video VOBU is +25 sectors;
        // sri_pvwv (SRI +0xA4) says previous is -7.
        let dsi = dsi_with(
            1000,
            [0; 3],
            &[
                (0x00, VobuSri::VALID_BIT | 25),
                (0xA4, VobuSri::VALID_BIT | 7),
            ],
        );
        assert_eq!(
            scan_step(&dsi, ScanDirection::Forward, 0.5),
            TrickStep::Jump {
                lbn: 1025,
                finer_steps_available: false,
            }
        );
        assert_eq!(
            scan_step(&dsi, ScanDirection::Backward, 0.5),
            TrickStep::Jump {
                lbn: 993,
                finer_steps_available: false,
            }
        );
    }

    #[test]
    fn scan_step_coarse_stride_resolves_span_buckets() {
        // Forward 10 s bucket (table index 3, SRI +0x04 + 3*4) with
        // the intermediate bit set; backward 10 s bucket lives at
        // on-disc entry 18-3=15 (SRI +0x58 + 15*4).
        let dsi = dsi_with(
            5000,
            [0; 3],
            &[
                (
                    0x04 + 3 * 4,
                    VobuSri::VALID_BIT | VobuSri::INTERMEDIATE_BIT | 300,
                ),
                (0x58 + 15 * 4, VobuSri::VALID_BIT | 280),
            ],
        );
        assert_eq!(
            scan_step(&dsi, ScanDirection::Forward, 10.0),
            TrickStep::Jump {
                lbn: 5300,
                finer_steps_available: true,
            }
        );
        assert_eq!(
            scan_step(&dsi, ScanDirection::Backward, 10.0),
            TrickStep::Jump {
                lbn: 4720,
                finer_steps_available: false,
            }
        );
    }

    #[test]
    fn scan_step_falls_back_to_bracket_when_no_span_authored() {
        // No span buckets valid, but sri_nvwv points +2.
        let dsi = dsi_with(100, [0; 3], &[(0x00, VobuSri::VALID_BIT | 2)]);
        assert_eq!(
            scan_step(&dsi, ScanDirection::Forward, 30.0),
            TrickStep::Jump {
                lbn: 102,
                finer_steps_available: false,
            }
        );
    }

    #[test]
    fn scan_step_no_more_video_and_cell_boundary() {
        // sri_nvwv carries the 0xBFFF_FFFF "no following VOBU
        // contains video" sentinel.
        let no_video = dsi_with(100, [0; 3], &[(0x00, SriPointer::NO_VIDEO_VOBU)]);
        assert_eq!(
            scan_step(&no_video, ScanDirection::Forward, 0.5),
            TrickStep::NoMoreVideo
        );
        // An all-zero SRI (nothing authored) reports the cell edge.
        let empty = dsi_with(100, [0; 3], &[]);
        assert_eq!(
            scan_step(&empty, ScanDirection::Forward, 10.0),
            TrickStep::CellBoundary
        );
        assert_eq!(
            scan_step(&empty, ScanDirection::Backward, 0.5),
            TrickStep::CellBoundary
        );
        // The NO_VOBU span sentinel also lands on the cell edge.
        let span_sentinel = dsi_with(100, [0; 3], &[(0x04, SriPointer::NO_VOBU)]);
        assert_eq!(
            scan_step(&span_sentinel, ScanDirection::Forward, 120.0),
            TrickStep::CellBoundary
        );
    }

    #[test]
    fn scan_step_backward_saturates_at_zero() {
        // Backward jump larger than the current LBN clamps to 0
        // rather than wrapping.
        let dsi = dsi_with(5, [0; 3], &[(0xA4, VobuSri::VALID_BIT | 50)]);
        assert_eq!(
            scan_step(&dsi, ScanDirection::Backward, 0.5),
            TrickStep::Jump {
                lbn: 0,
                finer_steps_available: false,
            }
        );
    }

    #[test]
    fn reference_frame_span_reads_dsi_gi_end_addresses() {
        let dsi = dsi_with(2000, [4, 9, 15], &[]);
        assert_eq!(reference_frame_span(&dsi, 1), Some((2000, 2004)));
        assert_eq!(reference_frame_span(&dsi, 2), Some((2000, 2009)));
        assert_eq!(reference_frame_span(&dsi, 3), Some((2000, 2015)));
        assert_eq!(reference_frame_span(&dsi, 0), None);
        assert_eq!(reference_frame_span(&dsi, 4), None);
        // Unauthored (zero) end address → None.
        let sparse = dsi_with(2000, [4, 0, 0], &[]);
        assert_eq!(reference_frame_span(&sparse, 1), Some((2000, 2004)));
        assert_eq!(reference_frame_span(&sparse, 2), None);
    }
}
