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
use crate::vm::{LinkAction, VmAction};

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
}
