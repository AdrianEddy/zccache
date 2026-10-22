# Sweep ownership and exclusion

Work in progress for #1964; not yet ready for the final workspace gate.

The lint resolves raw tree deletion DefIds and confines them to explicit sweep,
link-removal and private staging/lifecycle owners. Function-item capture is
rejected even in approved owners to prevent exporting a raw capability.

Guard analysis uses compiler move initialization, guard field paths and actual
control-flow edges, with enum variant refinement. The compiler fixtures cover
aliases, partial moves, released wrappers and real core publication roles.

The core publication facade distinguishes blocking commit permits from complete
admission operations and async checkpoint/shutdown drains. Typed native store
commits, drain capability placement, sweep facade effects, transitive helper and
callback checks, consumed async MIR and final cross-OS checks remain unfinished.
