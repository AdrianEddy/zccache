# Real facade fixtures

Compiler checks use the actual core publication and artifact retirement APIs.

`raw_guard_memory` rejects unsafe pointer ownership duplication/overwrites and
vector length changes for guard-containing elements, including resolved aliases,
function items and UFCS. The same operations on bytes remain allowed.

`ordered_local_unwind` covers release after tuple destructuring, rebinding,
branching and `?`. Its negative companion retains a live destructor after a
partial move, which must keep the real unwind and cleanup violation visible.
