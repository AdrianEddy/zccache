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
    seen: &mut Vec<DefId>,
) -> u8 {
    match callee.kind() {
        ty::FnDef(definition, arguments) => {
            let path = cx.get_def_path(*definition);
            let name = path.last().map(|name| name.as_str());
            if matches!(name, Some("call" | "call_once" | "call_mut"))
                && matches!(cx.tcx.crate_name(definition.krate).as_str(), "core" | "std")
            {
                return match arguments.types().next().map(|value| *value.kind()) {
                    Some(ty::Closure(definition, _)) | Some(ty::FnDef(definition, _)) => {
                        function(cx, definition, seen)
                    }
                    _ => UNKNOWN,
                };
            }
            function(cx, *definition, seen)
        }
        ty::FnPtr(..) => UNKNOWN,
        _ => UNKNOWN,
    }
}

fn function(cx: &LateContext<'_>, definition: DefId, seen: &mut Vec<DefId>) -> u8 {
    if seen.contains(&definition) {
        return 0;
    }
    seen.push(definition);
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
        return 0;
    };
    if !matches!(
        cx.tcx.def_kind(definition),
        rustc_hir::def::DefKind::Fn
            | rustc_hir::def::DefKind::AssocFn
            | rustc_hir::def::DefKind::Closure
    ) {
        return 0;
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
    calls
        .into_iter()
        .fold(0, |effects, target| effects | call(cx, target, seen))
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
