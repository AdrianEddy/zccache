//! Conservative initialized-guard dataflow over the compiler's control-flow graph.
use super::guard_types::Scope;
use rustc_hir::def_id::LocalDefId;
use rustc_lint::LateContext;
use rustc_middle::{mir, ty};

pub(super) fn check(cx: &LateContext<'_>, owner: LocalDefId) {
    use rustc_mir_dataflow::{move_paths::MoveData, Analysis};
    let (stored, _) = cx.tcx.mir_promoted(owner);
    let (mut owned, frame, aliases, suspensions) = if stored.is_stolen() {
        match super::runtime_mir::prepare(cx, cx.tcx.optimized_mir(owner).clone()) {
            Ok(prepared) => (
                prepared.body,
                prepared.frame,
                prepared.aliases,
                prepared.suspensions,
            ),
            Err(message) => {
                super::emit(cx, cx.tcx.def_span(owner), message);
                return;
            }
        }
    } else {
        ((*stored.borrow()).clone(), None, Vec::new(), Vec::new())
    };
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
    let mut leaf_sets = Vec::new();
    for scope in [Scope::Critical, Scope::Any] {
        let mut leaves = Vec::new();
        for (local, declaration) in owned.local_decls.iter_enumerated() {
            if !aliases.contains(&local) {
                guard_leaves(
                    cx,
                    declaration.ty,
                    mir::Place::from(local),
                    scope,
                    &mut leaves,
                );
            }
        }
        let mut collector = GuardPlaces {
            cx,
            body: &owned,
            scope,
            aliases: &aliases,
            leaves: &mut leaves,
        };
        rustc_middle::mir::visit::Visitor::visit_body(&mut collector, &owned);
        leaf_sets.push(leaves);
    }
    // Fake reads register exact move paths without any initialization/move
    // effects. Their block has no predecessors and never executes.
    let mut declarations = mir::BasicBlockData::new(
        Some(mir::Terminator {
            source_info: mir::SourceInfo::outermost(owned.span),
            kind: mir::TerminatorKind::Unreachable,
        }),
        false,
    );
    for place in leaf_sets.iter().flatten() {
        declarations.statements.push(mir::Statement::new(
            mir::SourceInfo::outermost(owned.span),
            mir::StatementKind::FakeRead(Box::new((mir::FakeReadCause::ForMatchGuard, *place))),
        ));
    }
    owned.basic_blocks.as_mut().push(declarations);
    let body = &owned;
    let data = MoveData::gather_moves(body, cx.tcx, |_| true);
    let mut sets = Vec::new();
    let mut borrowed = Vec::new();
    for (scope, leaves) in [Scope::Critical, Scope::Any].into_iter().zip(leaf_sets) {
        // Dropping/moving a reference never releases its caller-owned guard.
        borrowed.push(
            body.args_iter().any(|local| {
                !aliases.contains(&local)
                    && super::guard_types::borrowed(
                        cx,
                        body.local_decls[local].ty,
                        scope,
                        &mut Vec::new(),
                    )
            }) || frame.is_some_and(|local| {
                super::guard_types::borrowed(cx, body.local_decls[local].ty, scope, &mut Vec::new())
            }),
        );
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
        .with_frame(frame)
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
        if (suspensions.contains(&block)
            || matches!(terminator.kind, mir::TerminatorKind::Yield { .. }))
            && (borrowed[0] || sets[0].iter().any(|guard| cursor.get().contains(*guard)))
        {
            super::emit(
                cx,
                terminator.source_info.span,
                "short publication or store exclusion must not cross suspension",
            );
            continue;
        }
        // Pre-elaboration MIR retains drops of already moved values. They
        // cannot execute a destructor and must not manufacture cleanup work.
        if let mir::TerminatorKind::Drop { place, .. } = &terminator.kind {
            if super::completed_future::at_drop(cx, body, block, *place) {
                continue;
            }
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
    let fields = match value.kind() {
        ty::Tuple(fields) => Some(*fields),
        ty::Closure(_, args) => Some(args.as_closure().upvar_tys()),
        ty::Coroutine(_, args) => Some(args.as_coroutine().upvar_tys()),
        _ => None,
    };
    if let Some(fields) = fields {
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

struct GuardPlaces<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,
    body: &'a mir::Body<'tcx>,
    scope: Scope,
    aliases: &'a [mir::Local],
    leaves: &'a mut Vec<mir::Place<'tcx>>,
}
impl<'tcx> mir::visit::Visitor<'tcx> for GuardPlaces<'_, 'tcx> {
    fn visit_place(
        &mut self,
        place: &mir::Place<'tcx>,
        context: mir::visit::PlaceContext,
        location: mir::Location,
    ) {
        if !self.aliases.contains(&place.local) {
            guard_leaves(
                self.cx,
                place.ty(&self.body.local_decls, self.cx.tcx).ty,
                *place,
                self.scope,
                self.leaves,
            );
        }
        self.super_place(place, context, location);
    }
}
