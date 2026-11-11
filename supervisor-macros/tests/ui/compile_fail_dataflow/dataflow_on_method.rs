
use embassy_supervisor::{TaskNode, dataflow};

pub struct Radio;

impl Radio {
    // The derived tables are module statics, which an `impl` block cannot hold.
    #[dataflow]
    pub fn send(&self, node: &'static TaskNode) {
        let _ = node;
    }
}

fn main() {
    Radio.send(&NODE);
}

static NODE_CFG: embassy_supervisor::NodeCfg =
    embassy_supervisor::NodeCfg::new("n", embassy_supervisor::Mode::Terminate, None);
static NODE_HANDLE: embassy_supervisor::TaskHandle = embassy_supervisor::TaskHandle::new();
static NODE: TaskNode = TaskNode::new(&NODE_CFG, &NODE_HANDLE);
