# embassy-supervisor-observe

Observation facade for
[`embassy-supervisor`](https://docs.rs/embassy-supervisor): the one small
crate a signal library depends on so the supervisor can verify declared
dataflow against live behaviour, without the signal library depending on the
supervisor and without the supervisor knowing what a signal is. This is the
facade position the [`log`](https://docs.rs/log/latest/log/) crate holds for
logging and [`defmt`](https://defmt.ferrous-systems.com) for formatting:
everyone depends on the small crate, nobody on each other.

## The change-token contract

`Observable::change_token()` returns a `u32` with one guarantee: two unequal
readings mean the signal was written between them. The token wraps, carries
no order, and is never interpreted.

One consumer requires more. An entry that feeds a node's heartbeat
(`observed beat` in the graph) is folded with the node's other beat entries
into a single wrapping sum, so its token must be a **counting** one: each
write advances it. A value used as the token is fine for plain observation;
for a beat entry, return a counter (`Counted`, or a value that is itself
one).

## Two ways in

**Implement the trait** where your signal type lives:

```rust
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_supervisor_observe::Observable;

/// A signal type the application owns: a value plus a write count.
pub struct Reading {
    value: AtomicU32,
    writes: AtomicU32,
}

impl Reading {
    pub fn set(&self, v: u32) {
        self.value.store(v, Ordering::Relaxed);
        self.writes.fetch_add(1, Ordering::Release);
    }
}

impl Observable for Reading {
    fn change_token(&self) -> u32 {
        self.writes.load(Ordering::Acquire)
    }
}

let r = Reading { value: AtomicU32::new(0), writes: AtomicU32::new(0) };
let before = r.change_token();
r.set(7);
assert_ne!(r.change_token(), before);
```

A graph entry marked `observed` resolves its accessor in order: a per-entry
`observed via <expr>`, else the graph-level default for its direction
(`observe writes:` / `observe reads:`), else this trait.

**Wrap a primitive in `Counted`** when the type counts nothing itself:

```rust
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_supervisor_observe::{Counted, Observable, Sink};

static ESTIMATE: Counted<AtomicU32> = Counted::new(AtomicU32::new(0));

ESTIMATE.put(21); // counted write, through the `Sink` verb
ESTIMATE.put(21); // the same value again still counts
ESTIMATE.w().store(4, Ordering::Relaxed); // counted borrow; `.r()` counts a read
assert_eq!(ESTIMATE.inner().load(Ordering::Relaxed), 4); // `.inner()` counts nothing
assert_eq!(ESTIMATE.change_token(), 3);
```

`Counted` counts calls, so even a rewrite carrying the same value registers,
the one thing a value-as-token accessor cannot promise. The atomic integer
types and `AtomicBool`, in both the `core` and `portable-atomic` families,
implement `Observable` directly (value as token), with exactly that caveat.
A 64-bit (or 64-bit pointer-width) value folds its high half into the token
with a wrapping add, so a write that changes only the high bits still moves it.

## The value verbs

Two more traits ride along, behind the supervisor's `put`/`get` verbs:
`Sink` (`fn put(&self, v: Item)`) lets the verb perform the write itself,
and `Source` (`fn get(&self) -> Item`) is its snapshot-read counterpart. Both
are minimal on purpose: read-modify-write and consuming reads stay with the
signal's own API through the pass-through accessors (`writer`/`reader`). The
atomics implement both.

## Targets without atomic compare-and-swap

On a target without atomic compare-and-swap, enable one of `portable-atomic`'s
`critical-section` or `unsafe-assume-single-core` features in the application.
