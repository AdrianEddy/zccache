# Real facade fixtures

Compiler checks use the actual core publication and artifact retirement APIs.

`raw_guard_memory` rejects unsafe pointer ownership duplication/overwrites and
vector length changes for guard-containing elements, including resolved aliases,
function items and UFCS. The same operations on bytes remain allowed.
