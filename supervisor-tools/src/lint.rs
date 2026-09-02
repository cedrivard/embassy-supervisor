//! Dataflow linting: finding one-sided signal couplings.

use crate::render::{boxed_name, discovered_access_matches, discovered_key, signals_of};
use crate::{Bundle, Decl, Discovered, GateStatic, expand_bundles, module_of};
use std::collections::BTreeMap;

/// Enabled lint categories for a `supervisor-lint` run.
#[derive(Default, Clone)]
pub struct LintCats {
    /// Report signals that are read but never written.
    pub orphan_reads: bool,
    /// Report signals that are written but never read.
    pub dead_writes: bool,
    /// Report gate-wrapped statics (`Backed`, `Leased`, `VetoGate`) that are
    /// reachable from outside their module.
    pub public_gate: bool,
}

impl LintCats {
    /// Return `true` if any category is enabled.
    pub fn any(&self) -> bool {
        self.orphan_reads || self.dead_writes || self.public_gate
    }

    /// Enable every lint category.
    pub fn all() -> Self {
        Self {
            orphan_reads: true,
            dead_writes: true,
            public_gate: true,
        }
    }

    /// Parse a comma-separated lint category list such as `orphan-reads,dead-writes`.
    pub fn parse(&mut self, arg: &str) -> Result<(), String> {
        for cat in arg.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            match cat {
                "orphan-reads" => self.orphan_reads = true,
                "dead-writes" => self.dead_writes = true,
                "public-gate" => self.public_gate = true,
                "all" => *self = Self::all(),
                other => {
                    return Err(format!(
                        "unknown lint category `{other}`; the categories are \
                         orphan-reads, dead-writes, public-gate, all"
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Sides {
    writers: Vec<String>,
    readers: Vec<String>,
    monitor_consumed: bool,
}

/// Run enabled dataflow lints over the given declarations and discoveries.
///
/// `allow` contains signal path patterns that suppress matching warnings.
/// Returns human-readable warning strings.
pub fn dataflow_lints(
    decls: &[Decl],
    discovered: &[Discovered],
    bundles: &[Bundle],
    cats: &LintCats,
    allow: &[String],
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut allow_hit = vec![false; allow.len()];
    for decl in decls {
        let graph_module = module_of(&decl.origin);
        let mut order: Vec<String> = Vec::new();
        let mut sides: BTreeMap<String, Sides> = BTreeMap::new();
        let note = |order: &mut Vec<String>,
                    sides: &mut BTreeMap<String, Sides>,
                    key: &str,
                    node: &str,
                    write: bool,
                    monitor: bool| {
            if !sides.contains_key(key) {
                order.push(key.to_string());
            }
            let s = sides.entry(key.to_string()).or_default();
            let list = if write {
                &mut s.writers
            } else {
                &mut s.readers
            };
            if !list.contains(&node.to_string()) {
                list.push(node.to_string());
            }
            s.monitor_consumed |= monitor;
        };
        let mut declared_boxes: Vec<String> = Vec::new();
        for item in &decl.spec.items {
            let Some(name) = boxed_name(item) else {
                continue;
            };
            for (is_write, sig) in signals_of(item) {
                let key = sig.display();
                if !declared_boxes.contains(&key) {
                    declared_boxes.push(key.clone());
                }
                let monitor = is_write && (sig.observed.is_some() || sig.beat.is_some());
                note(&mut order, &mut sides, &key, &name, is_write, monitor);
            }
        }
        let mut emitted = declared_boxes.clone();
        for item in &decl.spec.items {
            let Some(name) = boxed_name(item) else {
                continue;
            };
            let funcs = expand_bundles(crate::render::dataflow_fn_sources(item), bundles);
            if funcs.is_empty() {
                continue;
            }
            let mut seen: Vec<(bool, String)> = Vec::new();
            for d in discovered {
                let a = &d.access;
                if !discovered_access_matches(&funcs, d, &graph_module)
                    || seen.iter().any(|(w, p)| *w == a.write && *p == a.path)
                {
                    continue;
                }
                seen.push((a.write, a.path.clone()));
                let key = discovered_key(d, &emitted, &declared_boxes);
                if !emitted.contains(&key) {
                    emitted.push(key.clone());
                }
                let monitor = a.write && a.verb.starts_with("beat");
                note(&mut order, &mut sides, &key, &name, a.write, monitor);
            }
        }
        let element_links: Vec<(String, String)> = order
            .iter()
            .filter_map(|key| {
                let (base, _) = key.split_once('[')?;
                let base = base.trim_end();
                let whole = order.iter().find(|o| {
                    !o.contains('[')
                        && (o.as_str() == base
                            || base.ends_with(&format!("::{o}"))
                            || o.ends_with(&format!("::{base}")))
                })?;
                Some((key.clone(), whole.clone()))
            })
            .collect();
        for (elem, whole) in &element_links {
            let merge = |from: &str, to: &str, sides: &mut BTreeMap<String, Sides>| {
                let src = &sides[from];
                let (w, r) = (src.writers.clone(), src.readers.clone());
                let dst = sides.get_mut(to).expect("both keys are in `order`");
                for n in w {
                    if !dst.writers.contains(&n) {
                        dst.writers.push(n);
                    }
                }
                for n in r {
                    if !dst.readers.contains(&n) {
                        dst.readers.push(n);
                    }
                }
            };
            merge(elem, whole, &mut sides);
            merge(whole, elem, &mut sides);
        }
        for key in &order {
            let s = &sides[key];
            let orphan = cats.orphan_reads && s.writers.is_empty() && !s.readers.is_empty();
            let dead = cats.dead_writes
                && s.readers.is_empty()
                && !s.writers.is_empty()
                && !s.monitor_consumed;
            if !(orphan || dead) {
                continue;
            }
            if let Some(i) = allow.iter().position(|a| allow_matches(key, a)) {
                allow_hit[i] = true;
                continue;
            }
            if orphan {
                warnings.push(format!(
                    "orphan read: `{key}` is read by {} but nothing in this graph writes it",
                    s.readers.join(", ")
                ));
            }
            if dead {
                warnings.push(format!(
                    "dead write: `{key}` is written by {} but nothing in this graph reads it",
                    s.writers.join(", ")
                ));
            }
        }
    }
    for (i, a) in allow.iter().enumerate() {
        if !allow_hit[i] {
            warnings.push(format!(
                "`--allow {a}` suppressed nothing — a stale entry, or a typo'd signal"
            ));
        }
    }
    warnings
}

/// A gate guards the access path, not the static. A gate-wrapped static that
/// is reachable from outside its module can be bypassed; keep it private and
/// expose only `#[dataflow]` accessors.
pub fn gate_lints(statics: &[GateStatic], cats: &LintCats) -> Vec<String> {
    if !cats.public_gate {
        return Vec::new();
    }
    statics
        .iter()
        .filter(|g| g.public)
        .map(|g| {
            format!(
                "public gate: `{}:{}` `{}: {}<..>` is reachable from outside its module; \
                 reaching the static directly bypasses the gate — keep it private and \
                 expose `#[dataflow]` accessors",
                g.file, g.line, g.name, g.ty
            )
        })
        .collect()
}

fn allow_matches(key: &str, allowed: &str) -> bool {
    let k = key.strip_prefix("crate::").unwrap_or(key);
    let a = allowed.strip_prefix("crate::").unwrap_or(allowed);
    k == a || k.ends_with(&format!("::{a}")) || a.ends_with(&format!("::{k}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_source, scan_source};

    fn all() -> LintCats {
        LintCats::all()
    }

    #[test]
    fn declared_sides_lint_one_sided_signals() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node W = Terminate, task: w_entry, writes: [crate::sig::OUT, crate::sig::TAP];
                node R = Terminate, task: r_entry, reads: [crate::sig::OUT, crate::sig::MISSING];
            }
        "#;
        let decls = parse_source(src, "t.rs").unwrap();
        let w = dataflow_lints(&decls, &[], &[], &all(), &[]);
        assert!(
            w.iter()
                .any(|w| w.contains("orphan read") && w.contains("MISSING") && w.contains("R")),
            "{w:?}"
        );
        assert!(
            w.iter()
                .any(|w| w.contains("dead write") && w.contains("TAP") && w.contains("W")),
            "{w:?}"
        );
    }

    #[test]
    fn observed_and_allow_suppress_their_findings() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node W = Terminate, task: w_entry,
                    writes: [crate::sig::OUT, crate::sig::BEAT observed beat];
                node R = Terminate, task: r_entry,
                    reads: [crate::sig::OUT, crate::sig::MISSING];
            }
        "#;
        let decls = parse_source(src, "t.rs").unwrap();
        let w = dataflow_lints(&decls, &[], &[], &all(), &[]);
        assert!(!w.iter().any(|w| w.contains("BEAT")), "{w:?}");
        let w = dataflow_lints(&decls, &[], &[], &all(), &["MISSING".into(), "GONE".into()]);
        assert!(!w.iter().any(|w| w.contains("orphan read")), "{w:?}");
        assert!(
            w.iter()
                .any(|w| w.contains("--allow GONE") && w.contains("suppressed nothing")),
            "{w:?}"
        );
        let w = dataflow_lints(&decls, &[], &[], &all(), &["OUT".into(), "MISSING".into()]);
        assert!(
            w.iter()
                .any(|w| w.contains("--allow OUT") && w.contains("suppressed nothing")),
            "{w:?}"
        );
    }

    #[test]
    fn element_writes_and_whole_array_reads_cover_each_other() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node W = Terminate, task: w_entry, writes: [crate::sig::ARR[0]];
                node R = Terminate, deps: [W], task: r_entry, reads: [crate::sig::ARR];
            }
        "#;
        let decls = parse_source(src, "t.rs").unwrap();
        let w = dataflow_lints(&decls, &[], &[], &all(), &[]);
        assert!(
            !w.iter()
                .any(|w| w.contains("orphan read") || w.contains("dead write")),
            "{w:?}"
        );
    }

    #[test]
    fn public_gates_are_reported_and_private_ones_are_not() {
        let src = r#"
            use embassy_supervisor::{Backed, Leased, VetoGate};
            pub static EST: Backed<u32> = Backed::new(0);
            pub(crate) static HANDLE: Leased<u32> = Leased::new(0);
            mod inner {
                static TRIP: VetoGate<4> = VetoGate::new();
                pub static LOUD: VetoGate<4> = VetoGate::new();
                pub static PLAIN: u32 = 0;
            }
            static QUIET: Backed<u32> = Backed::new(0);
        "#;
        let mut found = Vec::new();
        crate::scan_gate_statics(src, "sig.rs", &mut found);
        let names: Vec<&str> = found.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            ["EST", "HANDLE", "TRIP", "LOUD", "QUIET"],
            "a plain static is not a gate"
        );
        let w = gate_lints(&found, &all());
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].contains("`sig.rs:3` `EST: Backed<..>`"), "{}", w[0]);
        assert!(
            w[1].contains("HANDLE: Leased<..>"),
            "pub(crate) is reachable too: {}",
            w[1]
        );
        assert!(w[2].contains("LOUD: VetoGate<..>"), "{}", w[2]);
        assert!(w.iter().all(|w| w.contains("#[dataflow]")), "{w:?}");
        let mut narrowed = LintCats::default();
        narrowed.parse("orphan-reads").unwrap();
        assert!(gate_lints(&found, &narrowed).is_empty(), "off unless asked");
        let mut only = LintCats::default();
        only.parse("public-gate").unwrap();
        assert!(only.public_gate && !only.dead_writes);
    }

    #[test]
    fn discovered_accesses_lint_like_declared_ones() {
        let src = r#"
            embassy_supervisor::supervisor_graph! {
                node A = Terminate, task: tasks::entry, discover;
            }
        "#;
        let body = r#"
            #[embassy_supervisor::dataflow]
            async fn entry(node: &'static TaskNode) {
                node.put(&OUT, 1);
                let _ = node.get(&NEVER_WRITTEN);
            }
        "#;
        let decls = parse_source(src, "t.rs").unwrap();
        let mut discovered = Vec::new();
        scan_source(body, "tasks.rs", &mut discovered);
        let w = dataflow_lints(&decls, &discovered, &[], &all(), &[]);
        assert!(
            w.iter()
                .any(|w| w.contains("orphan read") && w.contains("NEVER_WRITTEN")),
            "{w:?}"
        );
        assert!(
            w.iter()
                .any(|w| w.contains("dead write") && w.contains("OUT")),
            "{w:?}"
        );
    }
}
