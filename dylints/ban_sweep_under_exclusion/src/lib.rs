#![feature(rustc_private)]
extern crate rustc_abi;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_mir_dataflow;

mod effects;
mod guard_analysis;
mod guard_initialization;
mod guard_types;
extern crate rustc_span;

dylint_linting::declare_late_lint! {
    /// Confine recursive deletion to cleanup owners and keep it outside exclusion.
    pub BAN_SWEEP_UNDER_EXCLUSION,
    Deny,
    "recursive cleanup must run outside publication and store exclusion"
}
use rustc_errors::DiagDecorator;
use rustc_hir::{def::Res, Expr, ExprKind};
use rustc_lint::{LateContext, LintContext};
use rustc_span::{FileName, RemapPathScopeComponents, Span};

impl<'tcx> rustc_lint::LateLintPass<'tcx> for BanSweepUnderExclusion {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        _: rustc_hir::intravisit::FnKind<'tcx>,
        _: &'tcx rustc_hir::FnDecl<'tcx>,
        _: &'tcx rustc_hir::Body<'tcx>,
        span: Span,
        owner: rustc_hir::def_id::LocalDefId,
    ) {
        let file = filename(cx, span);
        if cx.sess().opts.unstable_opts.ui_testing
            || (!cx.sess().is_test_crate()
                && (file.contains("crates/zccache-daemon-core/src/daemon/")
                    || file.contains("crates/zccache-artifact/src/kv/")))
        {
            guard_analysis::check(cx, owner);
        }
    }

    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let filename = filename(cx, expr.span);
        let ui = cx.sess().opts.unstable_opts.ui_testing;
        let production = filename.contains("crates/zccache-daemon-core/src/daemon/")
            || filename.contains("crates/zccache-artifact/src/kv/");
        if !ui
            && (!production
                || cx.sess().is_test_crate()
                || filename.contains("/tests/")
                || filename.ends_with("_tests.rs"))
        {
            return;
        }
        let definition = match expr.kind {
            ExprKind::Path(path) => match cx.qpath_res(&path, expr.hir_id) {
                Res::Def(_, definition) => definition,
                _ => return,
            },
            ExprKind::MethodCall(..) => {
                match cx.typeck_results().type_dependent_def_id(expr.hir_id) {
                    Some(definition) => definition,
                    None => return,
                }
            }
            _ => return,
        };
        let path = cx.get_def_path(definition);
        let direct = matches!(expr.kind, ExprKind::MethodCall(..))
            || matches!(cx.tcx.parent_hir_node(expr.hir_id),
            rustc_hir::Node::Expr(Expr { kind: ExprKind::Call(callee, _), .. }) if callee.hir_id == expr.hir_id);
        let drain = cx.tcx.crate_name(definition.krate).as_str() == "zccache_core"
            && path
                .iter()
                .any(|part| part.as_str() == "PublicationBarrier")
            && path.last().is_some_and(|part| part.as_str() == "drain_for");
        if drain {
            if !direct || !approved_drain_owner(cx, &filename) {
                emit(
                    cx,
                    expr.span,
                    "publication drains belong only to bounded checkpoint and shutdown owners",
                );
            }
            return;
        }
        let raw = matches!(
            cx.tcx.crate_name(definition.krate).as_str(),
            "std" | "tokio"
        ) && path.iter().any(|part| part.as_str() == "fs")
            && path
                .last()
                .is_some_and(|part| matches!(part.as_str(), "remove_dir_all" | "remove_dir"));
        if !raw {
            return;
        }
        // Even an approved owner may not export an erased raw function item.
        if direct && approved_owner(cx, &filename) {
            return;
        }
        emit(
            cx,
            expr.span,
            "raw tree deletion belongs to a typed cleanup owner",
        );
    }
}

fn enclosing_function(cx: &LateContext<'_>) -> Option<rustc_hir::def_id::DefId> {
    let Some(body) = cx.enclosing_body else {
        return None;
    };
    let mut owner = cx.tcx.hir_body_owner_def_id(body).to_def_id();
    while !matches!(
        cx.tcx.def_kind(owner),
        rustc_hir::def::DefKind::Fn | rustc_hir::def::DefKind::AssocFn
    ) {
        let Some(parent) = cx.tcx.opt_parent(owner) else {
            return None;
        };
        owner = parent;
    }
    Some(owner)
}

fn approved_drain_owner(cx: &LateContext<'_>, filename: &str) -> bool {
    let Some(owner) = enclosing_function(cx) else {
        return false;
    };
    let path = canonical_path(cx, owner);
    (path == "zccache_daemon_core::daemon::server::wal::drain_durable_state_for_shutdown"
        && filename.ends_with("/server/wal.rs"))
        || (path == "zccache_daemon_core::daemon::server::embedded::flush_embedded_state"
            && filename.ends_with("/server/embedded.rs"))
}

fn approved_owner(cx: &LateContext<'_>, filename: &str) -> bool {
    let Some(owner) = enclosing_function(cx) else {
        return false;
    };
    let name = cx.tcx.item_name(owner);
    let krate = cx.tcx.crate_name(owner.krate);
    if krate.as_str() == "zccache_artifact" {
        return filename.ends_with("crates/zccache-artifact/src/kv/retirement.rs")
            && name.as_str() == "sweep_tree";
    }
    if krate.as_str() != "zccache_daemon_core" {
        return false;
    }
    // Shared store owners. Empty trash unlink is a separate constant-depth
    // native commit; junction removal never recursively follows its target.
    if filename.ends_with("/persist/staged_store/trash.rs") {
        return matches!(name.as_str(), "remove_tree" | "try_remove_empty_trash");
    }
    if filename.ends_with("/persist/staged_store/root_safety.rs") {
        return name.as_str() == "remove_staged_link_or_reparse";
    }
    // Existing owners of private compiler output, materialization targets,
    // and lifecycle debris. These never own shared staged generations.
    const PRIVATE: &[(&str, &[&str])] = &[
        (
            "/server/staging_root.rs",
            &["cleanup_abandoned_older_than", "drop"],
        ),
        ("/server/handle_exec.rs", &["build", "cleanup"]),
        (
            "/persist/staged_plan.rs",
            &["rustc", "cc", "archive", "link", "cleanup"],
        ),
        (
            "/persist/staged_multi.rs",
            &["build_with_depfile_flag", "cleanup"],
        ),
        (
            "/persist/directory_bundle.rs",
            &["remove_directory_if_present"],
        ),
        ("/server/run.rs", &["run"]),
        ("/server/embedded.rs", &["shutdown"]),
    ];
    PRIVATE
        .iter()
        .any(|(file, functions)| filename.ends_with(file) && functions.contains(&name.as_str()))
}

fn filename(cx: &LateContext<'_>, span: Span) -> String {
    match cx.sess().source_map().span_to_filename(span) {
        FileName::Real(real) => real
            .local_path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                real.path(RemapPathScopeComponents::DIAGNOSTICS)
                    .to_string_lossy()
                    .into_owned()
            }),
        other => other
            .display(RemapPathScopeComponents::DIAGNOSTICS)
            .to_string(),
    }
    .replace('\\', "/")
}

fn emit(cx: &LateContext<'_>, span: Span, message: &str) {
    cx.opt_span_lint(
        BAN_SWEEP_UNDER_EXCLUSION,
        Some(span),
        DiagDecorator(|diag| {
            diag.primary_message(message.to_owned());
        }),
    );
}

#[cfg(test)]
mod ui_test_support {
    include!("../../ui_test_support.rs");
}
#[cfg(test)]
mod ui_dependencies;

#[test]
fn ui() {
    ui_test_support::run(env!("CARGO_PKG_NAME"), env!("CARGO_MANIFEST_DIR"));
    let _directory =
        ui_test_support::CurrentDirGuard::set(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    dylint_testing::ui::Test::src_base(env!("CARGO_PKG_NAME"), "examples")
        .rustc_flags(ui_dependencies::dependency_flags(
            std::time::Duration::from_secs(300),
        ))
        .run();
}

fn canonical_path(cx: &LateContext<'_>, definition: rustc_hir::def_id::DefId) -> String {
    cx.get_def_path(definition)
        .iter()
        .map(|part| part.as_str().to_string())
        .collect::<Vec<_>>()
        .join("::")
}
