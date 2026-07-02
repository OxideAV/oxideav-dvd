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
use crate::vm::VmAction;

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
}
