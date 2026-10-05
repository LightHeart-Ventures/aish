//! Plan-graph core — pure logic, no I/O.
//!
//! A [`PlanGraph`] is a DAG of [`PlanNode`]s describing the work a coordinator
//! intends to do for a scope. The module owns three operations the rest of the
//! sprint consumes:
//!
//! - [`PlanGraph::ready_set`] — the nodes whose dependencies are all satisfied.
//!   This is what fan-out dispatches, which makes parallelism a *computed
//!   property* instead of a judgement call.
//! - [`PlanGraph::topo_order`] — deterministic serial ordering, and the cycle
//!   detector.
//! - [`PlanGraph::validate`] — reject dangling `depends_on`, duplicate ids,
//!   malformed ids, and self-dependency before anything is persisted.
//!
//! Deliberately free of SQLite, network, and filesystem access so it is fully
//! unit-testable.

// Consumers land in later SPR-113 cards (persistence in TASK-804, fan-out
// dispatch in TASK-807); this card ships the types + algorithms + their tests,
// so nothing in the binary references them yet.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};

/// Identifier of a plan node. Must match `^[a-z0-9][a-z0-9-]{0,47}$`.
pub type NodeId = String;

/// Maximum length of a [`NodeId`].
const MAX_ID_LEN: usize = 48;

/// One unit of intended work inside a [`PlanGraph`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanNode {
    /// Stable identifier, unique within the graph.
    pub id: NodeId,
    /// Human-readable description of what this node accomplishes.
    pub intent: String,
    /// Paths this node is expected to touch. Used to decide whether two ready
    /// nodes can safely run in parallel.
    #[serde(default)]
    pub files: Vec<String>,
    /// Ids of nodes that must be done before this one can start.
    #[serde(default)]
    pub depends_on: Vec<NodeId>,
    /// Acceptance criteria for this node.
    #[serde(default)]
    pub acceptance: Vec<String>,
    /// Rough estimate of tool calls needed.
    #[serde(default)]
    pub est_calls: u32,
}

/// A graph of [`PlanNode`]s scoped to a single coordinator scope key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanGraph {
    /// Scope this plan belongs to.
    pub scope_key: String,
    /// Nodes in declaration order. Declaration order is load-bearing: it is the
    /// deterministic tie-break for [`PlanGraph::ready_set`] and
    /// [`PlanGraph::topo_order`].
    pub nodes: Vec<PlanNode>,
    /// Unix epoch seconds the plan was created.
    pub created_at: i64,
}

/// Why a [`PlanGraph`] is not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The graph contains a cycle. Carries the unresolved node ids, sorted.
    Cycle(Vec<NodeId>),
    /// `node` depends on `missing`, which is not present in the graph.
    DanglingDep { node: NodeId, missing: NodeId },
    /// The same id appears on more than one node.
    DuplicateId(NodeId),
    /// The id does not match `^[a-z0-9][a-z0-9-]{0,47}$`, or a node depends on
    /// itself.
    InvalidId(NodeId),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Cycle(ids) => {
                write!(f, "plan graph contains a cycle among: {}", ids.join(", "))
            }
            PlanError::DanglingDep { node, missing } => write!(
                f,
                "node '{node}' depends on '{missing}', which is not in the plan"
            ),
            PlanError::DuplicateId(id) => write!(f, "duplicate plan node id '{id}'"),
            PlanError::InvalidId(id) => write!(
                f,
                "invalid plan node id '{id}' (expected ^[a-z0-9][a-z0-9-]{{0,47}}$ and no self-dependency)"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// `^[a-z0-9][a-z0-9-]{0,47}$` without pulling in a regex engine.
fn is_valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return false;
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl PlanGraph {
    /// Validate ids, uniqueness, and dependency resolution.
    ///
    /// Returns the first violation found, scanning nodes in declaration order.
    pub fn validate(&self) -> Result<(), PlanError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if !is_valid_id(&node.id) {
                return Err(PlanError::InvalidId(node.id.clone()));
            }
            if !seen.insert(node.id.as_str()) {
                return Err(PlanError::DuplicateId(node.id.clone()));
            }
        }
        for node in &self.nodes {
            for dep in &node.depends_on {
                if dep == &node.id {
                    return Err(PlanError::InvalidId(node.id.clone()));
                }
                if !seen.contains(dep.as_str()) {
                    return Err(PlanError::DanglingDep {
                        node: node.id.clone(),
                        missing: dep.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Nodes that are not yet `done` and whose dependencies are all `done`.
    ///
    /// Result is in declaration order, so it is deterministic.
    pub fn ready_set(&self, done: &HashSet<NodeId>) -> Vec<&PlanNode> {
        self.nodes
            .iter()
            .filter(|n| !done.contains(&n.id))
            .filter(|n| n.depends_on.iter().all(|d| done.contains(d)))
            .collect()
    }

    /// Kahn's algorithm with a declaration-order tie-break.
    ///
    /// Returns `Err(PlanError::Cycle(remaining))` when the graph does not fully
    /// resolve; `remaining` is sorted for determinism.
    pub fn topo_order(&self) -> Result<Vec<NodeId>, PlanError> {
        let index: HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();

        // in_degree counts only dependencies that exist in the graph; dangling
        // deps are validate()'s business, not topo_order()'s.
        let mut in_degree: Vec<usize> = self
            .nodes
            .iter()
            .map(|n| {
                n.depends_on
                    .iter()
                    .filter(|d| index.contains_key(d.as_str()) && d.as_str() != n.id.as_str())
                    .count()
            })
            .collect();

        // dependency id -> indices of nodes that depend on it
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate() {
            for dep in &node.depends_on {
                if let Some(&d) = index.get(dep.as_str())
                    && d != i
                {
                    dependents[d].push(i);
                }
            }
        }

        // Seed in declaration order so ties resolve deterministically.
        let mut queue: VecDeque<usize> = (0..self.nodes.len())
            .filter(|&i| in_degree[i] == 0)
            .collect();

        let mut order: Vec<NodeId> = Vec::with_capacity(self.nodes.len());
        let mut emitted = vec![false; self.nodes.len()];
        while let Some(i) = queue.pop_front() {
            order.push(self.nodes[i].id.clone());
            emitted[i] = true;
            // Collect newly-freed dependents, then push in declaration order.
            let mut freed: Vec<usize> = Vec::new();
            for &dep_idx in &dependents[i] {
                in_degree[dep_idx] -= 1;
                if in_degree[dep_idx] == 0 {
                    freed.push(dep_idx);
                }
            }
            freed.sort_unstable();
            for idx in freed {
                queue.push_back(idx);
            }
        }

        if order.len() != self.nodes.len() {
            let mut remaining: Vec<NodeId> = self
                .nodes
                .iter()
                .enumerate()
                .filter(|(i, _)| !emitted[*i])
                .map(|(_, n)| n.id.clone())
                .collect();
            remaining.sort();
            return Err(PlanError::Cycle(remaining));
        }
        Ok(order)
    }

    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&PlanNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// `Some(ready)` when >= 2 nodes are ready AND no two of them share any path
    /// string in `files`; `None` otherwise (meaning: run solo).
    ///
    /// This predicate is the code twin of the fan-out prompt rule: fan out only
    /// when the ready work is genuinely independent.
    pub fn fan_out_candidates(&self, done: &HashSet<NodeId>) -> Option<Vec<&PlanNode>> {
        let ready = self.ready_set(done);
        if ready.len() < 2 {
            return None;
        }
        let mut claimed: HashSet<&str> = HashSet::new();
        for node in &ready {
            for file in &node.files {
                if !claimed.insert(file.as_str()) {
                    return None;
                }
            }
        }
        Some(ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, deps: &[&str]) -> PlanNode {
        PlanNode {
            id: id.to_string(),
            intent: format!("do {id}"),
            files: Vec::new(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            acceptance: Vec::new(),
            est_calls: 0,
        }
    }

    fn graph(nodes: Vec<PlanNode>) -> PlanGraph {
        PlanGraph {
            scope_key: "scope/test".to_string(),
            nodes,
            created_at: 1_700_000_000,
        }
    }

    fn done(ids: &[&str]) -> HashSet<NodeId> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn ids(nodes: &[&PlanNode]) -> Vec<String> {
        nodes.iter().map(|n| n.id.clone()).collect()
    }

    fn linear() -> PlanGraph {
        graph(vec![node("a", &[]), node("b", &["a"]), node("c", &["b"])])
    }

    fn diamond() -> PlanGraph {
        graph(vec![
            node("a", &[]),
            node("b", &["a"]),
            node("c", &["a"]),
            node("d", &["b", "c"]),
        ])
    }

    // ---------- ready_set ----------

    #[test]
    fn ready_set_empty_graph_is_empty() {
        let g = graph(vec![]);
        assert!(g.ready_set(&done(&[])).is_empty());
    }

    #[test]
    fn ready_set_linear_chain_surfaces_one_at_a_time() {
        let g = linear();
        assert_eq!(ids(&g.ready_set(&done(&[]))), vec!["a"]);
        assert_eq!(ids(&g.ready_set(&done(&["a"]))), vec!["b"]);
        assert_eq!(ids(&g.ready_set(&done(&["a", "b"]))), vec!["c"]);
    }

    #[test]
    fn ready_set_diamond_surfaces_both_branches_then_the_join() {
        let g = diamond();
        assert_eq!(ids(&g.ready_set(&done(&[]))), vec!["a"]);
        assert_eq!(ids(&g.ready_set(&done(&["a"]))), vec!["b", "c"]);
        assert_eq!(ids(&g.ready_set(&done(&["a", "b", "c"]))), vec!["d"]);
    }

    #[test]
    fn ready_set_all_done_is_empty() {
        let g = diamond();
        assert!(g.ready_set(&done(&["a", "b", "c", "d"])).is_empty());
        let g = linear();
        assert!(g.ready_set(&done(&["a", "b", "c"])).is_empty());
    }

    #[test]
    fn ready_set_preserves_declaration_order() {
        let g = graph(vec![node("z", &[]), node("m", &[]), node("a", &[])]);
        assert_eq!(ids(&g.ready_set(&done(&[]))), vec!["z", "m", "a"]);
    }

    // ---------- topo_order ----------

    /// Assert every dependency precedes its dependent in `order`.
    fn assert_valid_order(g: &PlanGraph, order: &[NodeId]) {
        assert_eq!(order.len(), g.nodes.len(), "order must cover every node");
        let pos: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        for n in &g.nodes {
            for dep in &n.depends_on {
                assert!(
                    pos[dep.as_str()] < pos[n.id.as_str()],
                    "{dep} must precede {}",
                    n.id
                );
            }
        }
    }

    #[test]
    fn topo_order_linear_is_valid() {
        let g = linear();
        let order = g.topo_order().expect("linear graph is a DAG");
        assert_valid_order(&g, &order);
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_order_diamond_is_valid_and_deterministic() {
        let g = diamond();
        let order = g.topo_order().expect("diamond graph is a DAG");
        assert_valid_order(&g, &order);
        assert_eq!(order, vec!["a", "b", "c", "d"]);
        // determinism: same input, same output
        assert_eq!(g.topo_order().unwrap(), order);
    }

    #[test]
    fn topo_order_empty_graph_is_empty() {
        assert_eq!(graph(vec![]).topo_order().unwrap(), Vec::<NodeId>::new());
    }

    #[test]
    fn topo_order_two_node_cycle_is_err() {
        let g = graph(vec![node("a", &["b"]), node("b", &["a"])]);
        match g.topo_order() {
            Err(PlanError::Cycle(remaining)) => assert_eq!(remaining, vec!["a", "b"]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn topo_order_three_node_cycle_is_err() {
        let g = graph(vec![
            node("a", &["c"]),
            node("b", &["a"]),
            node("c", &["b"]),
        ]);
        match g.topo_order() {
            Err(PlanError::Cycle(remaining)) => assert_eq!(remaining, vec!["a", "b", "c"]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn topo_order_cycle_with_a_clean_prefix_reports_only_the_cycle() {
        let g = graph(vec![
            node("root", &[]),
            node("a", &["root", "b"]),
            node("b", &["a"]),
        ]);
        match g.topo_order() {
            Err(PlanError::Cycle(remaining)) => assert_eq!(remaining, vec!["a", "b"]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    // ---------- validate ----------

    #[test]
    fn validate_accepts_a_clean_dag() {
        diamond().validate().expect("diamond is valid");
        linear().validate().expect("linear is valid");
        graph(vec![]).validate().expect("empty graph is valid");
    }

    #[test]
    fn validate_rejects_dangling_dependency() {
        let g = graph(vec![node("a", &[]), node("b", &["nope"])]);
        assert_eq!(
            g.validate(),
            Err(PlanError::DanglingDep {
                node: "b".to_string(),
                missing: "nope".to_string(),
            })
        );
    }

    #[test]
    fn validate_rejects_duplicate_id() {
        let g = graph(vec![node("a", &[]), node("a", &[])]);
        assert_eq!(g.validate(), Err(PlanError::DuplicateId("a".to_string())));
    }

    #[test]
    fn validate_rejects_malformed_id() {
        let g = graph(vec![node("Bad_Id", &[])]);
        assert_eq!(
            g.validate(),
            Err(PlanError::InvalidId("Bad_Id".to_string()))
        );
        // leading hyphen, empty, and over-length are all rejected too
        assert_eq!(
            graph(vec![node("-lead", &[])]).validate(),
            Err(PlanError::InvalidId("-lead".to_string()))
        );
        assert_eq!(
            graph(vec![node("", &[])]).validate(),
            Err(PlanError::InvalidId(String::new()))
        );
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert_eq!(
            graph(vec![node(&long, &[])]).validate(),
            Err(PlanError::InvalidId(long))
        );
    }

    #[test]
    fn validate_accepts_boundary_length_id() {
        let max = "a".repeat(MAX_ID_LEN);
        graph(vec![node(&max, &[])])
            .validate()
            .expect("48-char id is valid");
    }

    #[test]
    fn validate_rejects_self_dependency() {
        let g = graph(vec![node("a", &["a"])]);
        assert_eq!(g.validate(), Err(PlanError::InvalidId("a".to_string())));
    }

    #[test]
    fn plan_error_displays_and_is_an_error() {
        let e = PlanError::DanglingDep {
            node: "b".to_string(),
            missing: "nope".to_string(),
        };
        let msg = e.to_string();
        assert!(msg.contains("b") && msg.contains("nope"), "got: {msg}");
        let _: &dyn std::error::Error = &e;
        assert!(
            PlanError::Cycle(vec!["a".into(), "b".into()])
                .to_string()
                .contains("cycle")
        );
        assert!(
            PlanError::DuplicateId("a".into())
                .to_string()
                .contains("duplicate")
        );
        assert!(PlanError::InvalidId("A".into()).to_string().contains("A"));
    }

    // ---------- node ----------

    #[test]
    fn node_lookup_hits_and_misses() {
        let g = diamond();
        assert_eq!(g.node("c").map(|n| n.id.as_str()), Some("c"));
        assert!(g.node("missing").is_none());
    }

    // ---------- fan_out_candidates ----------

    fn node_with_files(id: &str, deps: &[&str], files: &[&str]) -> PlanNode {
        let mut n = node(id, deps);
        n.files = files.iter().map(|s| s.to_string()).collect();
        n
    }

    #[test]
    fn fan_out_candidates_some_when_two_ready_nodes_are_disjoint() {
        let g = graph(vec![
            node_with_files("a", &[], &["src/a.rs"]),
            node_with_files("b", &[], &["src/b.rs"]),
        ]);
        let candidates = g.fan_out_candidates(&done(&[])).expect("disjoint -> Some");
        assert_eq!(candidates.len(), 2);
        assert_eq!(ids(&candidates), vec!["a", "b"]);
    }

    #[test]
    fn fan_out_candidates_none_when_ready_nodes_share_a_file() {
        let g = graph(vec![
            node_with_files("a", &[], &["src/shared.rs", "src/a.rs"]),
            node_with_files("b", &[], &["src/shared.rs"]),
        ]);
        assert!(g.fan_out_candidates(&done(&[])).is_none());
    }

    #[test]
    fn fan_out_candidates_none_when_only_one_node_is_ready() {
        let g = linear();
        assert!(g.fan_out_candidates(&done(&[])).is_none());
        assert!(g.fan_out_candidates(&done(&["a"])).is_none());
    }

    #[test]
    fn fan_out_candidates_none_when_nothing_is_ready() {
        let g = linear();
        assert!(g.fan_out_candidates(&done(&["a", "b", "c"])).is_none());
    }

    #[test]
    fn fan_out_candidates_some_when_ready_nodes_declare_no_files() {
        let g = graph(vec![node("a", &[]), node("b", &[])]);
        assert_eq!(
            g.fan_out_candidates(&done(&[])).map(|r| r.len()),
            Some(2),
            "no declared files cannot collide"
        );
    }

    // ---------- serde ----------

    #[test]
    fn plan_graph_survives_a_json_round_trip() {
        let original = PlanGraph {
            scope_key: "repo/aish#SPR-113".to_string(),
            nodes: vec![
                PlanNode {
                    id: "scaffold".to_string(),
                    intent: "create the module".to_string(),
                    files: vec!["src/plan.rs".to_string()],
                    depends_on: vec![],
                    acceptance: vec!["module compiles".to_string()],
                    est_calls: 4,
                },
                PlanNode {
                    id: "wire-up".to_string(),
                    intent: "register the module".to_string(),
                    files: vec!["src/main.rs".to_string()],
                    depends_on: vec!["scaffold".to_string()],
                    acceptance: vec!["mod plan; present".to_string(), "tests green".to_string()],
                    est_calls: 2,
                },
            ],
            created_at: 1_762_000_000,
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: PlanGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, original);
    }

    #[test]
    fn plan_node_defaults_fill_in_omitted_fields() {
        let decoded: PlanNode =
            serde_json::from_str(r#"{"id":"a","intent":"do a"}"#).expect("defaults apply");
        assert_eq!(decoded, node("a", &[]));
    }
}
