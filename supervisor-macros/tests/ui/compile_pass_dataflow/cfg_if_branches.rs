
use core::sync::atomic::AtomicU32;

use embassy_supervisor::{Supervisor, TaskNode, dataflow, supervisor_graph};

pub static RAW: AtomicU32 = AtomicU32::new(0);
pub static TEMP: AtomicU32 = AtomicU32::new(0);

// The `cfg_if!` shape without the dependency: each arm compiles where its own
// predicate holds and every earlier one does not.
macro_rules! pick {
    (if #[cfg($p:meta)] { $($a:tt)* } else { $($b:tt)* }) => {
        #[cfg($p)]
        {
            $($a)*
        }
        #[cfg(not($p))]
        {
            $($b)*
        }
    };
}

#[dataflow]
async fn sense(node: &'static TaskNode) {
    pick! {
        if #[cfg(feature = "bmi")] {
            node.put(&crate::RAW, 1);
        } else {
            node.put(&crate::RAW, 2);
        }
    }
    node.put(&crate::TEMP, 3);
    node.ack_dropped();
}

supervisor_graph! {
    node SENSE = Terminate, task: sense, discover;
}

fn main() {
    let _sup = Supervisor::new(&GRAPH);
    // `RAW` is written on both arms, so it is in the table whichever compiles,
    // and the write after the chain carries no predicate of it.
    let writes: Vec<&str> = SENSE.writes()[0].iter().map(|c| c.name()).collect();
    assert_eq!(writes, ["crate::RAW", "crate::TEMP"]);
}
