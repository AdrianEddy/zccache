//! The canonical blocking launcher transfers a checked closure to one task.
//! Cancellation must release captured exclusion before opaque cleanup runs.
use super::guard_types::Scope;
use rustc_lint::LateContext;
use rustc_middle::{mir, ty};

pub(super) fn moves<'tcx>(
    cx: &LateContext<'tcx>,
    body: &mir::Body<'tcx>,
    terminator: &mir::Terminator<'tcx>,
    scope: Scope,
) -> Vec<mir::Place<'tcx>> {
    let mir::TerminatorKind::Call { func, args, .. } = &terminator.kind else {
        return Vec::new();
    };
    let ty::FnDef(definition, _) = *func.ty(&body.local_decls, cx.tcx).kind() else {
        return Vec::new();
    };
    if super::canonical_path(cx, definition) != "kernal_api::async_engine::launch_blocking" {
        return Vec::new();
    }
    let Some(argument) = args.first() else {
        return Vec::new();
    };
    let mir::Operand::Move(place) = argument.node else {
        return Vec::new();
    };
    let value = place.ty(&body.local_decls, cx.tcx).ty;
    let ty::Closure(definition, arguments) = *value.kind() else {
        return Vec::new();
    };
    if !definition.is_local()
        || !super::checks_guards(cx, cx.tcx.def_span(definition))
        || super::guard_types::borrowed(cx, value, scope, &mut Vec::new())
    {
        return Vec::new();
    }
    let output = arguments.as_closure().sig().skip_binder().output();
    if !super::checked_transfer::known_output(cx, output, &mut Vec::new())
        || super::guard_types::contains(cx, output, Scope::Any, &mut Vec::new())
    {
        return Vec::new();
    }
    if !super::guard_types::contains(cx, value, scope, &mut Vec::new())
        || !cancellation_safe(cx, value, scope)
    {
        return Vec::new();
    }
    vec![place]
}

/// Capture fields have compiler-defined drop order, including unwind before
/// the body has moved them into locals. This never treats custom Drop as safe.
pub(super) fn cancellation_safe<'tcx>(
    cx: &LateContext<'tcx>,
    value: ty::Ty<'tcx>,
    scope: Scope,
) -> bool {
    let ty::Closure(_, arguments) = *value.kind() else {
        return false;
    };
    let fields = arguments.as_closure().upvar_tys();
    let owns = |field| super::guard_types::contains(cx, field, scope, &mut Vec::new());
    let mut held = fields.iter().filter(|field| owns(*field)).count();
    for field in fields {
        if held != 0 {
            let effects = super::effects::drop_value(cx, field, &mut Vec::new(), &mut Vec::new());
            let forbidden = super::effects::SWEEP
                | super::effects::UNKNOWN
                | super::effects::ESCAPE
                | if scope == Scope::Critical {
                    super::effects::BULK
                } else {
                    0
                };
            if effects & forbidden != 0 {
                return false;
            }
        }
        if owns(field) {
            if !releases(cx, field, scope) {
                return false;
            }
            held -= 1;
        }
    }
    true
}

fn releases<'tcx>(cx: &LateContext<'tcx>, value: ty::Ty<'tcx>, scope: Scope) -> bool {
    if !super::guard_types::contains(cx, value, scope, &mut Vec::new()) {
        return true;
    }
    match value.kind() {
        ty::Adt(definition, _) if super::guard_types::direct(cx, definition.did()).is_some() => {
            true
        }
        ty::Tuple(fields) => fields.iter().all(|field| releases(cx, field, scope)),
        ty::Adt(definition, arguments)
            if matches!(
                super::canonical_path(cx, definition.did()).as_str(),
                "core::option::Option" | "core::result::Result"
            ) =>
        {
            definition
                .all_fields()
                .all(|field| releases(cx, field.ty(cx.tcx, arguments).skip_norm_wip(), scope))
        }
        // Arc need not release the last owner; ManuallyDrop never releases it.
        // Other wrapper destructors require their own release proof.
        _ => false,
    }
}
