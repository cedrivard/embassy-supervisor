
use embassy_supervisor::supervisor_fragment;

supervisor_fragment! {
    name: BAD_FRAG;
    node NET = Terminate, deps [NOPE]; 
}

fn main() {}
