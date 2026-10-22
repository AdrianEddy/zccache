//! Resolved bulk/sweep effects, including local function and closure bodies.
use rustc_hir::def_id::DefId;
use rustc_lint::LateContext;
use rustc_middle::{mir, ty};

pub(super) const BULK: u8 = 1;
pub(super) const SWEEP: u8 = 2;
pub(super) const UNKNOWN: u8 = 4;

pub(super) fn call<'tcx>(
    cx: &LateContext<'tcx>,
    callee: ty::Ty<'tcx>,
    seen: &mut Vec<(DefId, ty::GenericArgsRef<'tcx>, Option<u8>)>,
) -> u8 {
    match callee.kind() {
        ty::FnDef(definition, arguments) => {
            let path = cx.get_def_path(*definition);
            let name = path.last().map(|name| name.as_str());
            if matches!(name, Some("call" | "call_once" | "call_mut"))
                && matches!(cx.tcx.crate_name(definition.krate).as_str(), "core" | "std")
            {
                return match arguments.types().next().map(|value| *value.kind()) {
                    Some(ty::Closure(definition, args)) | Some(ty::FnDef(definition, args)) => {
                        function(cx, definition, args, seen)
                    }
                    _ => UNKNOWN,
                };
            }
            function(cx, *definition, arguments, seen)
        }
        ty::FnPtr(..) => UNKNOWN,
        _ => UNKNOWN,
    }
}

fn function<'tcx>(
    cx: &LateContext<'tcx>,
    definition: DefId,
    arguments: ty::GenericArgsRef<'tcx>,
    seen: &mut Vec<(DefId, ty::GenericArgsRef<'tcx>, Option<u8>)>,
) -> u8 {
    if let Some((_, _, effects)) = seen
        .iter()
        .find(|(id, args, _)| *id == definition && *args == arguments)
    {
        return effects.unwrap_or(0);
    }
    if seen
        .iter()
        .filter(|(_, _, effects)| effects.is_none())
        .count()
        >= 128
    {
        return UNKNOWN;
    }
    let index = seen.len();
    seen.push((definition, arguments, None));
    let result = (|| {
        let path = cx.get_def_path(definition);
        let name = path.last().map(|name| name.as_str()).unwrap_or("");
        let krate = cx.tcx.crate_name(definition.krate);
        if matches!(krate.as_str(), "zccache_artifact" | "zccache_daemon_core")
            && (path
                .iter()
                .any(|part| matches!(part.as_str(), "DetachedKvNamespace" | "DetachedStagedTrash"))
                && matches!(name, "sweep_for" | "sweep_until")
                || matches!(
                    name,
                    "sweep_staged_trash_for" | "sweep_retired_namespaces_for"
                ))
        {
            return SWEEP;
        }
        if matches!(krate.as_str(), "std" | "tokio")
            && (cx.tcx.def_kind(definition) == rustc_hir::def::DefKind::Fn
                && path.iter().any(|part| part.as_str() == "fs")
                && matches!(
                    name,
                    "read_dir" | "read" | "write" | "copy" | "remove_dir_all"
                )
                || matches!(
                    name,
                    "read_to_end" | "read_to_string" | "write_all" | "sync_all" | "sync_data"
                ))
        {
            return BULK;
        }
        let Some(local) = definition.as_local() else {
            // Foreign callback combinators (Iterator::for_each, etc.) do not
            // expose their MIR here. Their concrete local closure bodies do.
            // Conservatively propagate those effects rather than treating the
            // foreign function as a cleanup-free boundary.
            return arguments
                .types()
                .fold(0, |effects, value| match value.kind() {
                    ty::Closure(callback, arguments) => {
                        effects | function(cx, *callback, arguments, seen)
                    }
                    _ => effects,
                });
        };
        if !matches!(
            cx.tcx.def_kind(definition),
            rustc_hir::def::DefKind::Fn
                | rustc_hir::def::DefKind::AssocFn
                | rustc_hir::def::DefKind::Closure
        ) {
            return 0;
        }
        // Required trait methods have no MIR body. Their dynamic implementation
        // cannot be assumed free of cleanup, and querying MIR would ICE rustc.
        if cx.tcx.hir_maybe_body_owned_by(local).is_none() {
            return UNKNOWN;
        }
        // Release the query borrow before following callees: a query for a callee
        // may legitimately advance/consume MIR for an enclosing function.
        let calls = {
            let (stored, _) = cx.tcx.mir_promoted(local);
            if stored.is_stolen() {
                targets(cx, cx.tcx.optimized_mir(local))
            } else {
                targets(cx, &stored.borrow())
            }
        };
        calls.into_iter().fold(0, |effects, target| {
            // Only this sealed cursor's direct one-level directory open is
            // metadata work. Every other effect in its body/helper graph
            // remains subject to the normal exclusion policy.
            if super::detach_policy::cursor_open(cx, definition)
                && super::detach_policy::raw_read_dir(cx, target)
            {
                return effects;
            }
            let target = ty::EarlyBinder::bind(target)
                .instantiate(cx.tcx, arguments)
                .skip_norm_wip();
            effects | call(cx, target, seen)
        })
    })();
    seen[index].2 = Some(result);
    result
}

fn targets<'tcx>(cx: &LateContext<'tcx>, body: &mir::Body<'tcx>) -> Vec<ty::Ty<'tcx>> {
    body.basic_blocks
        .iter()
        .filter_map(|block| match &block.terminator().kind {
            mir::TerminatorKind::Call { func, .. } => Some(func.ty(&body.local_decls, cx.tcx)),
            _ => None,
        })
        .collect()
}
