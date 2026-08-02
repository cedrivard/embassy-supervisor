//! Fragment items are syntax-validated at the fragment site (with its spans),
//! not first at some distant compose site.

use embassy_supervisor::supervisor_fragment;

supervisor_fragment! {
    name: BAD_FRAG;
    node NET = Terminate; // missing the mandatory `, deps: [..]`
}

fn main() {}
