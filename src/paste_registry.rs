//! Paste-marker registry — the `[pasted N lines from clipboard]` stub and the
//! clipboard text it stands for.
//!
//! A bracketed paste that spans more than one line is collapsed, at the moment
//! it lands in the line editor, into a single-line MARKER (rustyline's
//! `Validator::transform_paste` hook). That keeps a 400-line paste from
//! flooding the composer, and the marker is swapped back for the real text on
//! submit so the model always sees what the operator actually pasted.
//!
//! The stub used to be a one-way door: nothing could look INSIDE it. The
//! operator could not re-read what they had pasted before sending it, and the
//! transcript echo recorded the stub rather than the content, so scrollback
//! lost it for good. This module is the registry that makes the marker
//! INSPECTABLE:
//!
//! * [`stash`] mints a collision-free marker and remembers the payload,
//! * [`expand`] swaps every known marker back for its payload — used by the
//!   submit path, by the committed scrollback echo, and by the interactive
//!   Ctrl-E expansion,
//! * [`has_marker`] tells the Ctrl-E handler whether there is anything to
//!   expand, so the chord keeps its readline meaning when there isn't.
//!
//! The store is process-global on purpose. The Ctrl-E key handler sits OUTSIDE
//! the rustyline `Helper` that owns the paste hook (a `ConditionalEventHandler`
//! is handed an `EventContext`, never the helper), so a helper-owned `Mutex` is
//! simply not reachable from it. One interactive line editor per process makes
//! a global the honest shape; it is cleared at the start of every `read_line`
//! so markers never leak across submissions.

use std::sync::{Mutex, OnceLock};

/// Live registry: `(marker, payload)` in insertion order.
fn store() -> &'static Mutex<Vec<(String, String)>> {
    static STORE: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Collapse `text` into a single-line marker and remember it, returning the
/// marker. Single-line pastes are handed back untouched — there is nothing to
/// collapse and no reason to hide one line behind a stub.
///
/// This is the body of `Validator::transform_paste`, lifted out of the helper
/// so the registry owns both halves of the transform.
pub fn stash(text: String) -> String {
    if !text.contains('\n') {
        return text;
    }
    let mut pastes = store().lock().unwrap();
    let taken: Vec<&str> = pastes.iter().map(|(marker, _)| marker.as_str()).collect();
    let marker = marker_for(text.lines().count().max(1), &taken);
    drop(taken);
    pastes.push((marker.clone(), text));
    marker
}

/// Mint the marker for an `lines`-line paste that does not collide with any
/// marker in `taken`.
///
/// Pure so the disambiguation suffix is testable without a terminal: the first
/// paste of a given length gets the bare stub, later ones get `#2`, `#3`, …
/// Collision-freedom is what makes [`expand`] unambiguous — two markers with
/// the same text would make the mapping back to a payload a coin flip.
pub fn marker_for(lines: usize, taken: &[&str]) -> String {
    let mut marker = format!("[pasted {lines} lines from clipboard]");
    let mut id = 2;
    while taken.iter().any(|key| *key == marker) {
        marker = format!("[pasted {lines} lines from clipboard #{id}]");
        id += 1;
    }
    marker
}

/// Drop every remembered paste. Called when a fresh line starts, so a marker
/// can never resolve against a stale payload from an earlier line.
pub fn clear() {
    store().lock().unwrap().clear();
}

/// Swap every known marker in `line` for the clipboard text it stands for.
pub fn expand(line: &str) -> String {
    expand_with(line, &store().lock().unwrap())
}

/// Pure core of [`expand`]: replace only markers minted THIS editing session,
/// and never rescan inserted payloads — clipboard text that happens to contain
/// `[pasted 3 lines from clipboard]` is data, not a placeholder, so expansion
/// must not recurse into what it just inserted.
///
/// Leftmost-match-first so the output preserves the operator's ordering when a
/// line carries several markers.
pub fn expand_with(line: &str, pastes: &[(String, String)]) -> String {
    let mut result = String::new();
    let mut rest = line;
    while let Some((offset, marker, payload)) = pastes
        .iter()
        .filter_map(|(marker, payload)| rest.find(marker.as_str()).map(|i| (i, marker, payload)))
        .min_by_key(|(i, _, _)| *i)
    {
        result.push_str(&rest[..offset]);
        result.push_str(payload);
        rest = &rest[offset + marker.len()..];
    }
    result.push_str(rest);
    result
}

/// True when `line` still carries at least one live marker — i.e. there is
/// something for Ctrl-E to expand. False for an already-expanded line, which is
/// what lets Ctrl-E fall back to its readline `EndOfLine` meaning instead of
/// stealing the chord outright.
pub fn has_marker(line: &str) -> bool {
    let pastes = store().lock().unwrap();
    pastes
        .iter()
        .any(|(marker, _)| line.contains(marker.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: every test here exercises a PURE function. The live `store()` is
    // process-global and the test harness is multi-threaded, so asserting on
    // its contents would be a cross-test race; the global path is covered by
    // `repl::clipboard_paste_tests`, which clears it first.

    #[test]
    fn single_line_paste_is_not_collapsed() {
        // Nothing to hide behind a stub, and collapsing it would make a
        // one-line paste unusable mid-command.
        assert_eq!(stash("ls -la".to_string()), "ls -la");
    }

    #[test]
    fn marker_counts_lines_and_disambiguates() {
        assert_eq!(marker_for(3, &[]), "[pasted 3 lines from clipboard]");
        let taken = ["[pasted 3 lines from clipboard]"];
        assert_eq!(marker_for(3, &taken), "[pasted 3 lines from clipboard #2]");
        let taken = [
            "[pasted 3 lines from clipboard]",
            "[pasted 3 lines from clipboard #2]",
        ];
        assert_eq!(marker_for(3, &taken), "[pasted 3 lines from clipboard #3]");
        // A different length never collides, so it keeps the bare stub.
        assert_eq!(marker_for(9, &taken), "[pasted 9 lines from clipboard]");
    }

    #[test]
    fn expand_swaps_markers_in_operator_order() {
        let pastes = vec![
            ("[A]".to_string(), "alpha".to_string()),
            ("[B]".to_string(), "beta".to_string()),
        ];
        // Registry order is insertion order; OUTPUT order must follow the line.
        assert_eq!(expand_with("x [B] y [A] z", &pastes), "x beta y alpha z");
    }

    #[test]
    fn expand_does_not_recurse_into_inserted_payload() {
        // Payload that looks like another marker is DATA. Re-scanning it would
        // let clipboard text impersonate a placeholder.
        let pastes = vec![
            ("[A]".to_string(), "see [B]".to_string()),
            ("[B]".to_string(), "beta".to_string()),
        ];
        assert_eq!(expand_with("[A]", &pastes), "see [B]");
    }

    #[test]
    fn expand_leaves_unknown_markers_alone() {
        let pastes = vec![("[A]".to_string(), "alpha".to_string())];
        assert_eq!(
            expand_with("[pasted 7 lines from clipboard]", &pastes),
            "[pasted 7 lines from clipboard]"
        );
    }
}
