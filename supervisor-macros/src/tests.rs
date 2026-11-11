use super::*;

/// Every feature on, whatever this test binary was built with.
const ALL: Features = Features::all();

/// Every feature off.
const NONE: Features = Features::none();

fn parse_gated(src: &str, f: &Features) -> SynResult<GraphSpec> {
    let spec = syn::parse_str::<GraphSpec>(src)?;
    // The gate strips what it defers, and `expand` gates again: hand back
    // the spec as written.
    gate::gate(&mut spec.clone(), f)?;
    Ok(spec)
}

/// `src` passes the gate with every feature on, and fails it naming the
/// feature under `off`.
fn gate_rejects(src: &str, off: Features, needle: &str) {
    if let Err(err) = parse_gated(src, &ALL) {
        panic!("rejected with every feature on: {err}");
    }
    match parse_gated(src, &off) {
        Ok(_) => panic!("accepted without the feature: {src}"),
        Err(err) => assert!(err.to_string().contains(needle), "unexpected error: {err}"),
    }
}

#[test]
fn ready_marker_requires_feature() {
    gate_rejects(
        "node NET = Terminate, deps: [];\n\
         node HTTP = Terminate, deps: [NET ready];",
        Features {
            readiness: false,
            ..ALL
        },
        "requires the `readiness` feature",
    );
}

#[test]
fn beat_timeout_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], beat_timeout: 100;",
        Features {
            liveness_monitor: false,
            ..ALL
        },
        "requires the `liveness-monitor` feature",
    );
}

#[test]
fn cfg_gated_clauses_defer_to_rustc() {
    for (src, off, builder) in [
        (
            "node A = Terminate, deps: [], \
             #[cfg(feature = \"x\")] beat_timeout: 100, \
             #[cfg(feature = \"x\")] beat_window: 3;",
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "with_beat_timeout",
        ),
        (
            "node A = Terminate, deps: [], task: w, \
             writes: [crate::S observed beat via it.get()], \
             #[cfg(feature = \"x\")] beat_timeout: 100, \
             #[cfg(feature = \"x\")] ready_on_write;",
            Features {
                readiness: false,
                ..ALL
            },
            "with_ready_on_write",
        ),
        (
            "node A = Terminate, deps: [], #[cfg(feature = \"x\")] enabled_if: crate::on;",
            Features {
                enabled_if: false,
                ..ALL
            },
            "with_enabled_if",
        ),
        (
            "node A = Terminate, deps: [], \
             #[cfg(feature = \"x\")] restart_budget: 3 in 1000;",
            Features {
                restart_intensity: false,
                ..ALL
            },
            "with_restart_budget (3u16",
        ),
        (
            "node A = Terminate, deps: [], task: w, #[cfg(feature = \"x\")] discover;",
            Features {
                dataflow: false,
                ..ALL
            },
            "__SV_DATAFLOW_",
        ),
    ] {
        let spec = parse_gated(src, &ALL).expect("a gated clause passes the gate");
        let out = expand(spec, &ALL).expect("expansion succeeds").to_string();
        assert!(out.contains(builder), "{src}\n{out}");
        assert!(!out.contains("compile_error"), "{src}\n{out}");
        let spec = parse_gated(src, &off).expect("a gated clause passes the gate");
        let out = expand(spec, &off).expect("expansion succeeds").to_string();
        assert!(out.contains("compile_error"), "{src}\n{out}");
        assert!(!out.contains(builder), "{src}\n{out}");
    }
}

#[test]
fn cfg_gated_featureless_clauses_emit_both_ways() {
    let spec = parse_gated(
        "node A = Terminate, deps: [], \
         #[cfg(feature = \"x\")] slot_timeout: 100, \
         #[cfg(feature = \"x\")] ack_timeout: 200, \
         #[cfg(feature = \"x\")] disabled;\n\
         node B = Terminate, deps: [], slot_timeout: 300;",
        &NONE,
    )
    .expect("gated featureless clauses pass the gate");
    let out = expand(spec, &NONE).expect("expansion succeeds").to_string();
    assert!(out.contains("with_slot_timeout"), "{out}");
    assert!(out.contains("with_ack_timeout"), "{out}");
    // The gated `disabled` becomes a cfg-block bool, not a bare literal.
    assert!(out.contains("# [cfg (feature = \"x\")] { true }"), "{out}");
    assert!(!out.contains("compile_error"), "{out}");

    // Pool timeouts ride the same emitter.
    let pool = Features { pool: true, ..NONE };
    let spec = parse_gated(
        "pool P = [Terminate], deps: [], task: w, \
         policy: embassy_supervisor::DeferredShrink::new(d()), \
         min: 1, max: 1, #[cfg(feature = \"x\")] slot_timeout: 100;",
        &pool,
    )
    .expect("gated pool timeout passes the gate");
    let out = expand(spec, &pool).expect("expansion succeeds").to_string();
    assert!(out.contains("with_slot_timeout"), "{out}");
    assert!(!out.contains("compile_error"), "{out}");
}

#[test]
fn nodes_pair_a_flash_cfg_with_a_zeroed_handle() {
    let pool = Features { pool: true, ..NONE };
    let spec = parse_gated(
        "node A = Terminate, deps: [], disabled;\n\
         node B = Terminate, deps: [];\n\
         pool P = [Terminate, Terminate], deps: [], task: w, \
         policy: embassy_supervisor::DeferredShrink::new(d()), min: 1, max: 2;",
        &pool,
    )
    .expect("the graph passes the gate");
    let out = expand(spec, &pool).expect("expansion succeeds").to_string();
    // One zeroed handle per node, paired with the node's config.
    assert!(
        out.contains(
            "static __SV_HANDLE_0_A : :: embassy_supervisor :: TaskHandle = \
             :: embassy_supervisor :: TaskHandle :: new () ;"
        ),
        "{out}"
    );
    assert!(
        out.contains("TaskNode :: new (& __SV_CFG_0_A , & __SV_HANDLE_0_A)"),
        "{out}"
    );
    assert!(
        out.contains("TaskNode :: new (& __SV_CFG_1_B , & __SV_HANDLE_1_B)"),
        "{out}"
    );
    // `disabled` is the config's: only `A` declares it.
    assert_eq!(out.matches("with_disabled_at_boot").count(), 1, "{out}");
    assert!(out.contains(". with_disabled_at_boot (true)"), "{out}");
    // Pool members: one handle array, indexed like the config array.
    assert!(
        out.contains("[const { :: embassy_supervisor :: TaskHandle :: new () } ; 2usize]"),
        "{out}"
    );
    assert!(
        out.contains("TaskNode :: new (& __SV_CFG_2_P [1usize] , & __SV_HANDLE_2_P [1usize])"),
        "{out}"
    );
}

#[test]
fn a_pool_without_take_kinds_shares_one_gate_table() {
    let pool = Features { pool: true, ..NONE };
    let expand_with = |resources: &str| {
        let src = format!(
            "pool P = [Terminate, Terminate], deps: [], task: w, resources: [{resources}], \
             policy: embassy_supervisor::DeferredShrink::new(d()), min: 1, max: 2;"
        );
        let spec = parse_gated(&src, &pool).expect("the graph passes the gate");
        expand(spec, &pool).expect("expansion succeeds").to_string()
    };
    // Only a pool-wide `shared` static: one table, named by both members.
    let out = expand_with("S: shared u32");
    assert!(out.contains("static __SV_GATES_0_P :"), "{out}");
    assert_eq!(
        out.matches("with_resources (& __SV_GATES_0_P)").count(),
        2,
        "{out}"
    );
    assert!(!out.contains("__SV_GATES_0_P_"), "{out}");
    // A take-kind entry gives each member its own element: one table each.
    let out = expand_with("S: shared u32, B: u32");
    assert!(out.contains("static __SV_GATES_0_P_0 :"), "{out}");
    assert!(out.contains("static __SV_GATES_0_P_1 :"), "{out}");
    assert!(!out.contains("static __SV_GATES_0_P :"), "{out}");
}

#[test]
fn self_handle_follows_trace_self() {
    let on = Features {
        graph_ref: true,
        trace: true,
        trace_self: true,
        ..NONE
    };
    let expand_with = |src: &str, f: &Features| {
        let spec = parse_gated(src, f).expect("the graph passes the gate");
        expand(spec, f).expect("expansion succeeds").to_string()
    };
    let src = "node A = Terminate, deps: [];";
    let out = expand_with(src, &on);
    assert!(
        out.contains(
            "static __SV_SELF_HANDLE : :: embassy_supervisor :: TaskHandle = \
             :: embassy_supervisor :: TaskHandle :: new () ;"
        ),
        "{out}"
    );
    assert!(
        out.contains("GraphRef :: new (& __SV_NODES) . with_self_handle (& __SV_SELF_HANDLE) ;"),
        "{out}"
    );
    // A named graph's handle carries the graph's name, like its `GraphRef`.
    let out = expand_with("name: SUB;\nnode A = Terminate, deps: [];", &on);
    assert!(
        out.contains(
            "GraphRef :: new (& __SV_NODES_SUB) . with_self_handle (& __SV_SELF_HANDLE_SUB) ;"
        ),
        "{out}"
    );
    let off = Features {
        trace_self: false,
        ..on
    };
    let out = expand_with(src, &off);
    assert!(out.contains("GraphRef :: new (& __SV_NODES) ;"), "{out}");
    assert!(!out.contains("SELF_HANDLE"), "{out}");
}

#[test]
fn beat_verb_requires_feature() {
    let expand = |verb: &str, f: &Features| {
        dataflow_expand(
            quote!(),
            format!("fn f(node: &'static TaskNode) {{ node.{verb}(&crate::OUT, 1); }}")
                .parse()
                .unwrap(),
            f,
        )
    };
    let off = Features {
        liveness: false,
        ..ALL
    };
    for verb in ["beat_put", "beat_writer"] {
        assert!(
            expand(verb, &ALL).is_ok(),
            "`{verb}` accepted with the feature"
        );
        match expand(verb, &off) {
            Ok(_) => panic!("`{verb}` accepted without the feature"),
            Err(err) => {
                let msg = err.to_string();
                assert!(
                    msg.contains("requires the `liveness` feature"),
                    "`{verb}`: {msg}"
                );
                assert!(
                    msg.contains(verb.trim_start_matches("beat_")),
                    "the message names the plain verb to fall back to: {msg}"
                );
            }
        }
    }
    assert!(expand("put", &off).is_ok(), "`put` needs no liveness");
    let gated = dataflow_expand(
        quote!(),
        "fn f(node: &'static TaskNode) { \
         #[cfg(feature = \"x\")] node.beat_put(&crate::OUT, 1); }"
            .parse()
            .unwrap(),
        &off,
    );
    assert!(gated.is_ok(), "a cfg-gated beat verb defers to rustc");
}

#[test]
fn coupling_clauses_require_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], reads: [crate::SIG], writes: [crate::OUT];",
        Features {
            coupling: false,
            ..ALL
        },
        "requires the `coupling` feature",
    );
}

#[test]
fn bound_marker_requires_feature() {
    let src = "node A = Terminate, deps: [];\nnode B = Terminate, deps: [A ready bound];";
    gate_rejects(
        src,
        Features {
            bound_deps: false,
            ..ALL
        },
        "requires the `bound-deps` feature",
    );
    gate_rejects(
        src,
        Features {
            readiness: false,
            ..ALL
        },
        "requires the `readiness` feature",
    );
}

#[test]
fn observed_marker_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, writes: [crate::S observed via it.get()];",
        Features {
            coupling_observe: false,
            ..ALL
        },
        "requires the `coupling-observe` feature",
    );
}

#[test]
fn discover_clause_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, discover;",
        Features {
            dataflow: false,
            ..ALL
        },
        "requires the `dataflow` feature",
    );
}

#[test]
fn dataflow_clause_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, dataflow: [crate::g];",
        Features {
            dataflow: false,
            ..ALL
        },
        "`dataflow:` requires the `dataflow` feature",
    );
}

/// A graph-level `observe` default is named as written, not as the entry
/// marker the author did not use.
#[test]
fn observe_default_requires_feature() {
    for dir in ["writes", "reads"] {
        gate_rejects(
            &format!("observe {dir}: it.get();\nnode A = Terminate, deps: [], task: f;"),
            Features {
                coupling_observe: false,
                ..ALL
            },
            &format!("`observe {dir}:` requires the `coupling-observe` feature"),
        );
    }
}

#[test]
fn local_resource_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, resources: [R: local Thing];",
        Features {
            local_resources: false,
            ..ALL
        },
        "`local-resources` feature",
    );
}

#[test]
fn state_clause_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, state: Buf = Buf::new();",
        Features {
            heap_state: false,
            ..ALL
        },
        "requires the `heap-state` feature",
    );
}

#[test]
fn zeroed_state_clause_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, state: zeroed Buf;",
        Features {
            heap_state: false,
            ..ALL
        },
        "requires the `heap-state` feature",
    );
}

#[test]
fn zeroed_state_parses_as_marker() {
    let spec = parse_gated(
        "node A = Terminate, deps: [], task: f, state: zeroed Buf;",
        &ALL,
    )
    .expect("marker form");
    let Item::Node(n) = &spec.items[0] else {
        panic!("node")
    };
    let Some((_, ty, StateInit::Zeroed(_))) = &n.state else {
        panic!("zeroed state")
    };
    assert_eq!(quote!(#ty).to_string(), "Buf");
    let spec = parse_gated(
        "node A = Terminate, deps: [], task: f, state: zeroed = Z;",
        &ALL,
    )
    .expect("type named zeroed");
    let Item::Node(n) = &spec.items[0] else {
        panic!("node")
    };
    let Some((_, ty, StateInit::Expr(_))) = &n.state else {
        panic!("init state")
    };
    assert_eq!(quote!(#ty).to_string(), "zeroed");
}

#[test]
fn deps_clause_is_optional() {
    assert!(parse_gated("node A = Terminate, task: f;", &ALL).is_ok());
    assert!(parse_gated("node A = Terminate, task: f, deps: [];", &ALL).is_ok());
    match parse_gated("node A = Terminate, deps: [], task: f, deps: [];", &ALL) {
        Ok(_) => panic!("duplicate deps accepted"),
        Err(e) => assert!(e.to_string().contains("duplicate `deps:`"), "{e}"),
    }
}

#[test]
fn provides_must_name_a_declared_slot() {
    let spec = syn::parse_str::<GraphSpec>(
        "node P = Terminate, deps: [], task: f, provides: [NOPE];\n\
         node C = Terminate, deps: [P], task: g, resources: [SLOT: shared u32];",
    )
    .unwrap();
    match expand(spec, &ALL) {
        Ok(_) => panic!("unknown slot accepted"),
        Err(e) => assert!(
            e.to_string().contains("no `resources:` entry"),
            "unexpected error: {e}"
        ),
    }
}

#[test]
fn beat_window_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, beat_timeout: 10, beat_window: 3;",
        Features {
            liveness_monitor: false,
            ..ALL
        },
        // `beat_window:` needs `beat_timeout:`, which is checked first.
        "`beat_timeout:` requires the `liveness-monitor` feature",
    );
}

/// A feature-gated construct under a `#[cfg]` on its item, entry or clause
/// is not rejected at expansion: it is dropped, and its error is deferred
/// to a `compile_error!` under the same cfgs.
#[test]
fn item_cfg_defers_feature_rejections() {
    const X: &str = "#[cfg(feature = \"x\")]";
    let node = |gated: &str, rest: &str| format!("{gated} node A = Terminate, deps: [], {rest};");
    // (source, the one feature off, what the construct emits when on)
    let cases: Vec<(String, Features, &str)> = vec![
        (
            format!("node NET = Terminate, deps: [];\n{X} node A = Terminate, deps: [NET ready];"),
            Features {
                readiness: false,
                ..ALL
            },
            "with_ready_deps",
        ),
        (
            format!("node NET = Terminate, deps: [];\nnode A = Terminate, deps: [{X} NET ready];"),
            Features {
                readiness: false,
                ..ALL
            },
            "with_ready_deps",
        ),
        (
            format!(
                "node NET = Terminate, deps: [];\n{X} node A = Terminate, deps: [NET ready bound];"
            ),
            Features {
                bound_deps: false,
                ..ALL
            },
            "with_bound_deps",
        ),
        (
            format!(
                "node NET = Terminate, deps: [];\nnode A = Terminate, deps: [{X} NET ready bound];"
            ),
            Features {
                bound_deps: false,
                ..ALL
            },
            "with_bound_deps",
        ),
        (
            node(X, "task: f, reads: [crate::S]"),
            Features {
                coupling: false,
                ..ALL
            },
            "with_reads",
        ),
        (
            node(X, "task: f, writes: [crate::S]"),
            Features {
                coupling: false,
                ..ALL
            },
            "with_writes",
        ),
        (
            node(X, "task: f, writes: [crate::S observed via it.get()]"),
            Features {
                coupling_observe: false,
                ..ALL
            },
            "with_writes",
        ),
        (
            node(
                "",
                &format!("task: f, writes: [{X} crate::S observed via it.get()]"),
            ),
            Features {
                coupling_observe: false,
                ..ALL
            },
            "with_writes",
        ),
        (
            node(X, "task: f, writes: [crate::TRIP veto]"),
            Features { veto: false, ..ALL },
            "__sv_check_veto",
        ),
        (
            node("", &format!("task: f, writes: [{X} crate::TRIP veto]")),
            Features { veto: false, ..ALL },
            "__sv_check_veto",
        ),
        (
            node(X, "task: w, discover"),
            Features {
                dataflow: false,
                ..ALL
            },
            "__SV_DATAFLOW_",
        ),
        (
            node("", &format!("task: w, {X} discover")),
            Features {
                dataflow: false,
                ..ALL
            },
            "__SV_DATAFLOW_",
        ),
        (
            node(X, "task: w, dataflow: [crate::g]"),
            Features {
                dataflow: false,
                ..ALL
            },
            "__SV_DATAFLOW_",
        ),
        (
            node("", &format!("task: w, dataflow: [{X} crate::g]")),
            Features {
                dataflow: false,
                ..ALL
            },
            "__SV_DATAFLOW_",
        ),
        (
            node(X, "task: f, resources: [R: local Thing]"),
            Features {
                local_resources: false,
                ..ALL
            },
            "__SvLocalResourceSlot",
        ),
        (
            node("", &format!("task: f, resources: [{X} R: local Thing]")),
            Features {
                local_resources: false,
                ..ALL
            },
            "__SvLocalResourceSlot",
        ),
        (
            node(X, "task: f, resources: [R: divisible]"),
            Features {
                budget: false,
                ..ALL
            },
            "with_claims",
        ),
        (
            node("", &format!("task: f, resources: [{X} R: divisible]")),
            Features {
                budget: false,
                ..ALL
            },
            "with_claims",
        ),
        (
            node(X, "task: f, state: Buf = Buf::new()"),
            Features {
                heap_state: false,
                ..ALL
            },
            "__sv_try_box",
        ),
        (
            node(X, "task: f, state: zeroed Buf"),
            Features {
                heap_state: false,
                ..ALL
            },
            "__sv_try_box",
        ),
        (
            node(
                X,
                "task: w, writes: [crate::S observed beat via it.get()], \
                 beat_timeout: 100, ready_on_write",
            ),
            Features {
                readiness: false,
                ..ALL
            },
            "with_ready_on_write",
        ),
        (
            node(
                "",
                &format!(
                    "task: w, writes: [crate::S observed beat via it.get()], \
                     beat_timeout: 100, {X} ready_on_write"
                ),
            ),
            Features {
                readiness: false,
                ..ALL
            },
            "with_ready_on_write",
        ),
        (
            node(X, "task: f, beat_timeout: 100"),
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "with_beat_timeout",
        ),
        (
            node("", &format!("task: f, {X} beat_timeout: 100")),
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "with_beat_timeout",
        ),
        (
            node(X, "task: f, beat_timeout: 100, beat_window: 3"),
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "with_beat_window",
        ),
        (
            node(
                "",
                &format!("task: f, {X} beat_timeout: 100, {X} beat_window: 3"),
            ),
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "with_beat_window",
        ),
        (
            node(X, "task: f, enabled_if: crate::on"),
            Features {
                enabled_if: false,
                ..ALL
            },
            "with_enabled_if",
        ),
        (
            node("", &format!("task: f, {X} enabled_if: crate::on")),
            Features {
                enabled_if: false,
                ..ALL
            },
            "with_enabled_if",
        ),
        (
            node(X, "task: f, restart_budget: 3 in 1000"),
            Features {
                restart_intensity: false,
                ..ALL
            },
            "with_restart_budget",
        ),
        (
            node("", &format!("task: f, {X} restart_budget: 3 in 1000")),
            Features {
                restart_intensity: false,
                ..ALL
            },
            "with_restart_budget",
        ),
        // `ready_on_write` without the monitor: deferred and dropped,
        // gated on its item or on itself.
        (
            node(
                X,
                "task: w, writes: [crate::S observed beat via it.get()], \
                 beat_timeout: 100, ready_on_write",
            ),
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "with_ready_on_write",
        ),
        (
            node(
                "",
                &format!(
                    "task: w, writes: [crate::S observed beat via it.get()], \
                     {X} beat_timeout: 100, {X} ready_on_write"
                ),
            ),
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "with_ready_on_write",
        ),
    ];
    for (src, off, emitted) in cases {
        let spec = syn::parse_str::<GraphSpec>(&src).unwrap_or_else(|e| panic!("{src}\n{e}"));
        let on = expand(spec.clone(), &ALL)
            .unwrap_or_else(|e| panic!("rejected with every feature on: {src}\n{e}"))
            .to_string();
        assert!(on.contains(emitted), "{src}\n{on}");
        assert!(!on.contains("compile_error"), "{src}\n{on}");
        let out = expand(spec, &off)
            .unwrap_or_else(|e| panic!("rejected although gated: {src}\n{e}"))
            .to_string();
        assert!(out.contains("compile_error"), "{src}\n{out}");
        assert!(!out.contains(emitted), "dropped, not emitted: {src}\n{out}");
    }
}

#[test]
fn enabled_if_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, enabled_if: crate::on;",
        Features {
            enabled_if: false,
            ..ALL
        },
        "requires the `enabled-if` feature",
    );
}

#[test]
fn restart_budget_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, restart_budget: 3 in 1000;",
        Features {
            restart_intensity: false,
            ..ALL
        },
        "requires the `restart-intensity` feature",
    );
}

#[test]
fn ready_on_write_requires_its_features() {
    let src = "node A = Terminate, deps: [], task: f, beat_timeout: 10, \
               ready_on_write, writes: [crate::S observed beat via it.get()];";
    for (off, needle) in [
        (
            Features {
                readiness: false,
                ..ALL
            },
            gate::READY_ON_WRITE_NEEDS,
        ),
        (
            Features {
                coupling_observe: false,
                ..ALL
            },
            "requires the `coupling-observe` feature",
        ),
        // The clause itself is refused, ahead of its `beat_timeout:`.
        (
            Features {
                liveness_monitor: false,
                ..ALL
            },
            "`ready_on_write` requires the `liveness-monitor` feature: the \
             monitor sweep is what asserts readiness",
        ),
        (
            Features {
                coupling: false,
                ..ALL
            },
            "requires the `coupling` feature",
        ),
    ] {
        gate_rejects(src, off, needle);
    }
}

#[test]
fn pool_requires_feature() {
    let src = "pool P = [Terminate], deps: [], task: w, \
               policy: embassy_supervisor::DeferredShrink::new(d()), min: 1, max: 1;";
    let spec = parse_gated(src, &ALL).expect("passes the gate");
    if let Err(e) = expand(spec, &ALL) {
        panic!("rejected with every feature on: {e}");
    }
    let off = Features { pool: false, ..ALL };
    let spec = parse_gated(src, &off).expect("passes the gate");
    match expand(spec, &off) {
        Ok(_) => panic!("a pool expanded without the feature"),
        Err(e) => assert!(
            e.to_string()
                .contains("requires enabling embassy-supervisor's `pool` feature"),
            "unexpected error: {e}"
        ),
    }
}

/// A failed `#[dataflow]` keeps its fn and, where a graph could bind
/// them, its two tables, empty.
#[test]
fn a_failed_dataflow_keeps_its_fn() {
    let item = || quote! { fn f(node: &'static TaskNode) { node.get(hidden); } };
    let err = dataflow_expand(TokenStream2::new(), item(), &ALL).unwrap_err();
    let out = dataflow_fallback(item(), err, &ALL).to_string();
    assert!(out.contains("fn f (node"), "{out}");
    assert!(out.contains("compile_error"), "{out}");
    assert!(
        out.contains("__SV_DATAFLOW_READS_f : [:: embassy_supervisor :: Coupling ; 0]"),
        "{out}"
    );
    assert!(
        out.contains("__SV_DATAFLOW_WRITES_f : [:: embassy_supervisor :: Coupling ; 0]"),
        "{out}"
    );
    // Without the feature no `discover` or `dataflow:` can name them.
    let off = Features {
        dataflow: false,
        ..ALL
    };
    let err = dataflow_expand(TokenStream2::new(), item(), &off).unwrap_err();
    let out = dataflow_fallback(item(), err, &off).to_string();
    assert!(out.contains("fn f (node"), "{out}");
    assert!(!out.contains("__SV_DATAFLOW_"), "{out}");
}

#[test]
fn dataflow_attributes_require_feature() {
    let off = Features {
        dataflow: false,
        ..ALL
    };
    let item = || quote! { fn f(node: &'static TaskNode) { node.put(&crate::OUT, 1); } };
    if let Err(e) = dataflow_expand(TokenStream2::new(), item(), &ALL) {
        panic!("`#[dataflow]` rejected with every feature on: {e}");
    }
    match dataflow_expand(TokenStream2::new(), item(), &off) {
        Ok(_) => panic!("`#[dataflow]` accepted without the feature"),
        Err(e) => assert!(
            e.to_string()
                .contains("`#[dataflow]` requires the `dataflow` feature"),
            "unexpected error: {e}"
        ),
    }
    let module = || {
        quote! {
            mod api {
                #[dataflow]
                fn f(node: &'static TaskNode) { node.put(&crate::OUT, 1); }
            }
        }
    };
    if let Err(e) = bundle_expand(TokenStream2::new(), module(), &ALL) {
        panic!("`#[dataflow_bundle]` rejected with every feature on: {e}");
    }
    match bundle_expand(TokenStream2::new(), module(), &off) {
        Ok(_) => panic!("`#[dataflow_bundle]` accepted without the feature"),
        Err(e) => assert!(
            e.to_string()
                .contains("`#[dataflow_bundle]` requires the `dataflow` feature"),
            "unexpected error: {e}"
        ),
    }
}

#[test]
fn emitted_glue_follows_the_feature_set() {
    let src = "node A = Terminate, deps: [], task: w;\nnode B = Terminate, deps: [A], task: w;";
    let expand_with = |f: &Features| {
        let spec = parse_gated(src, f).expect("passes the gate");
        expand(spec, f).expect("expansion succeeds").to_string()
    };
    let glue = [
        "with_graph",
        ". with_graph_ref (",
        "with_shell",
        "__sv_trace_hooks",
        ". adopt (",
    ];
    let none = expand_with(&NONE);
    for g in glue.iter().chain([&". stamp_name ("]) {
        assert!(!none.contains(g), "`{g}` without its feature: {none}");
    }
    let all = expand_with(&ALL);
    for g in glue {
        assert!(
            all.contains(g),
            "`{g}` missing with every feature on: {all}"
        );
    }
    assert!(
        !all.contains(". stamp_name ("),
        "`trace` adopts instead: {all}"
    );
    // The pool table is chained only onto a graph that declares a pool.
    assert!(!all.contains(". with_pools ("), "no pool declared: {all}");
    let pooled = "node A = Terminate, deps: [], task: w;\n\
                  pool P = [Terminate], deps: [A], task: w, \
                  policy: DeferredShrink::new(embassy_time::Duration::from_secs(1)), \
                  min: 1, max: 1;";
    let spec = parse_gated(pooled, &ALL).expect("passes the gate");
    let pooled = expand(spec, &ALL).expect("expansion succeeds").to_string();
    assert!(pooled.contains(". with_pools ("), "{pooled}");
    let names = expand_with(&Features {
        metadata_names: true,
        ..NONE
    });
    assert!(names.contains(". stamp_name ("), "{names}");
    assert!(!names.contains(". adopt ("), "{names}");
}

#[test]
fn struct_literal_field_calls_are_rewritten() {
    let item = quote! {
        async fn f(node: &'static TaskNode) -> R {
            R { a: node.reader(&crate::A), b: node.get(&crate::B) }
        }
    };
    let out = dataflow_expand(TokenStream2::new(), item, &ALL)
        .expect("expands")
        .to_string();
    assert_eq!(
        out.matches(":: embassy_supervisor :: Sig {").count(),
        2,
        "both field initializers are verb calls: {out}"
    );
}

/// The span of the first `compile_error` ident in `out`, through groups.
fn compile_error_span(out: TokenStream2) -> Option<proc_macro2::Span> {
    out.into_iter().find_map(|tt| match tt {
        proc_macro2::TokenTree::Ident(i) if i == "compile_error" => Some(i.span()),
        proc_macro2::TokenTree::Group(g) => compile_error_span(g.stream()),
        _ => None,
    })
}

#[test]
fn a_deferred_feature_error_points_at_its_clause() {
    let off = Features {
        enabled_if: false,
        ..ALL
    };
    let spec = parse_gated(
        "node A = Terminate, deps: [], #[cfg(feature = \"x\")] enabled_if: crate::on;",
        &off,
    )
    .expect("a gated clause passes the gate");
    let Item::Node(n) = &spec.items[0] else {
        panic!("node")
    };
    let kw = n.enabled_if.as_ref().expect("enabled_if").kw.span;
    let out = expand(spec, &off).expect("expansion succeeds");
    let at = compile_error_span(out).expect("a deferred error");
    assert_eq!(
        at.start(),
        kw.start(),
        "the error is spanned on `enabled_if`"
    );

    let off = Features {
        dataflow: false,
        ..ALL
    };
    let spec = parse_gated(
        "node A = Terminate, deps: [], task: w, #[cfg(feature = \"x\")] discover;",
        &off,
    )
    .expect("a gated clause passes the gate");
    let Item::Node(n) = &spec.items[0] else {
        panic!("node")
    };
    let kw = n.discover.as_ref().expect("discover").kw.span;
    let out = expand(spec, &off).expect("expansion succeeds");
    let at = compile_error_span(out).expect("a deferred error");
    assert_eq!(at.start(), kw.start(), "the error is spanned on `discover`");
}

#[test]
fn derived_public_names_cannot_be_reused() {
    for (src, what) in [
        (
            "pool P = [Terminate], deps: [], task: w, \
             policy: DeferredShrink::new(d), min: 1, max: 1;\n\
             node P_MAX = Terminate, deps: [];",
            "the `max:` const of pool `P`",
        ),
        (
            "node X = Terminate, deps: [], task: w, exit: u32;\n\
             node X_EXIT = Terminate, deps: [];",
            "the `exit:` slot of node `X`",
        ),
    ] {
        let spec = syn::parse_str::<GraphSpec>(src).unwrap();
        match expand(spec, &ALL) {
            Ok(_) => panic!("accepted: {src}"),
            Err(e) => assert!(e.to_string().contains(what), "unexpected error: {e}"),
        }
    }
}

#[test]
fn dataflow_indices_bake_after_full_collection() {
    let item = quote::quote! {
        async fn f(node: &'static TaskNode) {
            #[cfg(feature = "x")]
            node.put(&crate::A, 1u8);
            node.put(&crate::B, 2u8);
            node.put(&crate::A, 3u8);
        }
    };
    let out = dataflow_expand(TokenStream2::new(), item, &ALL)
        .expect("expands")
        .to_string();
    // The third call makes `A` unconditional, so the only `feature = "x"`
    // left is the statement's own attribute; a second one would be a stale
    // predicate on the table entry.
    assert_eq!(
        out.matches("feature = \"x\"").count(),
        1,
        "stale predicate survives: {out}"
    );
}

#[test]
fn divisible_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, resources: [R: divisible];",
        Features {
            budget: false,
            ..ALL
        },
        "`budget` feature",
    );
}

#[test]
fn divisible_emits_one_budget_sized_by_its_holders() {
    let spec = syn::parse_str::<GraphSpec>(
        "node A = Terminate, deps: [], task: f, resources: [P: divisible];\n\
         pool W = [Terminate, Terminate, Terminate], deps: [], task: g, \
         resources: [P: divisible], policy: DeferredShrink::new(d), min: 1, max: 3;\n\
         node B = Terminate, deps: [], task: f, resources: [P: divisible];",
    )
    .unwrap();
    let out = expand(spec, &ALL).expect("expansion succeeds").to_string();
    assert!(out.contains("pub static P : "), "{out}");
    assert!(
        out.contains("Budget < 5usize >"),
        "one slot for A, three for W, one for B: {out}"
    );
    assert!(out.contains("P . claimant (0u8)"), "A takes slot 0: {out}");
    assert!(
        out.contains("P . claimant ((1u8 as usize + i) as u8)"),
        "W's members take 1..=3: {out}"
    );
    assert!(out.contains("P . claimant (4u8)"), "B takes slot 4: {out}");
    assert!(
        out.contains("__SV_CLAIMS_1_W_2"),
        "one claims table per member: {out}"
    );
    assert!(
        out.contains("(& P , 3u8)"),
        "member 2 releases slot 3: {out}"
    );
    assert_eq!(out.matches(". with_claims (").count(), 5, "{out}");
    assert!(!out.contains(". restore ("), "nothing to restore: {out}");
}

#[test]
fn divisible_slots_are_counted_syntactically() {
    let spec = syn::parse_str::<GraphSpec>(
        "node A = Terminate, deps: [], task: f, resources: [#[cfg(any())] P: divisible];\n\
         node B = Terminate, deps: [], task: f, resources: [P: divisible];",
    )
    .unwrap();
    let out = expand(spec, &ALL).expect("expansion succeeds").to_string();
    assert!(
        out.contains("Budget < 2usize >"),
        "a cfg'd-out holder still takes its slot: {out}"
    );
}

#[test]
fn a_budget_may_be_provided_by_a_node() {
    let spec = syn::parse_str::<GraphSpec>(
        "node ALLOC = Terminate, deps: [], task: f, provides: [P];\n\
         node A = Terminate, deps: [ALLOC], task: g, resources: [P: divisible];",
    )
    .unwrap();
    let out = expand(spec, &ALL).expect("expansion succeeds").to_string();
    assert!(out.contains("__SV_PROVIDES_0_ALLOC"), "{out}");
}

/// A name is one kind, whichever declaration comes first.
#[test]
fn a_budget_name_cannot_double_as_a_take_kind_slot() {
    for src in [
        "node A = Terminate, deps: [], task: f, resources: [P: divisible];\n\
         node B = Terminate, deps: [], task: f, resources: [P: u32];",
        "node A = Terminate, deps: [], task: f, resources: [P: u32];\n\
         node B = Terminate, deps: [], task: f, resources: [P: divisible];",
        "node A = Terminate, deps: [], task: f, resources: [P: shared u32];\n\
         node B = Terminate, deps: [], task: f, resources: [P: u32];",
    ] {
        let spec = syn::parse_str::<GraphSpec>(src).unwrap();
        match expand(spec, &ALL) {
            Ok(_) => panic!("accepted: {src}"),
            Err(e) => assert!(
                e.to_string().contains(
                    "`P` is declared as a take-kind resource and as \
                     `shared`/`divisible`: a name has one kind"
                ),
                "{e}"
            ),
        }
    }
}

#[test]
fn veto_requires_feature() {
    gate_rejects(
        "node A = Terminate, deps: [], task: f, writes: [crate::TRIP veto];",
        Features { veto: false, ..ALL },
        "`veto` feature",
    );
}

#[test]
fn a_pool_without_veto_keeps_one_writes_table() {
    let spec = syn::parse_str::<GraphSpec>(
        "pool P = [Terminate, Terminate], deps: [], task: g, writes: [crate::OUT], \
         policy: DeferredShrink::new(d), min: 1, max: 2;",
    )
    .unwrap();
    let out = expand(spec, &ALL).expect("expansion succeeds").to_string();
    assert!(out.contains("__SV_WRITES_0_P :"), "{out}");
    assert!(!out.contains("__SV_WRITES_0_P_0"), "{out}");
}

#[test]
fn more_than_32_veto_writers_are_rejected() {
    let mut src = String::new();
    for i in 0..33 {
        src.push_str(&format!(
            "node N{i} = Terminate, deps: [], task: f, writes: [crate::TRIP veto];\n"
        ));
    }
    let spec = syn::parse_str::<GraphSpec>(&src).unwrap();
    match expand(spec, &ALL) {
        Ok(_) => panic!("accepted"),
        Err(e) => assert!(e.to_string().contains("more than 32"), "{e}"),
    }
}

#[test]
fn a_veto_gate_spelled_two_ways_is_rejected() {
    let spec = syn::parse_str::<GraphSpec>(
        "node A = Terminate, deps: [], task: f, writes: [crate::TRIP veto];\n\
         node B = Terminate, deps: [], task: f, writes: [TRIP veto];",
    )
    .unwrap();
    match expand(spec, &ALL) {
        Ok(_) => panic!("accepted"),
        Err(e) => {
            let e = e.to_string();
            assert!(e.contains("`crate::TRIP` and `TRIP`"), "{e}");
            assert!(e.contains("numbered per spelling"), "{e}");
        }
    }
    // Different statics that happen to share an ident are two gates.
    let spec = syn::parse_str::<GraphSpec>(
        "node A = Terminate, deps: [], task: f, writes: [crate::a::TRIP veto];\n\
         node B = Terminate, deps: [], task: f, writes: [crate::b::TRIP veto];",
    )
    .unwrap();
    match expand(spec, &ALL) {
        Ok(_) => panic!("accepted"),
        Err(e) => assert!(e.to_string().contains("`use .. as`"), "{e}"),
    }
    // An indexed gate is keyed by its index too.
    let spec = syn::parse_str::<GraphSpec>(
        "node A = Terminate, deps: [], task: f, writes: [crate::TRIP[0] veto];\n\
         node B = Terminate, deps: [], task: f, writes: [crate::TRIP[1] veto];",
    )
    .unwrap();
    let out = expand(spec, &ALL)
        .expect("two elements, two gates")
        .to_string();
    assert_eq!(out.matches("__sv_check_veto").count(), 2, "{out}");
    assert!(
        out.contains(". veto (0u8)") && !out.contains(". veto (1u8)"),
        "{out}"
    );
}

#[test]
fn the_veto_check_is_gated_like_its_writers() {
    let spec = syn::parse_str::<GraphSpec>(
        "#[cfg(feature = \"x\")] node A = Terminate, deps: [], task: f, \
         writes: [crate::TRIP veto];\n\
         node B = Terminate, deps: [], task: f, \
         writes: [#[cfg(feature = \"y\")] crate::TRIP veto];",
    )
    .unwrap();
    let out = expand(spec, &ALL).expect("expansion succeeds").to_string();
    assert!(
        out.contains("# [cfg (any (feature = \"x\" , feature = \"y\"))] const _ : () = "),
        "the check carries the union of its writers' cfgs: {out}"
    );
    let spec = syn::parse_str::<GraphSpec>(
        "#[cfg(feature = \"x\")] node A = Terminate, deps: [], task: f, \
         writes: [crate::TRIP veto];\n\
         node B = Terminate, deps: [], task: f, writes: [crate::TRIP veto];",
    )
    .unwrap();
    let out = expand(spec, &ALL).expect("expansion succeeds").to_string();
    assert!(
        out.contains("const _ : () = ") && !out.contains("))] const _ : () = "),
        "one unconditional writer keeps the check bare: {out}"
    );
}

#[test]
fn feature_verbs_name_their_feature_in_dataflow_bodies() {
    for (body, off, needle) in [
        (
            quote::quote! { let _ = node.open(&crate::EST).await; },
            Features {
                data_deps: false,
                ..ALL
            },
            "`data-deps` feature",
        ),
        (
            quote::quote! { let _ = node.lease(&crate::LNK); },
            Features {
                data_deps: false,
                ..ALL
            },
            "`data-deps` feature",
        ),
        (
            quote::quote! { node.veto(&crate::TRIP); },
            Features { veto: false, ..ALL },
            "`veto` feature",
        ),
        (
            quote::quote! { node.retire(&crate::EST, d).await; },
            Features {
                readiness: false,
                ..ALL
            },
            "`data-deps` and `readiness` features",
        ),
        (
            quote::quote! { node.retire(&crate::EST, d).await; },
            Features {
                data_deps: false,
                ..ALL
            },
            "`data-deps` and `readiness` features",
        ),
    ] {
        let item = quote::quote! {
            async fn f(node: &'static TaskNode) { #body }
        };
        if let Err(e) = dataflow_expand(TokenStream2::new(), item.clone(), &ALL) {
            panic!("rejected with every feature on: {e}");
        }
        match dataflow_expand(TokenStream2::new(), item, &off) {
            Ok(_) => panic!("accepted without the feature: {body}"),
            Err(e) => assert!(e.to_string().contains(needle), "{e}"),
        }
    }
}

#[test]
fn a_serialized_slot_holds_its_holders_to_one_executor() {
    let ok = syn::parse_str::<GraphSpec>(
        "executor HIGH;\n\
         node A = Terminate, deps: [], executor: HIGH, task: f, \
         resources: [BUS: shared serialized Bus];\n\
         node B = Terminate, deps: [], executor: HIGH, task: f, \
         resources: [BUS: shared serialized Bus];",
    )
    .unwrap();
    assert!(expand(ok, &ALL).is_ok(), "one tier: accepted");
    let root = syn::parse_str::<GraphSpec>(
        "node A = Terminate, deps: [], task: f, resources: [BUS: shared serialized Bus];\n\
         node B = Terminate, deps: [], task: f, resources: [BUS: shared serialized Bus];",
    )
    .unwrap();
    assert!(
        expand(root, &ALL).is_ok(),
        "both on the supervisor's executor: accepted"
    );
    for (src, a, b) in [
        (
            "executor HIGH;\n\
             node A = Terminate, deps: [], task: f, resources: [BUS: shared serialized Bus];\n\
             node B = Terminate, deps: [], executor: HIGH, task: f, \
             resources: [BUS: shared serialized Bus];",
            "`A` runs on the supervisor's executor",
            "`B` on `HIGH`",
        ),
        (
            "executor HIGH; executor LOW;\n\
             node A = Terminate, deps: [], executor: HIGH, task: f, \
             resources: [BUS: shared serialized Bus];\n\
             node B = Terminate, deps: [], executor: LOW, task: f, \
             resources: [BUS: shared serialized Bus];",
            "`A` runs on `HIGH`",
            "`B` on `LOW`",
        ),
    ] {
        let spec = syn::parse_str::<GraphSpec>(src).unwrap();
        match expand(spec, &ALL) {
            Ok(_) => panic!("accepted across tiers: {src}"),
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("priority ceiling"), "{msg}");
                assert!(msg.contains(a) && msg.contains(b), "{msg}");
            }
        }
    }
    let plain = syn::parse_str::<GraphSpec>(
        "executor HIGH;\n\
         node A = Terminate, deps: [], task: f, resources: [BUS: shared Bus];\n\
         node B = Terminate, deps: [], executor: HIGH, task: f, resources: [BUS: shared Bus];",
    )
    .unwrap();
    assert!(
        expand(plain, &ALL).is_ok(),
        "without the marker a shared slot may span tiers"
    );
}

/// Runtime names are lower-cased idents, and pool members are their
/// pool's name plus the index: two items mapping to one are refused.
#[test]
fn runtime_names_are_unique() {
    let pool = |name: &str| {
        format!(
            "pool {name} = [Terminate, Terminate], deps: [], task: w, \
             policy: DeferredShrink::new(d), min: 1, max: 2;"
        )
    };
    for (src, needle) in [
        (
            format!("{}\nnode W0 = Terminate, deps: [];", pool("W")),
            "runtime name `w0` is taken twice: `W[0]` and `W0`",
        ),
        (
            "node Net = Terminate, deps: [];\nnode NET = Terminate, deps: [];".to_string(),
            "runtime name `net` is taken twice: `Net` and `NET`",
        ),
        // The same ident twice is the duplicate-name check's.
        (
            "node A = Terminate, deps: [];\nnode A = Pause, deps: [];".to_string(),
            "duplicate node/pool name `A`",
        ),
    ] {
        let spec = syn::parse_str::<GraphSpec>(&src).unwrap();
        match expand(spec, &ALL) {
            Ok(_) => panic!("accepted: {src}"),
            Err(e) => assert!(e.to_string().contains(needle), "{src}\n{e}"),
        }
    }
    // A name past the pool's last member is free.
    let spec =
        syn::parse_str::<GraphSpec>(&format!("{}\nnode W2 = Terminate, deps: [];", pool("W")))
            .unwrap();
    if let Err(e) = expand(spec, &ALL) {
        panic!("`W2` beside a two-member pool `W` rejected: {e}");
    }
}

/// `it` is the signal only where it names a value: not as a field, a
/// method, a path segment, or a closure's own parameter.
#[test]
fn only_a_value_named_it_is_the_signal() {
    let target = quote!(crate::S);
    let sub = |src: &str| {
        substitute_it(&syn::parse_str(src).unwrap(), &target)
            .to_string()
            .replace(' ', "")
    };
    assert_eq!(sub("it.get()"), "(&crate::S).get()");
    assert_eq!(sub("it.it.it()"), "(&crate::S).it.it()");
    assert_eq!(sub("Wrapper::it(&it)"), "Wrapper::it(&(&crate::S))");
    assert_eq!(sub("|it: &u32| *it + 1"), "|it:&u32|*it+1");
    assert_eq!(
        sub("Some(it).map(|v| v.get())"),
        "Some((&crate::S)).map(|v|v.get())"
    );
    assert_eq!(
        sub("format_args!(\"{}\", it)"),
        "format_args!(\"{}\",(&crate::S))"
    );
}

/// A resource clash names the fragment its declaration came from.
#[test]
fn a_resource_clash_names_its_fragment() {
    for (src, needle) in [
        (
            "node A = Terminate, deps: [], task: f, resources: [H: shared u32];\n\
             @fragment NET; node B = Terminate, deps: [], task: f, \
             resources: [H: shared u8]; @endfragment;",
            "must repeat the same kind markers and type (from fragment `NET`)",
        ),
        (
            "node A = Terminate, deps: [], task: f, resources: [H: u32];\n\
             @fragment NET; node B = Terminate, deps: [], task: f, \
             resources: [H: u32]; @endfragment;",
            "may repeat a name) (from fragment `NET`)",
        ),
    ] {
        let spec = syn::parse_str::<GraphSpec>(src).unwrap();
        match expand(spec, &ALL) {
            Ok(_) => panic!("accepted: {src}"),
            Err(e) => assert!(e.to_string().contains(needle), "{e}"),
        }
    }
}
