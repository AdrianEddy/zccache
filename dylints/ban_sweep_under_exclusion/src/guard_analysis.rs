//! Conservative initialized-guard dataflow over the compiler's control-flow graph.
use super::guard_types::Scope;
use rustc_hir::def_id::LocalDefId;
use rustc_lint::LateContext;
use rustc_middle::{mir, ty};

pub(super) fn check(cx: &LateContext<'_>, owner: LocalDefId) {
    use rustc_mir_dataflow::{move_paths::MoveData, Analysis};
    let (stored, _) = cx.tcx.mir_promoted(owner);
    if stored.is_stolen() {
        let later = cx.tcx.optimized_mir(owner);
        // With consumed MIR, conservatively reject relevant effects anywhere
        // in the body rather than guessing where an optimized guard dropped.
        let effects =
            later
                .basic_blocks
                .iter()
                .fold(0, |effects, block| match &block.terminator().kind {
                    mir::TerminatorKind::Call { func, .. } => {
                        effects
                            | super::effects::call(
                                cx,
                                func.ty(&later.local_decls, cx.tcx),
                                &mut Vec::new(),
                            )
                    }
                    mir::TerminatorKind::Drop { place, .. } => {
                        effects
                            | super::effects::drop_value(
                                cx,
                                place.ty(&later.local_decls, cx.tcx).ty,
                                &mut Vec::new(),
                                &mut Vec::new(),
                            )
                    }
                    _ => effects,
                });
        let scope = if effects & (super::effects::SWEEP | super::effects::UNKNOWN) != 0 {
            Scope::Any
        } else {
            Scope::Critical
        };
        if effects != 0
            && later
                .local_decls
                .iter()
                .any(|local| contains_guard(cx, local.ty, scope, &mut Vec::new()))
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
    let mut sets = Vec::new();
    let mut borrowed = Vec::new();
    for scope in [Scope::Critical, Scope::Any] {
        // Dropping/moving a reference never releases its caller-owned guard.
        borrowed.push(body.args_iter().any(|local| {
            let value = body.local_decls[local].ty;
            matches!(value.kind(), ty::Ref(..)) && contains_guard(cx, value, scope, &mut Vec::new())
        }));
        let mut leaves = Vec::new();
        for (local, declaration) in body.local_decls.iter_enumerated() {
            guard_leaves(
                cx,
                declaration.ty,
                mir::Place::from(local),
                scope,
                &mut leaves,
            );
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
        sets.push(guards);
    }
    if sets[1].is_empty() && !borrowed[1] {
        return;
    }
    let mut cursor = super::guard_initialization::GuardInitialization::new(cx.tcx, body, &data)
        .iterate_to_fixpoint(cx.tcx, body, None)
        .into_results_cursor(body);
    for (block, value) in body.basic_blocks.iter_enumerated() {
        let terminator = value.terminator();
        cursor.seek_before_primary_effect(mir::Location {
            block,
            statement_index: value.statements.len(),
        });
        if !borrowed[1] && !sets[1].iter().any(|guard| cursor.get().contains(*guard)) {
            continue;
        }
        // Pre-elaboration MIR retains drops of already moved values. They
        // cannot execute a destructor and must not manufacture cleanup work.
        if let mir::TerminatorKind::Drop { place, .. } = &terminator.kind {
            match data.rev_lookup.find(place.as_ref()) {
                rustc_mir_dataflow::move_paths::LookupResult::Exact(index)
                | rustc_mir_dataflow::move_paths::LookupResult::Parent(Some(index))
                    if !cursor.get().contains(index) =>
                {
                    continue
                }
                _ => {}
            }
        }
        let effects = match &terminator.kind {
            mir::TerminatorKind::Call { func, .. } => {
                super::effects::call(cx, func.ty(&body.local_decls, cx.tcx), &mut Vec::new())
            }
            mir::TerminatorKind::Drop { place, .. } => {
                super::drop_paths::live_types(cx, body, *place, |place| {
                    match data.rev_lookup.find(place.as_ref()) {
                        rustc_mir_dataflow::move_paths::LookupResult::Exact(index)
                        | rustc_mir_dataflow::move_paths::LookupResult::Parent(Some(index)) => {
                            cursor.get().contains(index)
                        }
                        rustc_mir_dataflow::move_paths::LookupResult::Parent(None) => true,
                    }
                })
                .into_iter()
                .fold(0, |effects, value| {
                    effects
                        | super::effects::drop_value(cx, value, &mut Vec::new(), &mut Vec::new())
                })
            }
            _ => continue,
        };
        let index = if effects & (super::effects::SWEEP | super::effects::UNKNOWN) != 0 {
            1
        } else if effects & super::effects::BULK != 0 {
            0
        } else {
            continue;
        };
        if borrowed[index]
            || sets[index]
                .iter()
                .any(|guard| cursor.get().contains(*guard))
        {
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
    scope: Scope,
    seen: &mut Vec<ty::Ty<'tcx>>,
) -> bool {
    super::guard_types::contains(cx, value, scope, seen)
}

// Query guard leaves rather than initialized wrapper roots. Moving one field
// must not erase its sibling, and a moved-out Result must not retain a phantom
// guard. Untracked leaves conservatively use the compiler's nearest parent.
fn guard_leaves<'tcx>(
    cx: &LateContext<'tcx>,
    value: ty::Ty<'tcx>,
    place: mir::Place<'tcx>,
    scope: Scope,
    leaves: &mut Vec<mir::Place<'tcx>>,
) {
    if !contains_guard(cx, value, scope, &mut Vec::new()) {
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
                scope,
                leaves,
            );
        }
        return;
    }
    if let ty::Adt(adt, arguments) = value.kind() {
        let name = cx.tcx.item_name(adt.did());
        let transparent = super::guard_types::direct(cx, adt.did()).is_none()
            && (adt.did().is_local()
                || (cx.tcx.crate_name(adt.did().krate).as_str() == "core"
                    && matches!(name.as_str(), "Option" | "Result")));
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
                        scope,
                        leaves,
                    );
                }
            }
            return;
        }
    }
    leaves.push(place);
}
