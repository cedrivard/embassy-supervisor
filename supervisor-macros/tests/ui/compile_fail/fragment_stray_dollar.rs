//! Only `$crate` may appear in a fragment — any other `$` would be read as a
//! metavariable by the relay macro_rules.

use embassy_supervisor::supervisor_fragment;

supervisor_fragment! {
    name: BAD_FRAG;
    node NET = Terminate, deps: [], task: $worker;
}

fn main() {}
