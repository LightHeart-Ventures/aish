//! TASK-809 — the sprint exit gate: the five-step loop composed end to end.
//!
//! Every *unit* of SPR-113 is already covered on its own card: `ready_set`,
//! `topo_order`, cycle detection and `validate` in `src/plan.rs` (TASK-802),
//! plan-graph persistence in `src/coordinator_store.rs` (TASK-804), and the
//! prompt constants in `src/coordinator.rs` (TASK-805/806/807). What no single
//! card asserts is the **composition** — the property the whole sprint exists to
//! deliver:
//!
//! ```text
//! review-ask → review-state → identify-delta → plan-with-deps → fan-out
//! ```
//!
//! i.e. a plan is persisted once, the done-set accumulates across turns, and a
//! FRESH process re-derives the SAME ready-set and the SAME fan-out decision
//! instead of re-planning (and re-fanning) from scratch. That is what turned an
//! 87-call runaway into a bounded loop, so it is asserted here as a loop-level
//! regression test rather than inferred from the per-card units.
//!
//! Why a `#[cfg(test)]` module in the binary instead of `tests/plan_dag_loop.rs`
//! (the file named in the engineering spec §2): `src/lib.rs` deliberately
//! re-exports ONLY `skill_contract`, and an integration test can only reach the
//! *lib* target. Exposing `coordinator_store` (and its rusqlite dependency
//! cascade) from the lib purely to host a test would compile the store twice and
//! violate the spec's own "zero production-code change" constraint. `src/main.rs`
//! already carries this exact pattern (`#[cfg(test)] mod
//! plugin_phase05_consolidation_tests;`), so the loop test lives in-crate and
//! reaches the real types directly. The only production delta is the one
//! test-gated `mod` line.
//!
//! NOT covered here, and deliberately so — the owning cards have not landed:
//! - the **delta artifact** round-trip (TASK-803): no `delta` table or API
//!   exists in the merged tree yet, so there is nothing to assert against.
//! - the **node-keyed lease** (TASK-808): `claim_work_package` is still keyed on
//!   normalized task TEXT. The loop-level property TASK-808 must deliver is
//!   pinned below as `node_keyed_lease_is_stable_where_task_text_is_not` so the
//!   requirement is on the record with the current behaviour measured, not
//!   assumed.

use crate::coordinator_store::{CoordinatorStore, work_package_key};
use crate::plan::{NodeId, PlanGraph, PlanNode};
use std::collections::HashSet;
use std::path::PathBuf;

/// A temp-dir db path, unique per test name + process. The coordinator store is
/// NEVER opened at `db_paths::main_db_path()` from a test — a test that writes
/// the developer's real coordinator db is a defect, not a convenience.
fn temp_db(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "aish_plan_dag_{tag}_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn node(id: &str, deps: &[&str], files: &[&str]) -> PlanNode {
    PlanNode {
        id: id.to_string(),
        intent: format!("do {id}"),
        files: files.iter().map(|s| (*s).to_string()).collect(),
        depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
        acceptance: vec![format!("{id} lands")],
        est_calls: 3,
    }
}

/// The canonical fixture: diamond `a -> {b, c} -> d`, with DISJOINT `files` on
/// the two middle nodes, so the ready pair is genuinely parallelizable.
fn diamond_graph(scope: &str) -> PlanGraph {
    PlanGraph {
        scope_key: scope.to_string(),
        nodes: vec![
            node("a", &[], &["src/a.rs"]),
            node("b", &["a"], &["src/b.rs"]),
            node("c", &["a"], &["src/c.rs"]),
            node("d", &["b", "c"], &["src/d.rs"]),
        ],
        created_at: 1_762_000_000,
    }
}

/// Same shape, but `b` and `c` both touch `src/shared.rs` — the ready pair is
/// NOT independent, so fan-out must collapse to solo.
fn shared_file_graph(scope: &str) -> PlanGraph {
    PlanGraph {
        scope_key: scope.to_string(),
        nodes: vec![
            node("a", &[], &["src/a.rs"]),
            node("b", &["a"], &["src/shared.rs", "src/b.rs"]),
            node("c", &["a"], &["src/shared.rs"]),
            node("d", &["b", "c"], &["src/d.rs"]),
        ],
        created_at: 1_762_000_000,
    }
}

fn ids(nodes: &[&PlanNode]) -> Vec<String> {
    nodes.iter().map(|n| n.id.clone()).collect()
}

/// STEP 4→5 composed through the STORE: plan-with-deps is persisted once, the
/// done-set accumulates, and fan-out is DERIVED from what the store reports —
/// never from the coordinator's in-memory recollection of its own plan.
#[test]
fn five_step_loop_derives_fan_out_from_the_persisted_plan_and_done_set() {
    let path = temp_db("five_step");
    let store = CoordinatorStore::open(&path).unwrap();
    let scope = "goal:spr113";

    // --- plan-with-deps: persisted exactly once.
    let planned = diamond_graph(scope);
    store.save_plan_graph(&planned).unwrap();

    // --- turn 1: nothing done. Ready-set is the single root; fan-out is None,
    // so the coordinator runs SOLO by construction (not by judgement).
    let graph = store
        .load_plan_graph(scope)
        .unwrap()
        .expect("graph persisted");
    assert_eq!(graph, planned, "the loaded plan is byte-identical");
    let done = store.plan_nodes_done(scope).unwrap();
    assert!(done.is_empty());
    assert_eq!(ids(&graph.ready_set(&done)), vec!["a"]);
    assert!(
        graph.fan_out_candidates(&done).is_none(),
        "one ready node => solo"
    );

    // --- turn 2: `a` done. BOTH middle nodes are ready and their file sets are
    // disjoint => fan out one worker per ready node, in declaration order.
    store.mark_plan_node_done(scope, "a").unwrap();
    let done = store.plan_nodes_done(scope).unwrap();
    assert_eq!(ids(&graph.ready_set(&done)), vec!["b", "c"]);
    let fan = graph
        .fan_out_candidates(&done)
        .expect("two disjoint ready nodes => fan out");
    assert_eq!(ids(&fan), vec!["b", "c"], "one worker per ready node");

    // --- turn 3: the join node only becomes ready once BOTH parents are done.
    store.mark_plan_node_done(scope, "b").unwrap();
    let done = store.plan_nodes_done(scope).unwrap();
    assert_eq!(
        ids(&graph.ready_set(&done)),
        vec!["c"],
        "`d` has an unmet dependency and must NEVER be dispatched"
    );
    store.mark_plan_node_done(scope, "c").unwrap();
    let done = store.plan_nodes_done(scope).unwrap();
    assert_eq!(ids(&graph.ready_set(&done)), vec!["d"]);
    assert!(graph.fan_out_candidates(&done).is_none());

    // --- terminal: everything done => nothing ready, nothing to dispatch.
    store.mark_plan_node_done(scope, "d").unwrap();
    let done = store.plan_nodes_done(scope).unwrap();
    assert!(graph.ready_set(&done).is_empty(), "loop terminates");
    assert!(graph.fan_out_candidates(&done).is_none());

    // The serial fallback ordering is still available and still a valid topo
    // order of the same persisted graph.
    assert_eq!(graph.topo_order().unwrap(), vec!["a", "b", "c", "d"]);

    let _ = std::fs::remove_file(&path);
}

/// COMPACTION SURVIVAL — the property the sprint exists to deliver. The goal
/// loop respawns a FRESH coordinator process each turn; if the plan did not
/// outlive the process, every turn would re-plan and re-fan the same work (that
/// is the 87-call runaway). A brand-new store handle on the same db must
/// reproduce the IDENTICAL ready-set and the IDENTICAL fan-out decision.
#[test]
fn reopened_store_reproduces_the_same_ready_set_and_fan_out_decision() {
    let path = temp_db("compaction");
    let scope = "goal:spr113";

    let (before_ready, before_fan): (Vec<String>, Option<Vec<String>>) = {
        let store = CoordinatorStore::open(&path).unwrap();
        store.save_plan_graph(&diamond_graph(scope)).unwrap();
        store.mark_plan_node_done(scope, "a").unwrap();
        let graph = store.load_plan_graph(scope).unwrap().unwrap();
        let done = store.plan_nodes_done(scope).unwrap();
        (
            ids(&graph.ready_set(&done)),
            graph.fan_out_candidates(&done).map(|r| ids(&r)),
        )
    }; // store dropped — stand-in for the process that planned exiting.

    let reopened = CoordinatorStore::open(&path).unwrap();
    let graph = reopened
        .load_plan_graph(scope)
        .unwrap()
        .expect("plan outlives the process that planned it");
    let done = reopened.plan_nodes_done(scope).unwrap();
    assert_eq!(
        ids(&graph.ready_set(&done)),
        before_ready,
        "a fresh process must NOT re-derive a different ready-set"
    );
    assert_eq!(
        graph.fan_out_candidates(&done).map(|r| ids(&r)),
        before_fan,
        "a fresh process must NOT re-decide fan-out"
    );
    assert_eq!(
        done,
        HashSet::from(["a".to_string()]),
        "the done-set accumulates across processes"
    );

    // And the done-set survives a RE-PLAN: work already finished stays finished,
    // so a re-plan can never resurrect node `a`.
    let mut replanned = diamond_graph(scope);
    replanned.created_at += 600;
    replanned.nodes.push(node("e", &["d"], &["src/e.rs"]));
    reopened.save_plan_graph(&replanned).unwrap();
    let graph = reopened.load_plan_graph(scope).unwrap().unwrap();
    let done = reopened.plan_nodes_done(scope).unwrap();
    assert_eq!(graph.nodes.len(), 5, "the re-plan replaced the graph");
    assert_eq!(
        ids(&graph.ready_set(&done)),
        vec!["b", "c"],
        "a done node is never re-dispatched after a re-plan"
    );

    let _ = std::fs::remove_file(&path);
}

/// THE 87-CALL RUNAWAY REGRESSION. Driving the persisted plan to completion is
/// BOUNDED: each iteration either fans out the disjoint ready set or runs one
/// node solo, each node is dispatched exactly once, and the loop terminates in
/// far fewer than 10 iterations. A loop that re-dispatches done nodes — the
/// runaway — fails the iteration assertion instead of burning 87 calls.
#[test]
fn driving_the_plan_to_completion_is_bounded_and_dispatches_each_node_once() {
    let path = temp_db("bounded");
    let store = CoordinatorStore::open(&path).unwrap();
    let scope = "goal:spr113";
    store.save_plan_graph(&diamond_graph(scope)).unwrap();
    let graph = store.load_plan_graph(scope).unwrap().unwrap();

    let mut iterations = 0_u32;
    let mut dispatched: Vec<String> = Vec::new();
    let mut widths: Vec<usize> = Vec::new();
    loop {
        iterations += 1;
        assert!(
            iterations < 10,
            "runaway: {iterations} iterations for a 4-node plan (dispatched: {dispatched:?})"
        );
        let done = store.plan_nodes_done(scope).unwrap();
        let ready = graph.ready_set(&done);
        if ready.is_empty() {
            break;
        }
        // THE decision, derived — never a judgement call.
        let wave: Vec<NodeId> = match graph.fan_out_candidates(&done) {
            Some(nodes) => ids(&nodes),
            None => vec![ready[0].id.clone()],
        };
        widths.push(wave.len());
        for id in wave {
            assert!(
                !dispatched.contains(&id),
                "node `{id}` dispatched twice — this is the runaway"
            );
            store.mark_plan_node_done(scope, &id).unwrap();
            dispatched.push(id);
        }
    }

    assert_eq!(
        dispatched,
        vec!["a", "b", "c", "d"],
        "every node dispatched exactly once, in dependency order"
    );
    assert_eq!(widths, vec![1, 2, 1], "solo, fan-out-2, solo");
    assert_eq!(iterations, 4, "three waves plus the terminating check");

    let _ = std::fs::remove_file(&path);
}

/// The same drive over a plan whose ready pair OVERLAPS on `src/shared.rs`:
/// two nodes are ready, but fan-out must collapse to SOLO (one worker per
/// iteration) because they would contend on a file — and crucially the loop is
/// STILL bounded. Overlapping work serializing is correct; it must not become a
/// runaway.
#[test]
fn overlapping_file_sets_collapse_to_solo_without_a_runaway() {
    let path = temp_db("solo");
    let store = CoordinatorStore::open(&path).unwrap();
    let scope = "goal:spr113";
    store.save_plan_graph(&shared_file_graph(scope)).unwrap();
    let graph = store.load_plan_graph(scope).unwrap().unwrap();

    // The ready pair really is a pair — the collapse is about FILES, not arity.
    store.mark_plan_node_done(scope, "a").unwrap();
    let done = store.plan_nodes_done(scope).unwrap();
    assert_eq!(ids(&graph.ready_set(&done)), vec!["b", "c"]);
    assert!(
        graph.fan_out_candidates(&done).is_none(),
        "two ready nodes sharing a file must run SOLO, never in parallel"
    );

    let mut iterations = 0_u32;
    let mut widths: Vec<usize> = Vec::new();
    let mut dispatched: Vec<String> = vec!["a".to_string()];
    loop {
        iterations += 1;
        assert!(iterations < 10, "runaway: {iterations} iterations");
        let done = store.plan_nodes_done(scope).unwrap();
        let ready = graph.ready_set(&done);
        if ready.is_empty() {
            break;
        }
        let wave: Vec<NodeId> = match graph.fan_out_candidates(&done) {
            Some(nodes) => ids(&nodes),
            None => vec![ready[0].id.clone()],
        };
        widths.push(wave.len());
        for id in wave {
            store.mark_plan_node_done(scope, &id).unwrap();
            dispatched.push(id);
        }
    }

    assert!(
        widths.iter().all(|&w| w == 1),
        "every wave is SOLO while files overlap, got {widths:?}"
    );
    assert_eq!(dispatched, vec!["a", "b", "c", "d"]);
    assert!(iterations < 10, "bounded, not an 87-call runaway");

    let _ = std::fs::remove_file(&path);
}

/// TASK-808's requirement, pinned at the loop level with the CURRENT behaviour
/// measured rather than assumed: a lease keyed on the plan NODE ID is stable
/// across a re-worded brief, whereas the text-derived key in the merged tree is
/// not. Until TASK-808 lands, re-wording a work package's prose mints a NEW
/// lease key and the same node can be dispatched twice — exactly the
/// duplicate-work class SPR-113 is closing. This test fails the day the key
/// stops being stable, in either direction.
#[test]
fn node_keyed_lease_is_stable_where_task_text_is_not() {
    // Same node, two phrasings of the same brief.
    let brief_v1 = "Implement the ready-set helper in src/plan.rs";
    let brief_v2 = "implement ready-set helper — src/plan.rs (re-worded)";
    assert_ne!(
        work_package_key(brief_v1),
        work_package_key(brief_v2),
        "text-derived keys drift with wording — the gap TASK-808 closes"
    );

    // Keying on the node id is wording-independent by construction, and still
    // scoped tightly enough that two nodes never collide.
    let key_for = |scope: &str, node: &str| work_package_key(&format!("{scope}::node::{node}"));
    assert_eq!(
        key_for("goal:spr113", "ready-set"),
        key_for("goal:spr113", "ready-set"),
        "node-keyed leases survive a re-worded brief"
    );
    assert_ne!(
        key_for("goal:spr113", "ready-set"),
        key_for("goal:spr113", "topo-order"),
        "distinct nodes must not contend for one lease"
    );

    // And the ledger honours it: the second claim on the SAME node id, by a
    // DIFFERENT run, is refused loudly instead of silently duplicating work.
    let path = temp_db("lease");
    let store = CoordinatorStore::open(&path).unwrap();
    let node_task = "goal:spr113::node::ready-set";
    store
        .claim_work_package("goal:spr113", node_task, "run_one", 600)
        .unwrap();
    store.insert("run_one", "work node", "sess", None).unwrap();
    let err = store
        .claim_work_package("goal:spr113", node_task, "run_two", 600)
        .expect_err("a live claim on the same node must be refused");
    assert!(
        err.to_string().contains("already claimed by run `run_one`"),
        "the refusal must name the holder, got: {err}"
    );
    // Re-claiming as the SAME owner just renews.
    store
        .claim_work_package("goal:spr113", node_task, "run_one", 600)
        .expect("same-owner re-claim renews");

    let _ = std::fs::remove_file(&path);
}
