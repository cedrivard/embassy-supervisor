//! Rendering supervisor task graphs as Mermaid diagrams.

use std::collections::BTreeMap;
use std::fmt::Write;

use embassy_supervisor_syntax::{
    Dep, Item, NodeItem, PoolItem, ResourceDecl, SignalDecl, TaskSource, path_to_string,
};
use quote::ToTokens;

use crate::find::{Decl, UNRESOLVED_CRATE};

/// Rendering options for a Mermaid diagram.
#[derive(Clone)]
pub struct Options {
    /// Flowchart direction (`TD`, `LR`, `RL`, or `BT`).
    pub direction: String,
    /// Draw declared signal couplings.
    pub signals: bool,
    /// Add a legend subgraph.
    pub legend: bool,
    /// Show full signal paths instead of shortened names.
    pub full_paths: bool,
    /// Include `#[cfg(...)]` predicates in labels.
    pub show_cfg: bool,
    /// Group items by fragment.
    pub fragments: bool,
    /// Render lifecycle state diagrams instead of flowcharts.
    pub states: bool,
    /// Horizontal node spacing, in pixels.
    pub h_spacing: Option<u32>,
    /// Vertical node spacing, in pixels.
    pub v_spacing: Option<u32>,
    /// Render runtime coupling instead of bring-up deps.
    pub runtime: bool,
    /// Include all bring-up deps as dotted context in runtime mode.
    pub runtime_deps: bool,
    /// Include bring-up deps only for uncoupled nodes in runtime mode.
    pub anchor_uncoupled: bool,
    /// Discovered accesses to overlay on the diagram.
    pub discovered: Vec<crate::Discovered>,
    /// Named Mermaid layout engine.
    pub layout: Option<String>,
    /// Override diagram title.
    pub title: Option<String>,
    /// Show the title in the generated frontmatter.
    pub show_title: bool,
    /// Group items by executor.
    pub executors: bool,
    /// Collapse signal fan-out above this many readers (0 means off).
    pub max_fanout: usize,
    /// Bundles to expand when matching discovered accesses.
    pub bundles: Vec<crate::Bundle>,
    /// Click-link template, e.g. a URL with `{file}` and `{line}` placeholders.
    pub links: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            direction: "TD".to_string(),
            signals: false,
            legend: false,
            full_paths: false,
            show_cfg: true,
            fragments: false,
            states: false,
            h_spacing: None,
            v_spacing: None,
            runtime: false,
            runtime_deps: false,
            anchor_uncoupled: false,
            discovered: Vec::new(),
            bundles: Vec::new(),
            layout: None,
            title: None,
            show_title: true,
            executors: false,
            max_fanout: 0,
            links: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeDepEdges {
    None,
    Anchors,
    All,
}

fn runtime_dep_edges(opts: &Options) -> RuntimeDepEdges {
    if opts.runtime_deps {
        RuntimeDepEdges::All
    } else if opts.anchor_uncoupled {
        RuntimeDepEdges::Anchors
    } else {
        RuntimeDepEdges::None
    }
}

#[derive(Default)]
struct Ids {
    map: BTreeMap<(String, String), String>,
    used: BTreeMap<String, usize>,
    issued: std::collections::BTreeSet<String>,
}

impl Ids {
    fn get(&mut self, prefix: &str, key: &str) -> String {
        let map_key = (prefix.to_string(), key.to_string());
        if let Some(id) = self.map.get(&map_key) {
            return id.clone();
        }
        let base: String = key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let base = format!("{prefix}_{base}");
        let n = self.used.entry(base.clone()).or_insert(0);
        let mut id = if *n == 0 {
            base.clone()
        } else {
            format!("{base}__{n}")
        };
        *n += 1;
        while self.issued.contains(&id) {
            id = format!("{base}__{n}");
            *n += 1;
        }
        self.issued.insert(id.clone());
        self.map.insert(map_key, id.clone());
        id
    }
}

/// Render a declaration as a Mermaid flowchart diagram.
pub fn render(decl: &Decl, opts: &Options) -> String {
    let mut out = String::new();
    let mut ids = Ids::default();
    let items = &decl.spec.items;

    let title = diagram_title(decl, opts);
    out.push_str(&frontmatter(&title, opts));
    out.push_str(&spacing_init(opts));
    let kind = if opts.runtime {
        "runtime coupling — "
    } else {
        ""
    };
    let _ = writeln!(out, "%% {kind}{title}  ({}:{})", decl.origin, decl.line);
    let _ = writeln!(out, "flowchart {}", opts.direction);
    let _ = writeln!(
        out,
        "  accDescr: supervisor task graph declared at {}:{}",
        decl.origin, decl.line
    );
    if opts.legend {
        let _ = writeln!(out, "  subgraph {MAIN}[\" \"]");
        let _ = writeln!(out, "    direction {}", opts.direction);
    }

    let declared: Vec<String> = items.iter().filter_map(boxed_name).collect();

    let group_of = |item: &Item| -> Option<String> {
        if opts.executors {
            Some(match executor_of(item) {
                Some(e) => format!("@{e}"),
                None => "thread mode".to_string(),
            })
        } else if opts.fragments {
            fragment_of(item).map(str::to_string)
        } else {
            None
        }
    };
    let groups: Vec<Option<String>> = if opts.fragments || opts.executors {
        let mut g = Vec::new();
        for item in items {
            let f = group_of(item);
            if !g.contains(&f) {
                g.push(f);
            }
        }
        g
    } else {
        vec![None]
    };

    for group in &groups {
        let members: Vec<&Item> = items
            .iter()
            .filter(|i| {
                boxed_name(i).is_some()
                    && ((!opts.fragments && !opts.executors) || group_of(i) == *group)
            })
            .collect();
        if members.is_empty() {
            continue;
        }
        let indent = match group {
            Some(name) if groups.len() > 1 => {
                let gid = ids.get("f", name);
                let _ = writeln!(out, "  subgraph {gid}[\"{}\"]", esc(name));
                let _ = writeln!(out, "    direction {}", opts.direction);
                "    "
            }
            _ => "  ",
        };
        for item in members {
            let name = boxed_name(item).unwrap();
            let id = ids.get("n", &name);
            let _ = writeln!(out, "{indent}{}", shape(&id, item, opts));
        }
        if indent.len() > 2 {
            let _ = writeln!(out, "  end");
        }
    }

    if let Some(tpl) = &opts.links {
        for item in items {
            let Some(name) = boxed_name(item) else {
                continue;
            };
            let origin = link_origin(decl, item);
            let file = std::fs::canonicalize(origin)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| origin.to_string());
            let line = item_line(item).max(1);
            let url = tpl
                .replace("{file}", &file)
                .replace("{line}", &line.to_string());
            let id = ids.get("n", &name);
            let _ = writeln!(out, "  click {id} \"{}\"", esc(&url));
        }
    }

    let graph_module = crate::module_of(&decl.origin);
    let runtime_dep_edges = runtime_dep_edges(opts);
    let uncoupled: Vec<String> = if opts.runtime && runtime_dep_edges == RuntimeDepEdges::Anchors {
        items
            .iter()
            .filter(|i| {
                signals_of(i).is_empty()
                    && resources_of(i).is_empty()
                    && !has_discovered_access(i, &graph_module, &opts.discovered, &opts.bundles)
            })
            .filter_map(boxed_name)
            .collect()
    } else {
        Vec::new()
    };
    let dep_edges: Vec<(String, &Dep)> = items
        .iter()
        .filter_map(|i| boxed_name(i).map(|n| (n, deps_of(i))))
        .flat_map(|(name, deps)| deps.iter().map(move |d| (name.clone(), d)))
        .filter(|(name, dep)| match () {
            _ if !opts.runtime => true,
            _ if runtime_dep_edges == RuntimeDepEdges::All => true,
            _ if runtime_dep_edges == RuntimeDepEdges::Anchors => {
                uncoupled.contains(name) || uncoupled.contains(&dep.ident.to_string())
            }
            _ => false,
        })
        .collect();

    let mut externals: Vec<String> = Vec::new();
    for (_, dep) in &dep_edges {
        let name = dep.ident.to_string();
        if !declared.contains(&name) && !externals.contains(&name) {
            externals.push(name);
        }
    }
    for name in &externals {
        let id = ids.get("n", name);
        let _ = writeln!(out, "  {id}([\"{}\"])", esc(name));
    }

    if !dep_edges.is_empty() {
        let _ = writeln!(out);
    }

    for (name, dep) in &dep_edges {
        let to = ids.get("n", name);
        let from = ids.get("n", &dep.ident.to_string());
        let mut label = markers(dep);
        if let Some(c) = cfg_note(&dep.cfg, opts.show_cfg) {
            label = if label.is_empty() {
                c
            } else {
                format!("{label} · {c}")
            };
        }
        if opts.runtime {
            let label = if label.is_empty() {
                "spawn".to_string()
            } else {
                format!("spawn · {label}")
            };
            let _ = writeln!(out, "  {from} -. \"{}\" .-> {to}", esc(&label));
        } else {
            let _ = writeln!(out, "  {}", dep_edge(&from, &to, dep, &label));
        }
    }

    if opts.signals || opts.runtime {
        let _ = writeln!(out);
        let dotted_signal_edges = opts.signals && !opts.runtime;
        let labels = signal_labels(items, opts.full_paths);
        let mut emitted: Vec<String> = Vec::new();
        for item in items {
            for (_, sig) in signals_of(item) {
                let key = sig.display();
                if emitted.contains(&key) {
                    continue;
                }
                emitted.push(key.clone());
                let id = ids.get("s", &key);
                let label = labels.get(&key).cloned().unwrap_or(key);
                let _ = writeln!(out, "  {id}[/\"{}\"/]", esc(&unresolved(&label)));
            }
        }
        let mut readers: Vec<(String, String, String)> = Vec::new();
        for item in items {
            let Some(name) = boxed_name(item) else {
                continue;
            };
            let node = ids.get("n", &name);
            for (is_write, sig) in signals_of(item) {
                let s = ids.get("s", &sig.display());
                if is_write {
                    let _ = writeln!(
                        out,
                        "  {}",
                        sig_edge(&node, &s, sig, opts.show_cfg, dotted_signal_edges)
                    );
                } else {
                    readers.push((
                        s.clone(),
                        name.clone(),
                        sig_edge(&s, &node, sig, opts.show_cfg, dotted_signal_edges),
                    ));
                }
            }
        }
        let declared_boxes = emitted.clone();
        for item in items {
            let Some(name) = boxed_name(item) else {
                continue;
            };
            let funcs = crate::expand_bundles(dataflow_fn_sources(item), &opts.bundles);
            if funcs.is_empty() {
                continue;
            }
            let node = ids.get("n", &name);
            let mut drawn: Vec<(bool, String)> = Vec::new();
            for d in &opts.discovered {
                let a = &d.access;
                let matched = discovered_access_matches(&funcs, d, &graph_module);
                if !matched || drawn.iter().any(|(w, p)| *w == a.write && *p == a.path) {
                    continue;
                }
                drawn.push((a.write, a.path.clone()));
                let key = discovered_key(d, &emitted, &declared_boxes);
                if !emitted.contains(&key) {
                    emitted.push(key.clone());
                    let id = ids.get("s", &key);
                    let label = if opts.full_paths {
                        key.clone()
                    } else {
                        key.rsplit("::").next().unwrap_or(&key).to_string()
                    };
                    let _ = writeln!(out, "  {id}[/\"{}\"/]", esc(&label));
                }
                let s = ids.get("s", &key);
                let (from, to) = if a.write { (&node, &s) } else { (&s, &node) };
                let mut label = match a.verb.as_str() {
                    "open" => "gated",
                    "beat_put" | "beat_writer" => "beat",
                    "get" | "put" | "reader" | "writer" => "discovered",
                    other => other,
                }
                .to_string();
                if opts.show_cfg {
                    for c in &a.cfgs {
                        label = format!("{label} · <small>cfg({c})</small>");
                    }
                }
                let edge = signal_edge(from, to, &label, dotted_signal_edges);
                if a.write {
                    let _ = writeln!(out, "  {edge}");
                } else {
                    readers.push((s.clone(), name.clone(), edge));
                }
            }
        }
        let element_links: Vec<(String, String)> = emitted
            .iter()
            .filter_map(|key| {
                let (base, _) = key.split_once('[')?;
                let base = base.trim_end();
                let whole = emitted.iter().find(|o| {
                    !o.contains('[')
                        && (o.as_str() == base
                            || base.ends_with(&format!("::{o}"))
                            || o.ends_with(&format!("::{base}")))
                })?;
                Some((key.clone(), whole.clone()))
            })
            .collect();
        for (elem, whole) in element_links {
            let _ = writeln!(
                out,
                "  {} -. \"element of\" .- {}",
                ids.get("s", &elem),
                ids.get("s", &whole)
            );
        }

        let mut by_sig: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (sig, node, edge) in readers {
            by_sig.entry(sig).or_default().push((node, edge));
        }
        for (sig, rs) in by_sig {
            if opts.max_fanout > 0 && rs.len() > opts.max_fanout {
                let agg = format!("fan_{sig}");
                let names: Vec<&str> = rs.iter().map(|(n, _)| n.as_str()).collect();
                let _ = writeln!(
                    out,
                    "  {agg}[\"{} readers: {}\"]",
                    names.len(),
                    esc(&names.join(", "))
                );
                let _ = writeln!(
                    out,
                    "  {}",
                    signal_edge(&sig, &agg, "", dotted_signal_edges)
                );
            } else {
                for (_, edge) in rs {
                    let _ = writeln!(out, "  {edge}");
                }
            }
        }
        let sigs: Vec<String> = emitted.iter().map(|k| ids.get("s", k)).collect();
        if !sigs.is_empty() {
            let _ = writeln!(out, "  class {} signal;", sigs.join(","));
        }
    }

    {
        let mut slots: Vec<String> = Vec::new();
        let _ = writeln!(out);
        for item in items {
            for r in resources_of(item) {
                let name = r.ident.to_string();
                if slots.contains(&name) {
                    continue;
                }
                slots.push(name.clone());
                let id = ids.get("r", &name);
                let _ = writeln!(
                    out,
                    "  {id}@{{ shape: notch-rect, label: \"{}\" }}",
                    esc(&name)
                );
            }
        }
        for item in items {
            let Some(name) = boxed_name(item) else {
                continue;
            };
            let node = ids.get("n", &name);
            for r in resources_of(item) {
                let slot = ids.get("r", &r.ident.to_string());
                let _ = writeln!(out, "  {}", resource_edge(&slot, &node, r, opts.show_cfg));
            }
        }
        for item in items {
            let Item::Node(n) = item else {
                continue;
            };
            if n.provides.is_empty() {
                continue;
            }
            let Some(name) = boxed_name(item) else {
                continue;
            };
            let node = ids.get("n", &name);
            for slot in &n.provides {
                let name = slot.ident.to_string();
                if !slots.contains(&name) {
                    continue;
                }
                let id = ids.get("r", &name);
                let label = match cfg_note(&slot.cfg, opts.show_cfg) {
                    Some(c) => format!("provides · {c}"),
                    None => "provides".to_string(),
                };
                let _ = writeln!(out, "  {node} -- \"{}\" --> {id}", esc(&label));
            }
        }
        if !slots.is_empty() {
            let ids: Vec<String> = slots.iter().map(|n| ids.get("r", n)).collect();
            let _ = writeln!(out, "  class {} resource;", ids.join(","));
        }
    }

    if opts.legend {
        let _ = writeln!(out, "  end");
    }

    let mut classed: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for item in items {
        let Some(name) = boxed_name(item) else {
            continue;
        };
        let id = ids.get("n", &name);
        match item {
            Item::Node(n) if n.disabled.is_some() => {
                classed.entry("disabled").or_default().push(id)
            }
            Item::Node(n) if n.source.is_none() => classed.entry("parked").or_default().push(id),
            _ => {}
        }
    }
    for name in &externals {
        classed
            .entry("external")
            .or_default()
            .push(ids.get("n", name));
    }
    let _ = writeln!(out);
    for (class, members) in &classed {
        let _ = writeln!(out, "  class {} {class};", members.join(","));
    }
    out.push_str(CLASSDEFS);
    if opts.legend {
        let before = count_links(&out);
        out.push_str(&legend(opts, true, before));
    }
    out
}

const CLASSDEFS: &str = "  classDef parked stroke-dasharray:4 3;
  classDef disabled stroke-dasharray:2 4,opacity:0.5;
  classDef external stroke-dasharray:3 3,opacity:0.7;
  classDef signal stroke-width:1px,font-size:10px;
  classDef resource stroke-width:1px,stroke-dasharray:0,font-size:10px;
";

fn count_links(s: &str) -> usize {
    s.matches("-->").count() + s.matches("==>").count() + s.matches(".->").count()
}

const MAIN: &str = "__sv_graph";

fn legend(opts: &Options, chained: bool, links_before: usize) -> String {
    let rows: Vec<(&str, &str, &str)> = if opts.runtime {
        let mut r = vec![
            (
                "{a}[\"writer\"] --> {b}[/\"signal\"/] --> {c}[\"reader\"]",
                "b",
                "signal",
            ),
            (
                "{a}[\"writer\"] -- \"observed\" --> {b}[/\"polled by the supervisor\"/]",
                "b",
                "signal",
            ),
            (
                "{a}[\"writer\"] -- \"reported\" --> {b}[/\"reported by the task body\"/]",
                "b",
                "signal",
            ),
            (
                "{a}[\"node\"] -- \"discovered\" --> {b}[/\"seen in the body, not declared\"/]",
                "b",
                "signal",
            ),
            (
                "{a}[/\"signal\"/] -- \"gated\" --> {b}[\"reader that cannot start without it\"]",
                "a",
                "signal",
            ),
            (
                "{a}[\"writer\"] -- \"beat\" --> {b}[/\"and drives its heartbeat\"/]",
                "b",
                "signal",
            ),
            (
                "{a}[/\"signal\"/] -- \"lease\" --> {b}[\"holds its producer's stop\"]",
                "a",
                "signal",
            ),
            (
                "{a}[/\"ARR#lsqb;0#rsqb;\"/] -. \"element of\" .- {b}[/\"the whole array's coupling\"/]",
                "a",
                "signal",
            ),
            (
                "{a}@{ shape: notch-rect, label: \"resources: slot\" } --> {b}[\"the node that takes it\"]",
                "a",
                "resource",
            ),
            (
                "{a}[\"provider\"] -- \"provides\" --> {b}@{ shape: notch-rect, label: \"slot it fills\" }",
                "b",
                "resource",
            ),
        ];
        match runtime_dep_edges(opts) {
            RuntimeDepEdges::All => r.push((
                "{a}[\"node\"] -. \"spawn\" .-> {b}[\"bring-up order shown as context\"]",
                "",
                "",
            )),
            RuntimeDepEdges::Anchors => r.push((
                "{a}[\"node\"] -. \"spawn\" .-> {b}[\"pinned by bring-up order\"]",
                "",
                "",
            )),
            RuntimeDepEdges::None => {}
        }
        r
    } else {
        vec![
            ("{a}[\"dep\"] --> {b}[\"spawns after it\"]", "", ""),
            (
                "{a}[\"node\"] -- \"ready\" --> {b}[\"awaits set_ready\"]",
                "",
                "",
            ),
            (
                "{a}[\"node\"] == \"ready bound\" ==> {b}[\"readiness propagates\"]",
                "",
                "",
            ),
            (
                "{a}[\"writer\"] -.-> {b}[/\"signal\"/] -.-> {c}[\"reader\"]",
                "b",
                "signal",
            ),
            (
                "{a}[\"node\"] -. \"discovered\" .-> {b}[/\"signal found in the body\"/]",
                "b",
                "signal",
            ),
            (
                "{a}@{ shape: notch-rect, label: \"resources: slot\" } --> {b}[\"gates its spawn\"]",
                "a",
                "resource",
            ),
            (
                "{a}[\"provider\"] -- \"provides\" --> {b}@{ shape: notch-rect, label: \"slot it fills\" }",
                "b",
                "resource",
            ),
        ]
    };

    let mut out = String::from("\n  subgraph legend[\"legend\"]\n    direction LR\n");
    let mut ids: Vec<String> = Vec::new();
    let mut row_links = 0usize;
    for (i, (row, _, _)) in rows.iter().enumerate() {
        let (a, b, c) = (format!("lg{i}a"), format!("lg{i}b"), format!("lg{i}c"));
        let text = row.replace("{a}", &a).replace("{b}", &b).replace("{c}", &c);
        row_links += count_links(&text);
        let _ = writeln!(out, "    {text}");
        ids.push(a);
        ids.push(b);
        if row.contains("{c}") {
            ids.push(c);
        }
    }
    let _ = writeln!(out, "  end");
    if chained {
        let _ = writeln!(out, "  {MAIN} ~~~ legend");
        let _ = writeln!(out, "  style {MAIN} fill:none,stroke:none");
    }
    let _ = writeln!(out, "  class {} legendtext;", ids.join(","));
    let _ = writeln!(out, "  classDef legendtext font-size:11px;");
    let styled: Vec<String> = (links_before..links_before + row_links)
        .map(|i| i.to_string())
        .collect();
    if !styled.is_empty() {
        let _ = writeln!(out, "  linkStyle {} font-size:10px;", styled.join(","));
    }
    out
}

pub(crate) fn diagram_title(decl: &Decl, opts: &Options) -> String {
    opts.title.clone().unwrap_or_else(|| match decl.name() {
        Some(n) => format!("{} {n}", decl.kind.macro_name()),
        None => decl.kind.macro_name().to_string(),
    })
}

pub(crate) fn frontmatter(title: &str, opts: &Options) -> String {
    if !opts.show_title && opts.layout.is_none() {
        return String::new();
    }
    let mut out = String::from("---\n");
    if opts.show_title {
        if opts.title.is_some() {
            let title = title.replace('\\', "\\\\").replace('"', "\\\"");
            let _ = writeln!(out, "title: \"{title}\"");
        } else {
            let _ = writeln!(out, "title: {title}");
        }
    }
    if let Some(layout) = &opts.layout {
        let _ = writeln!(out, "config:\n  layout: {layout}");
    }
    out.push_str("---\n");
    out
}

pub(crate) fn spacing_init(opts: &Options) -> String {
    let sideways = matches!(opts.direction.as_str(), "LR" | "RL");
    let (node, rank) = if sideways {
        (opts.v_spacing, opts.h_spacing)
    } else {
        (opts.h_spacing, opts.v_spacing)
    };
    let node = node.or(if opts.legend { Some(12) } else { None });
    let mut set = Vec::new();
    if let Some(n) = node {
        set.push(format!("\"nodeSpacing\": {n}"));
    }
    if let Some(r) = rank {
        set.push(format!("\"rankSpacing\": {r}"));
    }
    if set.is_empty() {
        return String::new();
    }
    format!("%%{{init: {{\"flowchart\": {{{}}}}} }}%%\n", set.join(", "))
}

const STATES_SMALL: &str = "%%{init: {\"themeVariables\": {\"fontSize\": \"11px\"}} }%%\n";

/// Render a standalone legend diagram for the given options.
pub fn legend_diagram(opts: &Options) -> String {
    if opts.states {
        return format!(
            "{STATES_SMALL}%% legend\nstateDiagram-v2\n  direction {}\n{}",
            match opts.direction.as_str() {
                "TD" => "TB",
                other => other,
            },
            crate::states::legend(match opts.direction.as_str() {
                "TD" => "TB",
                other => other,
            })
        );
    }
    format!(
        "{}%% legend\nflowchart LR\n{}{CLASSDEFS}",
        spacing_init(&Options {
            legend: true,
            ..opts.clone()
        }),
        legend(opts, false, 0)
    )
}

pub(crate) fn boxed_name(item: &Item) -> Option<String> {
    match item {
        Item::Node(n) => Some(n.ident.to_string()),
        Item::Pool(p) => Some(p.ident.to_string()),
        Item::Executor(_) => None,
    }
}

fn fragment_of(item: &Item) -> Option<&str> {
    match item {
        Item::Node(n) => n.fragment.as_deref(),
        Item::Pool(p) => p.fragment.as_deref(),
        Item::Executor(_) => None,
    }
}

fn link_origin<'a>(decl: &'a Decl, item: &Item) -> &'a str {
    fragment_of(item)
        .and_then(|name| decl.fragment_origins.get(name).map(String::as_str))
        .unwrap_or(&decl.origin)
}

fn deps_of(item: &Item) -> &[Dep] {
    match item {
        Item::Node(n) => &n.deps,
        Item::Pool(p) => &p.deps,
        Item::Executor(_) => &[],
    }
}

/// The `resources:` slots an item takes. An `executor` slot declares none.
fn resources_of(item: &Item) -> &[ResourceDecl] {
    match item {
        Item::Node(n) => &n.resources,
        Item::Pool(p) => &p.resources,
        Item::Executor(_) => &[],
    }
}

/// `(is_write, entry)` for every declared coupling entry.
/// Do two path spellings plausibly name one signal? Exact, or one is a
/// segment-suffix of the other (`PERIOD_MS` written at a call site,
/// `crate::heartbeat::PERIOD_MS` declared elsewhere). Textual and therefore
/// best-effort: identity at runtime is the address, which source cannot see.
/// The signal box a discovered access lands in. An alias-resolved path merges
/// with a declared spelling by segment suffix. A relative one tries its
/// module-qualified spelling first (so `params::TABLE` in `controller_rate.rs`
/// finds `…::controller_rate::params::TABLE` and never a sibling module's);
pub(crate) fn discovered_key(
    d: &crate::Discovered,
    emitted: &[String],
    declared: &[String],
) -> String {
    let raw = &d.access.path;
    if !d.relative {
        return emitted
            .iter()
            .find(|k| paths_alias(k, raw))
            .cloned()
            .unwrap_or_else(|| raw.clone());
    }
    let qualified = format!("{}::{raw}", d.module);
    if let Some(k) = emitted.iter().find(|k| paths_alias(k, &qualified)) {
        return k.clone();
    }
    let mut hits = declared.iter().filter(|k| paths_alias(k, raw));
    match (hits.next(), hits.next()) {
        (Some(k), None) => k.clone(),
        _ => qualified,
    }
}

fn paths_alias(a: &str, b: &str) -> bool {
    let a = a.strip_prefix("crate::").unwrap_or(a);
    let b = b.strip_prefix("crate::").unwrap_or(b);
    a == b || a.ends_with(&format!("::{b}")) || b.ends_with(&format!("::{a}"))
}

pub(crate) fn dataflow_fn_sources(item: &Item) -> Vec<(String, Option<String>)> {
    fn split(segments: Vec<String>) -> Option<(String, Option<String>)> {
        let name = segments.last()?.clone();
        let hint = if segments.len() >= 2 {
            let m = &segments[segments.len() - 2];
            (!matches!(m.as_str(), "crate" | "self" | "super")).then(|| m.clone())
        } else {
            None
        };
        Some((name, hint))
    }
    let mut out: Vec<(String, Option<String>)> =
        task_fn_segments(item).and_then(split).into_iter().collect();
    let adopted = match item {
        Item::Node(n) => &n.dataflow,
        Item::Pool(p) => &p.dataflow,
        Item::Executor(_) => return out,
    };
    out.extend(adopted.iter().filter_map(|f| {
        split(
            f.path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect(),
        )
    }));
    out
}

fn has_discovered_access(
    item: &Item,
    graph_module: &str,
    discovered: &[crate::Discovered],
    bundles: &[crate::Bundle],
) -> bool {
    let funcs = crate::expand_bundles(dataflow_fn_sources(item), bundles);
    discovered
        .iter()
        .any(|access| discovered_access_matches(&funcs, access, graph_module))
}

pub(crate) fn discovered_access_matches(
    funcs: &[(String, Option<String>)],
    discovered: &crate::Discovered,
    graph_module: &str,
) -> bool {
    let access = &discovered.access;
    funcs.iter().any(|(fn_name, hint)| {
        fn_name == &access.func
            && hint
                .as_deref()
                .is_none_or(|hint| hint == discovered.module || discovered.module == graph_module)
    })
}

fn task_fn_segments(item: &Item) -> Option<Vec<String>> {
    fn segments(e: &syn::Expr) -> Option<Vec<String>> {
        match e {
            syn::Expr::Path(p) => Some(
                p.path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect(),
            ),
            syn::Expr::Call(c) => segments(&c.func),
            _ => None,
        }
    }
    let source = match item {
        Item::Node(n) => n.source.as_ref()?,
        Item::Pool(p) => &p.source,
        Item::Executor(_) => return None,
    };
    let (TaskSource::Shell(e) | TaskSource::Spawn(e)) = source;
    segments(e)
}

pub(crate) fn signals_of(item: &Item) -> Vec<(bool, &SignalDecl)> {
    let (reads, writes) = match item {
        Item::Node(n) => (&n.reads, &n.writes),
        Item::Pool(p) => (&p.reads, &p.writes),
        Item::Executor(_) => return Vec::new(),
    };
    writes
        .iter()
        .map(|s| (true, s))
        .chain(reads.iter().map(|s| (false, s)))
        .collect()
}

fn shape(id: &str, item: &Item, opts: &Options) -> String {
    match item {
        Item::Node(n) => format!("{id}[\"{}\"]", esc(&node_label(n, opts.show_cfg))),
        Item::Pool(p) => format!("{id}[[\"{}\"]]", esc(&pool_label(p, opts.show_cfg))),
        Item::Executor(_) => String::new(),
    }
}

fn node_label(n: &NodeItem, show_cfg: bool) -> String {
    let mut facts = vec![n.mode.to_string(), body_kind(n.source.as_ref())];
    if let Some(size) = &n.pool_size {
        facts.push(format!("×{size}"));
    }
    if let Some(e) = &n.executor {
        facts.push(format!("@{e}"));
    }
    if let Some(bt) = &n.beat_timeout {
        let ms = &bt.value;
        facts.push(gated_fact(format!("beat {ms}"), &bt.cfg, show_cfg));
    }
    if let Some(r) = &n.ready_on_write {
        facts.push(gated_fact("ready_on_write".to_string(), &r.cfg, show_cfg));
    }
    if let Some(d) = &n.disabled
        && let Some(c) = cfg_note(&d.cfg, show_cfg)
    {
        facts.push(format!("disabled {c}"));
    }
    if n.cancel {
        facts.push("cancel".to_string());
    }
    if let Some(c) = cfg_note(&n.cfg, show_cfg) {
        facts.push(c);
    }
    format!("{}<br/>{}", n.ident, facts.join(" · "))
}

fn pool_label(p: &PoolItem, show_cfg: bool) -> String {
    let mut facts = vec![
        format!("pool ×{}", p.modes.len()),
        body_kind(Some(&p.source)),
    ];
    facts.push(format!("{}..{}", tail(&p.min), tail(&p.max)));
    if let Some(e) = &p.executor {
        facts.push(format!("@{e}"));
    }
    if p.cancel {
        facts.push("cancel".to_string());
    }
    if let Some(c) = cfg_note(&p.cfg, show_cfg) {
        facts.push(c);
    }
    format!("{}<br/>{}", p.ident, facts.join(" · "))
}

/// A label fact with its clause gate appended — `beat 100 <small>cfg(..)</small>`.
fn gated_fact(fact: String, cfg: &[syn::Attribute], show_cfg: bool) -> String {
    match cfg_note(cfg, show_cfg) {
        Some(c) => format!("{fact} {c}"),
        None => fact,
    }
}

fn cfg_note(attrs: &[syn::Attribute], show: bool) -> Option<String> {
    if !show {
        return None;
    }
    let preds: Vec<String> = attrs
        .iter()
        .filter_map(|a| match &a.meta {
            syn::Meta::List(l) if l.path.is_ident("cfg") => Some(l.tokens.to_string()),
            _ => None,
        })
        .collect();
    match preds.len() {
        0 => None,
        _ => Some(format!("<small>cfg({})</small>", preds.join(") · cfg("))),
    }
}

fn executor_of(item: &Item) -> Option<String> {
    match item {
        Item::Node(n) => n.executor.as_ref().map(|e| e.to_string()),
        Item::Pool(p) => p.executor.as_ref().map(|e| e.to_string()),
        Item::Executor(_) => None,
    }
}

fn item_line(item: &Item) -> usize {
    match item {
        Item::Node(n) => n.ident.span().start().line,
        Item::Pool(p) => p.ident.span().start().line,
        Item::Executor(x) => x.ident.span().start().line,
    }
}

fn body_kind(source: Option<&TaskSource>) -> String {
    match source {
        Some(TaskSource::Shell(_)) => "task",
        Some(TaskSource::Spawn(_)) => "spawn",
        None => "parked",
    }
    .to_string()
}

fn tail(e: &syn::Expr) -> String {
    let text = e.to_token_stream().to_string().replace(' ', "");
    text.rsplit("::").next().unwrap_or(&text).to_string()
}

fn signal_labels(items: &[Item], full: bool) -> BTreeMap<String, String> {
    let mut keys: Vec<(String, Vec<String>, String)> = Vec::new();
    for item in items {
        for (_, sig) in signals_of(item) {
            let key = sig.display();
            if keys.iter().any(|(k, _, _)| *k == key) {
                continue;
            }
            let path = path_to_string(&sig.path);
            let segs = path
                .split("::")
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let index = key.strip_prefix(path.as_str()).unwrap_or("").to_string();
            keys.push((key, segs, index));
        }
    }
    let mut out = BTreeMap::new();
    if full {
        for (k, _, _) in &keys {
            out.insert(k.clone(), k.clone());
        }
        return out;
    }
    let longest = keys.iter().map(|(_, s, _)| s.len()).max().unwrap_or(1);
    let mut pending: Vec<usize> = (0..keys.len()).collect();
    for n in 1..=longest {
        let candidates: Vec<String> = keys
            .iter()
            .map(|(_, segs, index)| {
                let from = segs.len().saturating_sub(n);
                format!("{}{index}", segs[from..].join("::"))
            })
            .collect();
        pending.retain(|&i| {
            if candidates.iter().filter(|c| **c == candidates[i]).count() > 1 {
                return true;
            }
            out.insert(keys[i].0.clone(), candidates[i].clone());
            false
        });
        if pending.is_empty() {
            break;
        }
    }
    for &i in &pending {
        out.insert(keys[i].0.clone(), keys[i].0.clone());
    }
    out
}

fn unresolved(label: &str) -> String {
    label.replace(UNRESOLVED_CRATE, "$crate")
}

fn markers(dep: &Dep) -> String {
    let mut m = Vec::new();
    if dep.ready.is_some() {
        m.push("ready");
    }
    if dep.bound.is_some() {
        m.push("bound");
    }
    m.join(" ")
}

fn dep_edge(from: &str, to: &str, dep: &Dep, label: &str) -> String {
    let q = format!("\"{}\"", esc(label));
    if dep.bound.is_some() {
        return format!("{from} == {q} ==> {to}");
    }
    if label.is_empty() {
        format!("{from} --> {to}")
    } else {
        format!("{from} -- {q} --> {to}")
    }
}

fn resource_edge(slot: &str, node: &str, r: &ResourceDecl, show_cfg: bool) -> String {
    let mut marks: Vec<String> = Vec::new();
    if r.local.is_some() {
        marks.push("local".to_string());
    }
    if r.consume.is_some() {
        marks.push("consume".to_string());
    }
    if r.shared.is_some() {
        marks.push("shared".to_string());
    }
    if r.serialized.is_some() {
        marks.push("serialized".to_string());
    }
    if r.divisible.is_some() {
        marks.push("divisible".to_string());
    }
    if let Some(c) = cfg_note(&r.cfg, show_cfg) {
        marks.push(c);
    }
    if marks.is_empty() {
        format!("{slot} --> {node}")
    } else {
        format!("{slot} -- \"{}\" --> {node}", marks.join(" · "))
    }
}

fn sig_edge(from: &str, to: &str, sig: &SignalDecl, show_cfg: bool, dotted: bool) -> String {
    let mut marks: Vec<String> = Vec::new();
    if sig.observed.is_some() {
        marks.push("observed".to_string());
    }
    if sig.beat.is_some() {
        marks.push("beat".to_string());
    }
    if sig.veto.is_some() {
        marks.push("veto".to_string());
    }
    if let Some(c) = cfg_note(&sig.cfg, show_cfg) {
        marks.push(c);
    }
    signal_edge(from, to, &marks.join(" · "), dotted)
}

fn signal_edge(from: &str, to: &str, label: &str, dotted: bool) -> String {
    if label.is_empty() {
        if dotted {
            format!("{from} -.-> {to}")
        } else {
            format!("{from} --> {to}")
        }
    } else if dotted {
        format!("{from} -. \"{}\" .-> {to}", esc(label))
    } else {
        format!("{from} -- \"{}\" --> {to}", esc(label))
    }
}

fn esc(s: &str) -> String {
    s.replace('"', "#quot;")
}
