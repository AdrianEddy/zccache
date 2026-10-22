//! Conservative initialized-guard dataflow over the compiler's control-flow graph.
use rustc_hir::def_id::LocalDefId;
use rustc_lint::LateContext;
use rustc_middle::{mir, ty};

pub(super) fn check(cx: &LateContext<'_>, owner: LocalDefId) {
    use rustc_mir_dataflow::{move_paths::MoveData, Analysis};
    let (stored, _) = cx.tcx.mir_promoted(owner);
    if stored.is_stolen() {
        let later = cx.tcx.optimized_mir(owner);
        if later
            .local_decls
            .iter()
            .any(|local| contains_guard(cx, local.ty, &mut Vec::new()))
        {
            super::emit(
                cx,
                later.span,
                "cannot validate guard lifetime after compiler MIR was consumed",
            );
        }
        return;
    }
    let mut owned = (*stored.borrow()).clone();
    // Borrow checking has imaginary edges that bypass real moves/drops.
    // Analyze execution paths, preserving the original MIR for the compiler.
    for block in owned.basic_blocks.as_mut() {
        let Some(terminator) = &mut block.terminator else {
            continue;
        };
        if let mir::TerminatorKind::FalseEdge { real_target, .. }
        | mir::TerminatorKind::FalseUnwind { real_target, .. } = terminator.kind
        {
            terminator.kind = mir::TerminatorKind::Goto {
                target: real_target,
            };
        }
    }
    let body = &owned;
    let data = MoveData::gather_moves(body, cx.tcx, |_| true);
    let mut leaves = Vec::new();
    for (local, declaration) in body.local_decls.iter_enumerated() {
        guard_leaves(cx, declaration.ty, mir::Place::from(local), &mut leaves);
    }
    let mut guards = Vec::new();
    for leaf in leaves {
        let index = match data.rev_lookup.find(leaf.as_ref()) {
            rustc_mir_dataflow::move_paths::LookupResult::Exact(index)
            | rustc_mir_dataflow::move_paths::LookupResult::Parent(Some(index)) => index,
            rustc_mir_dataflow::move_paths::LookupResult::Parent(None) => continue,
        };
        if !guards.contains(&index) {
            guards.push(index);
        }
    }
    if guards.is_empty() {
        return;
    }
    let mut cursor = super::guard_initialization::GuardInitialization::new(cx.tcx, body, &data)
        .iterate_to_fixpoint(cx.tcx, body, None)
        .into_results_cursor(body);
    for (block, value) in body.basic_blocks.iter_enumerated() {
        let terminator = value.terminator();
        let mir::TerminatorKind::Call { func, .. } = &terminator.kind else {
            continue;
        };
        let callee = func.ty(&body.local_decls, cx.tcx);
        let ty::FnDef(definition, _) = callee.kind() else {
            continue;
        };
        let path = cx.get_def_path(*definition);
        let slow = cx.tcx.def_kind(*definition) == rustc_hir::def::DefKind::Fn
            && matches!(
                cx.tcx.crate_name(definition.krate).as_str(),
                "std" | "tokio"
            )
            && path.iter().any(|part| part.as_str() == "fs")
            && path.last().is_some_and(|part| {
                matches!(
                    part.as_str(),
                    "read_dir" | "read" | "write" | "copy" | "remove_dir_all"
                )
            });
        if !slow {
            continue;
        }
        cursor.seek_before_primary_effect(mir::Location {
            block,
            statement_index: value.statements.len(),
        });
        if guards.iter().any(|guard| cursor.get().contains(*guard)) {
            super::emit(
                cx,
                terminator.source_info.span,
                "slow filesystem work retains publication or store exclusion",
            );
        }
    }
}

fn contains_guard<'tcx>(
    cx: &LateContext<'tcx>,
    value: ty::Ty<'tcx>,
    seen: &mut Vec<ty::Ty<'tcx>>,
) -> bool {
    if matches!(value.kind(), ty::FnDef(..) | ty::FnPtr(..)) {
        return false;
    }
    if seen.contains(&value) {
        return false;
    }
    seen.push(value);
    value
        .walk()
        .filter_map(|argument| argument.as_type())
        .any(|nested| {
            let ty::Adt(adt, arguments) = nested.kind() else {
                return false;
            };
            let path = cx.get_def_path(adt.did());
            let name = path.last().map(|name| name.as_str());
            if super::canonical_path(cx, adt.did())
                == "zccache_daemon_core::daemon::server::staging_root::StagingRoot"
            {
                return false;
            }
            let native = match cx.tcx.crate_name(adt.did().krate).as_str() {
                "zccache_core" => name.is_some_and(|name| {
                    matches!(
                        name,
                        "PublicationCommitPermit"
                            | "MutexGuard"
                            | "RwLockWriteGuard"
                            | "RwLockReadGuard"
                    )
                }),
                "zccache_artifact" => {
                    name == Some("StagedCommitGuard")
                        && cx.tcx.opt_parent(adt.did()).is_some_and(|parent| {
                            cx.tcx.item_name(parent).as_str() == "staged_lock"
                        })
                }
                "std" | "kernal_api" | "tokio" | "lock_api" => name.is_some_and(|name| {
                    matches!(
                        name,
                        "FileLock"
                            | "OwnedFileLock"
                            | "MutexGuard"
                            | "RwLockWriteGuard"
                            | "RwLockReadGuard"
                    )
                }),
                _ => false,
            };
            native
                || (adt.did().is_local()
                    && adt.all_fields().any(|field| {
                        contains_guard(cx, field.ty(cx.tcx, arguments).skip_norm_wip(), seen)
                    }))
        })
}

// Query guard leaves rather than initialized wrapper roots. Moving one field
// must not erase its sibling, and a moved-out Result must not retain a phantom
// guard. Untracked leaves conservatively use the compiler's nearest parent.
fn guard_leaves<'tcx>(
    cx: &LateContext<'tcx>,
    value: ty::Ty<'tcx>,
    place: mir::Place<'tcx>,
    leaves: &mut Vec<mir::Place<'tcx>>,
) {
    if !contains_guard(cx, value, &mut Vec::new()) {
        return;
    }
    if let ty::Tuple(fields) = value.kind() {
        for (index, field) in fields.iter().enumerate() {
            guard_leaves(
                cx,
                field,
                place.project_deeper(
                    &[mir::ProjectionElem::Field(
                        rustc_abi::FieldIdx::from_usize(index),
                        field,
                    )],
                    cx.tcx,
                ),
                leaves,
            );
        }
        return;
    }
    if let ty::Adt(adt, arguments) = value.kind() {
        let name = cx.tcx.item_name(adt.did());
        let transparent = adt.did().is_local()
            || (cx.tcx.crate_name(adt.did().krate).as_str() == "core"
                && matches!(name.as_str(), "Option" | "Result"));
        if transparent {
            for (variant, definition) in adt.variants().iter_enumerated() {
                let base = if adt.is_enum() {
                    place.project_deeper(&[mir::ProjectionElem::Downcast(None, variant)], cx.tcx)
                } else {
                    place
                };
                for (index, field) in definition.fields.iter_enumerated() {
                    let field = field.ty(cx.tcx, arguments).skip_norm_wip();
                    guard_leaves(
                        cx,
                        field,
                        base.project_deeper(&[mir::ProjectionElem::Field(index, field)], cx.tcx),
                        leaves,
                    );
                }
            }
            return;
        }
    }
    leaves.push(place);
}
