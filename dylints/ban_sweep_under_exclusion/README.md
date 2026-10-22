# Sweep ownership and exclusion

Work in progress for #1964; not yet ready for the final workspace gate.

The lint resolves raw tree deletion DefIds and confines them to explicit sweep,
link-removal and private staging/lifecycle owners. Function-item capture is
rejected even in approved owners to prevent exporting a raw capability.

Guard analysis uses compiler move initialization, guard field paths and actual
control-flow edges, with enum variant refinement. The compiler fixtures cover
aliases, partial moves, released wrappers and real core publication roles.

Instantiated helper summaries propagate ownership leaks and ownership-erasing
casts, including generic `forget` and `Box<dyn Any>` helpers. Non-owning markers
do not acquire ownership merely by naming a permit type. Coroutine cancellation
includes captured and saved-local destructors; a direct coroutine drop following
a proven `Poll::Ready` uses a separate completion proof. Custom futures retain
their destructor effects after `Ready`.

Owned transfers into checked synchronous local helpers use the callee's explicit
guard parameter types and move paths. Other caller-held guards remain live.
Generic parameters, borrowed guards, foreign callees, async callees and opaque
or guard-owning return values do not establish this transfer contract. The
callee remains subject to guard checks, including destructor and unwind paths.

The core publication facade distinguishes blocking commit permits from complete
admission operations and async checkpoint/shutdown drains. The workspace gate
still fails on owned task transfers and opaque drop effects. Cancellation glue
with retained guards, blocking-lane enforcement, the remaining ownership audit,
and final OS/feature checks remain unfinished.
