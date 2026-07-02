//! `#[cfg(...)]` on a pool *mode* is unsupported: the mode list is parsed as a
//! plain `Punctuated<Ident, ,>`, so an attribute where an identifier is expected
//! is a parse error.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    pool P = [Terminate, #[cfg(all())] OnDemand], deps: [A],
        spawn: worker,
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 1;
}

fn main() {}
