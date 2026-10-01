//! aish — library surface.
//!
//! aish is primarily a binary (`src/main.rs`). This lib target exists for one
//! narrow, deliberate reason (TASK-694): the skill-registry **mirror generator**
//! in `tools/skill-mirror` must apply byte-identical validation to the client,
//! so it links the real rules instead of reimplementing them.
//!
//! Keep this surface minimal. It intentionally re-exports **only**
//! [`skill_contract`] — the pure frontmatter/path-segment/catalog-row rules.
//! Adding a heavyweight module here would force it (and its dependency
//! cascade) to compile twice, once for the lib and once for the bin, for no
//! benefit. The binary does not depend on this lib target; its behaviour is
//! unchanged.

pub mod skill_contract;
