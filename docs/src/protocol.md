# Guest protocol & migration state machine

> This is the **integration contract** — the versioned surface a non-Rust workload can
> speak without depending on sleepwalk's source. The canonical copy is
> [`docs/protocol.md`](https://github.com/thearyanahmed/sleepwalk/blob/master/docs/protocol.md),
> included verbatim below. The Rust types in the [`proto`](architecture/crates.md#proto)
> crate mirror it; where they ever disagree, this document is the spec.

{{#include ../protocol.md}}
