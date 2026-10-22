//! Prove a local enum's sole initialized variant without assuming mutable aliases.
use rustc_abi::VariantIdx;
use rustc_middle::mir;

pub(super) fn known<'tcx>(body: &mir::Body<'tcx>, place: mir::Place<'tcx>) -> Option<VariantIdx> {
    if body.args_iter().any(|argument| argument == place.local) {
        return None;
    }
    let mut variant = None;
    for block in body.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                mir::StatementKind::Assign(assignment) => {
                    if overlaps(assignment.0, place) {
                        if assignment.0 != place || variant.is_some() {
                            return None;
                        }
                        let mir::Rvalue::Aggregate(kind, _) = &assignment.1 else {
                            return None;
                        };
                        let mir::AggregateKind::Adt(_, value, ..) = **kind else {
                            return None;
                        };
                        variant = Some(value);
                    }
                    // Even a sole assignment does not prove the discriminant
                    // if a reference or raw pointer can mutate its storage.
                    match &assignment.1 {
                        mir::Rvalue::Ref(_, _, borrowed) | mir::Rvalue::RawPtr(_, borrowed)
                            if overlaps(*borrowed, place) =>
                        {
                            return None
                        }
                        _ => {}
                    }
                }
                mir::StatementKind::SetDiscriminant { place: changed, .. }
                    if overlaps(**changed, place) =>
                {
                    return None
                }
                _ => {}
            }
        }
        match &block.terminator().kind {
            mir::TerminatorKind::Call { destination, .. } if overlaps(*destination, place) => {
                return None
            }
            mir::TerminatorKind::InlineAsm { .. } => return None,
            _ => {}
        }
    }
    variant
}
fn overlaps<'tcx>(left: mir::Place<'tcx>, right: mir::Place<'tcx>) -> bool {
    left.local == right.local
        && (left.projection.starts_with(right.projection)
            || right.projection.starts_with(left.projection))
}
