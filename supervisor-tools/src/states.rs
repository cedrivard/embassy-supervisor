//! Rendering supervisor node lifecycles as Mermaid state diagrams.

use std::collections::BTreeMap;
use std::fmt::Write;

use embassy_supervisor_syntax::{Item, NodeItem, PoolItem};

use crate::find::Decl;
use crate::render::Options;

/// Lifecycle shape key used to group nodes with identical gate sets.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct Shape {
    mode: String,
    pool: bool,
    parked: bool,
    disabled: bool,
    cancel: bool,
    asserts_ready: bool,
    ready_on_write: bool,
    bound: bool,
    beat: Option<String>,
}

#[derive(Default)]
struct Gates {
    takes: Vec<String>,
    waits: Vec<String>,
    clears: Vec<String>,
    within: Option<String>,
    reads: Vec<String>,
    writes: Vec<String>,
}

fn node_gates(n: &NodeItem) -> Gates {
    let mut g = Gates::default();
    for r in &n.resources {
        let kind = if r.shared.is_some() {
            " (shared)"
        } else if r.local.is_some() {
            " (local)"
        } else if r.consume.is_some() {
            " (consume)"
        } else {
            ""
        };
        g.takes.push(format!("{}{kind}", r.ident));
    }
    g.waits = n
        .deps
        .iter()
        .filter(|d| d.ready.is_some() || d.bound.is_some())
        .map(|d| d.ident.to_string())
        .collect();
    g.clears = n.provides.iter().map(|i| i.to_string()).collect();
    g.within = n.slot_timeout.as_ref().map(|t| t.to_string());
    for (is_write, sig) in [(false, &n.reads), (true, &n.writes)] {
        for decl in sig.iter() {
            let d = decl.display();
            let short = d.rsplit("::").next().unwrap_or(&d).to_string();
            if is_write {
                g.writes.push(short);
            } else {
                g.reads.push(short);
            }
        }
    }
    g
}

fn pool_gates(p: &PoolItem) -> Gates {
    let mut g = Gates {
        takes: p
            .resources
            .iter()
            .map(|r| {
                let kind = if r.shared.is_some() {
                    " (shared)"
                } else if r.local.is_some() {
                    " (local)"
                } else if r.consume.is_some() {
                    " (consume)"
                } else {
                    ""
                };
                format!("{}{kind}", r.ident)
            })
            .collect(),
        waits: p
            .deps
            .iter()
            .filter(|d| d.ready.is_some() || d.bound.is_some())
            .map(|d| d.ident.to_string())
            .collect(),
        within: p.slot_timeout.as_ref().map(|t| t.to_string()),
        ..Gates::default()
    };
    for (is_write, sig) in [(false, &p.reads), (true, &p.writes)] {
        for decl in sig.iter() {
            let d = decl.display();
            let short = d.rsplit("::").next().unwrap_or(&d).to_string();
            if is_write {
                g.writes.push(short);
            } else {
                g.reads.push(short);
            }
        }
    }
    g
}

/// Render a declaration as a Mermaid state diagram of node lifecycles.
pub fn render(decl: &Decl, opts: &Options) -> String {
    let items = &decl.spec.items;
    let mut groups: Vec<(Shape, Vec<String>, Gates)> = Vec::new();
    let mut grouped: BTreeMap<Shape, Vec<String>> = BTreeMap::new();
    for item in items {
        match item {
            Item::Node(n) if opts.signals => {
                groups.push((
                    node_shape(n, items),
                    vec![n.ident.to_string()],
                    node_gates(n),
                ));
            }
            Item::Pool(p) if opts.signals => {
                let mut by_mode: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                for (i, m) in p.modes.iter().enumerate() {
                    by_mode.entry(m.to_string()).or_default().push(i);
                }
                for (mode, members) in by_mode {
                    groups.push((
                        pool_shape(p, items, &mode),
                        vec![format!("{} {}", p.ident, member_range(&members))],
                        pool_gates(p),
                    ));
                }
            }
            Item::Node(n) => grouped
                .entry(node_shape(n, items))
                .or_default()
                .push(n.ident.to_string()),
            Item::Pool(p) => {
                let mut by_mode: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                for (i, m) in p.modes.iter().enumerate() {
                    by_mode.entry(m.to_string()).or_default().push(i);
                }
                for (mode, members) in by_mode {
                    grouped
                        .entry(pool_shape(p, items, &mode))
                        .or_default()
                        .push(format!("{} {}", p.ident, member_range(&members)));
                }
            }
            Item::Executor(_) => {}
        }
    }
    groups.extend(
        grouped
            .into_iter()
            .map(|(shape, members)| (shape, members, Gates::default())),
    );

    let mut out = String::new();
    let title = crate::render::diagram_title(decl, opts);
    out.push_str(&crate::render::frontmatter(&title, opts));
    let _ = writeln!(
        out,
        "%% node lifecycles — {title}  ({}:{})",
        decl.origin, decl.line
    );
    out.push_str(&crate::render::spacing_init(opts));
    let _ = writeln!(out, "stateDiagram-v2");
    let _ = writeln!(
        out,
        "  accDescr: supervisor node lifecycles declared at {}:{}",
        decl.origin, decl.line
    );
    let dir = match opts.direction.as_str() {
        "TD" => "TB",
        other => other,
    };
    let _ = writeln!(out, "  direction {dir}");

    if groups.is_empty() {
        let _ = writeln!(out, "  state \"no nodes declared\" as empty");
        return out;
    }

    for (i, (shape, members, gates)) in groups.iter().enumerate() {
        let _ = writeln!(out);
        let g = opts.signals.then_some(gates);
        let _ = write!(out, "{}", composite(i, shape, members, dir, g));
    }
    if opts.legend {
        let _ = writeln!(out);
        let _ = write!(out, "{}", legend(dir));
    }
    out
}

/// Return the Mermaid source for the state-diagram legend.
pub(crate) fn legend(dir: &str) -> String {
    format!(
        "  state \"legend — one composite per lifecycle shape, shared by the nodes it names\" as legend {{\n\
        \x20   direction {dir}\n\
        \x20   state \"down\" as lg_down\n\
        \x20   state \"running\" as lg_run\n\
        \x20   [*] --> lg_down: where the node begins\n\
        \x20   lg_down --> lg_run: a supervisor operation moves it\n\
        \x20   lg_run --> lg_run: an observation, and it stays put\n\
        \x20 }}\n"
    )
}

fn member_range(members: &[usize]) -> String {
    match members {
        [only] => format!("member {only}"),
        [first, .., last] => format!("members {first}..{last}"),
        [] => "no members".to_string(),
    }
}

fn node_shape(n: &NodeItem, items: &[Item]) -> Shape {
    Shape {
        mode: n.mode.to_string(),
        pool: false,
        parked: n.source.is_none(),
        disabled: n.disabled,
        cancel: n.cancel,
        asserts_ready: gated_by_readiness(&n.ident.to_string(), items),
        ready_on_write: n.ready_on_write.is_some(),
        bound: n.deps.iter().any(|d| d.bound.is_some()),
        beat: n.beat_timeout.as_ref().map(|(_, ms)| ms.to_string()),
    }
}

fn pool_shape(p: &PoolItem, items: &[Item], mode: &str) -> Shape {
    Shape {
        mode: mode.to_string(),
        pool: true,
        parked: false,
        disabled: false,
        cancel: p.cancel,
        asserts_ready: gated_by_readiness(&p.ident.to_string(), items),
        ready_on_write: false,
        bound: p.deps.iter().any(|d| d.bound.is_some()),
        beat: None,
    }
}

fn gated_by_readiness(name: &str, items: &[Item]) -> bool {
    items.iter().any(|i| {
        let deps = match i {
            Item::Node(n) => &n.deps,
            Item::Pool(p) => &p.deps,
            Item::Executor(_) => return false,
        };
        deps.iter()
            .any(|d| d.ident == name && (d.ready.is_some() || d.bound.is_some()))
    })
}

fn composite(
    i: usize,
    shape: &Shape,
    members: &[String],
    dir: &str,
    gates: Option<&Gates>,
) -> String {
    let up_gates = gates.map_or(String::new(), |g| {
        let mut parts = Vec::new();
        if !g.takes.is_empty() {
            parts.push(format!("takes {}", g.takes.join(", ")));
        }
        if !g.waits.is_empty() {
            parts.push(format!("waits {} ready", g.waits.join(", ")));
        }
        if let (Some(ms), false) = (&g.within, parts.is_empty()) {
            parts.push(format!("within {ms} ms"));
        }
        parts.iter().map(|p| format!(" · {p}")).collect::<String>()
    });
    let down_gates = gates.map_or(String::new(), |g| {
        if g.clears.is_empty() {
            String::new()
        } else {
            format!(" · clears {}", g.clears.join(", "))
        }
    });
    let g = format!("g{i}");
    let mut s = String::new();
    let mut title = label(shape, members);
    if let Some(g) = gates {
        if !g.reads.is_empty() {
            title.push_str(&format!(" · reads {}", g.reads.join(", ")));
        }
        if !g.writes.is_empty() {
            title.push_str(&format!(" · writes {}", g.writes.join(", ")));
        }
    }
    let _ = writeln!(s, "  state \"{}\" as {g} {{", esc(&title));
    let _ = writeln!(s, "    direction {dir}");

    let running = format!("{g}_run");
    let down = format!("{g}_down");

    if shape.disabled {
        let _ = writeln!(s, "    state \"disabled\" as {g}_off");
        let _ = writeln!(s, "    [*] --> {g}_off");
        let _ = writeln!(
            s,
            "    {g}_off --> {down}: activate · the one path that clears the latch"
        );
    }

    let _ = writeln!(s, "    state \"down\" as {down}");
    if shape.asserts_ready {
        let assertion = if shape.ready_on_write {
            "the sweep sees a declared write advance"
        } else {
            "the task calls set_ready()"
        };
        let _ = writeln!(s, "    state \"running\" as {running} {{");
        let _ = writeln!(s, "      direction {dir}");
        let _ = writeln!(s, "      state \"starting\" as {g}_starting");
        let _ = writeln!(s, "      state \"ready\" as {g}_ready");
        let _ = writeln!(s, "      [*] --> {g}_starting");
        let _ = writeln!(s, "      {g}_starting --> {g}_ready: {assertion}");
        let _ = writeln!(
            s,
            "      {g}_ready --> {g}_starting: clear_ready() · dependents wait again"
        );
        let _ = writeln!(s, "    }}");
    } else {
        let _ = writeln!(s, "    state \"running\" as {running}");
    }

    if !shape.disabled {
        let _ = writeln!(s, "    [*] --> {down}");
    }

    match shape.mode.rsplit("::").next().unwrap_or(&shape.mode) {
        "Pause" => {
            let _ = writeln!(s, "    state \"parked\" as {g}_parked");
            let _ = writeln!(
                s,
                "    {down} --> {running}: {}{up_gates}",
                start_label(shape)
            );
            let _ = writeln!(
                s,
                "    {running} --> {g}_parked: teardown or stop_node · acks, then parks on wait_resume(){down_gates}"
            );
            let _ = writeln!(
                s,
                "    {g}_parked --> {running}: resume_node or resume_pausable · resumed in place, keeps its resources"
            );
        }
        "OnDemand" => {
            let _ = writeln!(
                s,
                "    {down} --> {running}: start_node · the pool grows under load, start() skips it{up_gates}"
            );
            let _ = writeln!(
                s,
                "    {running} --> {down}: stop_node · the pool shrinks{}{down_gates}",
                if shape.cancel {
                    ", its future dropped in place"
                } else {
                    ", acks and exits"
                }
            );
        }
        _ => {
            let _ = writeln!(
                s,
                "    {down} --> {running}: {}{up_gates}",
                start_label(shape)
            );
            let _ = writeln!(
                s,
                "    {running} --> {down}: teardown or stop_node · {}{down_gates}",
                if shape.cancel {
                    "future dropped in place, the shell still runs its tail"
                } else {
                    "acks, exits its loop"
                }
            );
            if !shape.pool {
                let _ = writeln!(s, "    {down} --> {running}: respawn_terminate");
            }
        }
    }

    if shape.bound {
        let is_pause = shape.mode.rsplit("::").next().unwrap_or(&shape.mode) == "Pause";
        let stopped = if is_pause {
            format!("{g}_parked")
        } else {
            down.clone()
        };
        let _ = writeln!(
            s,
            "    {running} --> {stopped}: a bound dep called clear_ready()"
        );
        let back = if is_pause {
            "that dep is ready again · resumed in place"
        } else {
            "that dep is ready again · brought back through the gates"
        };
        let _ = writeln!(s, "    {stopped} --> {running}: {back}");
    }

    if let Some(ms) = &shape.beat {
        let _ = writeln!(
            s,
            "    {running} --> {running}: no beat for {ms} ms · reported stale, still running"
        );
    }

    let _ = writeln!(s, "  }}");
    s
}

fn start_label(shape: &Shape) -> String {
    if shape.parked {
        "start · only marked running, the application spawns the task".to_string()
    } else if shape.pool {
        "the pool policy brings the member up".to_string()
    } else {
        "start · spawned in dep order".to_string()
    }
}

fn label(shape: &Shape, members: &[String]) -> String {
    let mut facts = vec![shape.mode.clone()];
    if shape.pool {
        facts.push("pool member".to_string());
    }
    if shape.parked {
        facts.push("parked".to_string());
    }
    if shape.disabled {
        facts.push("disabled".to_string());
    }
    if shape.cancel {
        facts.push("cancel".to_string());
    }
    if shape.ready_on_write {
        facts.push("ready_on_write".to_string());
    } else if shape.asserts_ready {
        facts.push("gates dependents".to_string());
    }
    if shape.bound {
        facts.push("bound".to_string());
    }
    if let Some(ms) = &shape.beat {
        facts.push(format!("beat {ms}"));
    }
    const SHOWN: usize = 4;
    let names = if members.len() > SHOWN {
        format!(
            "{}, +{} more",
            members[..SHOWN].join(", "),
            members.len() - SHOWN
        )
    } else {
        members.join(", ")
    };
    format!("{} — {names}", facts.join(" · "))
}

fn esc(s: &str) -> String {
    s.replace('"', "#quot;")
}
