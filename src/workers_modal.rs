//! Interactive `:workers` modal picker (TASK-313).
//!
//! Turns the static `:workers` markdown table into a keyboard-driven popup the
//! operator can drive at the idle prompt:
//!
//! * **↑/↓** (or `k`/`j`) — move the selection between worker rows.
//! * **Enter** — `:attach` the selected worker.
//! * **Delete / `d`** — `:close` the selected worker.
//! * **Esc / `q`** — dismiss, leaving the prompt untouched.
//!
//! ## Design
//! There is **no crossterm in the tree** — we mirror [`crate::keywatch`], which
//! already does cbreak-via-libc-termios, CSI byte-stream parsing, and RAII
//! restore. The modal is a self-contained *synchronous* helper invoked inline
//! from the `Some("workers")` arm in `repl.rs`; it briefly owns the tty exactly
//! like a confirm prompt. It never touches the async turn machinery and never
//! reimplements attach/close — it returns a [`ModalAction`] and the caller
//! dispatches to the existing `attach_worker` / `close_worker` paths.
//!
//! Everything that can be tested without a real terminal is split into pure
//! functions ([`parse_modal_keys`], [`move_selection`]) with unit tests; the
//! render + termios juggling is best-effort and TTY-guarded by the caller.

use std::collections::VecDeque;
use std::io::{self, Write};

/// One selectable row in the modal — the snapshot the caller collects from the
/// session's live workers. Raw (un-styled) status/result strings are carried so
/// the modal can colorize them; the active row is marked with a `>` gutter
/// caret rather than highlighting the whole row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    /// Stable run id — the value handed to `attach_worker` / `close_worker`.
    pub id: String,
    /// Worker-type glyph: 🤖 coordinator · 🎯 goal · ⏰ alert. Derived from the
    /// task text (`crate::style::job_activity_emoji`) so the modal shows the
    /// same type indicator the static `:workers` table stamps.
    pub type_emoji: String,
    /// Display id (may carry a `↻N` resumed-thread marker).
    pub id_cell: String,
    /// Session label cell (e.g. `abcd *`).
    pub session_label: String,
    /// Raw status word (`running`, `done`, `failed`, …) for `styled_status`.
    pub status: String,
    /// Relative "started ago" cell.
    pub started_cell: String,
    /// Elapsed / total runtime cell.
    pub runtime_cell: String,
    /// One-line clipped task text.
    pub task: String,
    /// Raw result cell (`✓ #42`, `✗ …`, `—`) for `styled_result`.
    pub result_cell: String,
    /// Display-only parent linkage (durable `parent_run_id`). `None` at a root.
    /// Used only to order/indent the forest — never for dispatch.
    pub parent_id: Option<String>,
    /// Depth in the forest (0 = root). Set by [`build_worker_forest`]; the
    /// table/modal indent the `task` cell by this many levels.
    pub depth: usize,
}

/// What the operator chose when the modal returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalAction {
    /// `:attach` this worker id.
    Attach(String),
    /// `:close` this worker id.
    Close(String),
    /// Esc/`q` — do nothing, return to the prompt.
    Dismiss,
}

/// A parsed keypress the modal acts on. `Other` covers bytes we ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Delete,
    Dismiss,
}

/// Move `sel` within `[0, len)` for a key, saturating at both ends (no wrap —
/// matches the spec). Pure so the clamp logic is unit-tested. `len == 0` pins 0.
pub fn move_selection(sel: usize, len: usize, key: Key) -> usize {
    if len == 0 {
        return 0;
    }
    match key {
        Key::Up => sel.saturating_sub(1),
        Key::Down => (sel + 1).min(len - 1),
        _ => sel.min(len - 1),
    }
}

/// Order a flat set of [`WorkerRow`]s into a stable pre-order **forest** and
/// stamp each row's `depth`.
///
/// - **Roots** are rows whose `parent_id` is `None` *or* whose parent is not
///   present in the input set (a transitive descendant whose coordinator this
///   session can't see degrades gracefully to a root). Root order is preserved
///   from the input (callers pass newest-first).
/// - Each root is immediately followed by its children (input order preserved),
///   recursively, each indented one `depth` level deeper.
/// - **Cycles** and repeated visits are guarded: every id is emitted at most
///   once, so a `parent_id` chain that loops can't spin or duplicate rows.
///
/// Pure and allocation-simple so it can be unit-tested without a live session.
pub fn build_worker_forest(rows: Vec<WorkerRow>) -> Vec<WorkerRow> {
    use std::collections::{HashMap, HashSet};

    let present: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
    // parent id -> child indices, preserving input order.
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match &r.parent_id {
            Some(p) if present.contains(p) && p != &r.id => {
                children.entry(p.clone()).or_default().push(i);
            }
            // None, unknown parent, or self-parent → root.
            _ => roots.push(i),
        }
    }

    let mut out: Vec<WorkerRow> = Vec::with_capacity(rows.len());
    let mut emitted: HashSet<usize> = HashSet::new();
    // Explicit stack of (row index, depth) for pre-order DFS without recursion.
    // Push roots in reverse so the stack pops them in input (newest-first) order.
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&i| (i, 0usize)).collect();
    while let Some((i, depth)) = stack.pop() {
        if !emitted.insert(i) {
            continue; // cycle / already emitted — skip.
        }
        let mut row = rows[i].clone();
        row.depth = depth;
        let id = row.id.clone();
        out.push(row);
        if let Some(kids) = children.get(&id) {
            for &c in kids.iter().rev() {
                if !emitted.contains(&c) {
                    stack.push((c, depth + 1));
                }
            }
        }
    }
    // Any row never reached (part of a pure cycle with no root entry) is
    // appended as a depth-0 root so nothing silently vanishes.
    for (i, r) in rows.into_iter().enumerate() {
        if !emitted.contains(&i) {
            let mut row = r;
            row.depth = 0;
            out.push(row);
        }
    }
    out
}

/// Feed one read-chunk of raw tty bytes through the CSI state machine, returning
/// the carry-over `state` and every complete [`Key`] the chunk produced.
///
/// State: `0` ground, `1` saw ESC, `2` saw `ESC [`, `3` saw `ESC [ 3`.
///
/// A lone ESC that ends a chunk leaves `state == 1`; the read loop disambiguates
/// it from a CSI prefix with a short poll timeout ([`pending_esc_dismiss`]).
/// A `ESC` immediately followed by a non-`[` byte is treated as a real Escape
/// (emits [`Key::Dismiss`]) and the trailing byte is re-processed from ground —
/// so `ESC x` dismisses without waiting.
pub fn parse_modal_keys(mut state: u8, bytes: &[u8]) -> (u8, Vec<Key>) {
    let mut keys = Vec::new();
    for &b in bytes {
        loop {
            match state {
                1 => {
                    // Saw ESC. `[` opens a CSI; anything else means the ESC was a
                    // bare Escape → dismiss, then re-handle this byte from ground.
                    if b == b'[' {
                        state = 2;
                        break;
                    }
                    keys.push(Key::Dismiss);
                    state = 0;
                    continue;
                }
                2 => {
                    // Saw `ESC [`.
                    match b {
                        b'A' => keys.push(Key::Up),
                        b'B' => keys.push(Key::Down),
                        b'3' => {
                            state = 3;
                            break;
                        }
                        _ => {} // arrows C/D, back-tab Z, etc. — ignored.
                    }
                    state = 0;
                    break;
                }
                3 => {
                    // Saw `ESC [ 3` — `~` completes the Delete/forward-delete key.
                    if b == b'~' {
                        keys.push(Key::Delete);
                    }
                    state = 0;
                    break;
                }
                _ => {
                    // Ground.
                    match b {
                        0x1b => state = 1,
                        b'\r' | b'\n' => keys.push(Key::Enter),
                        0x7f => keys.push(Key::Delete),
                        b'd' => keys.push(Key::Delete),
                        b'j' => keys.push(Key::Down),
                        b'k' => keys.push(Key::Up),
                        b'q' => keys.push(Key::Dismiss),
                        _ => {}
                    }
                    break;
                }
            }
        }
    }
    (state, keys)
}

/// After a read left a dangling ESC (`state == 1`) and a follow-up `poll` timed
/// out with no further bytes, the ESC was a real Escape keypress. Returns the
/// dismiss key and resets state to ground. Pure companion to the poll idiom.
pub fn pending_esc_dismiss(state: &mut u8) -> Option<Key> {
    if *state == 1 {
        *state = 0;
        Some(Key::Dismiss)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Terminal / raw-mode plumbing (TTY only; not unit-tested)
// ---------------------------------------------------------------------------

/// RAII cbreak guard: on construct, flip fd 0 into cbreak (ICANON+ECHO off,
/// ISIG kept so Ctrl-C still fires) and hide the cursor; on drop, restore the
/// saved cooked termios and show the cursor. Guaranteed even on panic/early
/// return so the tty is never left wedged.
struct RawGuard {
    cooked: libc::termios,
}

impl RawGuard {
    fn install() -> Option<RawGuard> {
        // SAFETY: tcgetattr into a zeroed termios; checked return code.
        let cooked = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            t
        };
        let mut raw = cooked;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: tcsetattr on fd 0 with a valid termios.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &raw);
        }
        // Hide cursor for a clean redraw.
        let _ = write!(io::stdout(), "\x1b[?25l");
        let _ = io::stdout().flush();
        Some(RawGuard { cooked })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // SAFETY: restore the saved cooked termios on fd 0.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.cooked);
        }
        // Show cursor again and land on a fresh line.
        let _ = write!(io::stdout(), "\x1b[?25h\r\n");
        let _ = io::stdout().flush();
    }
}

/// `poll(fd0, timeout_ms)` → true when a byte is readable before the timeout.
fn poll_readable(timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll on a single fd.
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 }
}

/// Block for the next parsed [`Key`], draining a small queue first, then
/// reading from the tty. Disambiguates a lone ESC from a CSI prefix with a 40ms
/// poll (the keywatch idiom). Returns `None` only on a hard read error.
fn read_key(state: &mut u8, queue: &mut VecDeque<Key>) -> Option<Key> {
    if let Some(k) = queue.pop_front() {
        return Some(k);
    }
    loop {
        // Block in ~1s slices so a dangling ESC still resolves promptly.
        if !poll_readable(1000) {
            if let Some(k) = pending_esc_dismiss(state) {
                return Some(k);
            }
            continue;
        }
        let mut buf = [0u8; 64];
        // SAFETY: read into a stack buffer; n bounded by buf.len().
        let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return None;
        }
        let (ns, keys) = parse_modal_keys(*state, &buf[..n as usize]);
        *state = ns;
        for k in keys {
            queue.push_back(k);
        }
        if let Some(k) = queue.pop_front() {
            return Some(k);
        }
        // No complete key yet. A dangling ESC (state 1): poll briefly — if
        // nothing follows it's a real Escape, else loop to read the CSI tail.
        if *state == 1 && !poll_readable(40) {
            if let Some(k) = pending_esc_dismiss(state) {
                return Some(k);
            }
        }
    }
}

/// Visible width of a string ignoring the few ANSI SGR sequences we emit — used
/// for column padding. Good enough for our controlled cell content (it skips
/// `ESC [ … m`).
fn display_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch == 'm' {
                in_esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_esc = true;
            continue;
        }
        w += 1;
    }
    w
}

fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Draw the modal as a bottom-anchored "tray". When a footer scroll region is
/// live the tray's BOTTOM line is pinned to the last scrolling body row
/// (`rows - FOOTER_ROWS`) — directly above the footer's horizontal rule — using
/// absolute cursor addressing wrapped in DECSC/DECRC so the operator's prompt
/// cursor is left untouched. `prev_lines` is the row count painted by the
/// previous call, used to erase stale rows when the tray shrinks. Without a
/// footer region (piped stdout / terminal too short) it falls back to the
/// legacy in-place redraw. Returns the number of rows actually painted.
fn render(rows: &[WorkerRow], sel: usize, prev_lines: usize) -> usize {
    let color = crate::style::colors_enabled();

    // Column widths from the id/status/runtime cells (task/result flow at the end).
    let id_w = rows
        .iter()
        .map(|r| display_width(&r.id_cell))
        .chain(std::iter::once(6)) // "Worker"
        .max()
        .unwrap_or(6);
    let st_w = rows
        .iter()
        .map(|r| display_width(&r.status))
        .chain(std::iter::once(6)) // "Status"
        .max()
        .unwrap_or(6);
    let rt_w = rows
        .iter()
        .map(|r| display_width(&r.runtime_cell))
        .chain(std::iter::once(7))
        .max()
        .unwrap_or(7);

    let mut lines: Vec<String> = Vec::new();
    // Title.
    lines.push(if color {
        format!("\x1b[1m:workers\x1b[0m \x1b[2m({} live)\x1b[0m", rows.len())
    } else {
        format!(":workers ({} live)", rows.len())
    });
    // Column header.
    let header = format!(
        "  {}  {}  {}  {}  {}",
        pad("", 2), // type-glyph column (🤖/🎯/⏰ render 2 cells wide)
        pad("Worker", id_w),
        pad("Status", st_w + 2),
        pad("Runtime", rt_w),
        "Task"
    );
    lines.push(if color {
        format!("\x1b[2m{header}\x1b[0m")
    } else {
        header
    });

    for (i, r) in rows.iter().enumerate() {
        let selected = i == sel;
        // Mark the active row with a `>` indicator in the gutter instead of
        // inverse-video highlighting the whole row. The two-column gutter keeps
        // every row aligned; when color is on the caret is bold cyan so it's
        // easy to spot, and it stays a plain `>` when piped / --no-color.
        let gutter = if selected {
            if color { "\x1b[1;36m>\x1b[0m " } else { "> " }
        } else {
            "  "
        };
        // Indent nested subworkers under their parent so the forest reads as a
        // tree; roots (depth 0) are flush. A `└ ` elbow marks each child.
        let task = if r.depth > 0 {
            let indent = "  ".repeat(r.depth);
            let budget = 60usize.saturating_sub(indent.chars().count() + 2);
            format!("{indent}└ {}", clip(&r.task, budget))
        } else {
            clip(&r.task, 60)
        };
        // Type glyph rides at the front (unpadded — every glyph is 2 cells, so
        // the data columns stay aligned with the blank 2-wide header cell).
        let body = format!(
            "{}{}  {}  {}  {}  {}",
            gutter,
            r.type_emoji,
            pad(&r.id_cell, id_w),
            pad(&crate::style::styled_status(&r.status), st_w + 2),
            pad(&r.runtime_cell, rt_w),
            task
        );
        lines.push(body);
    }

    // Footer hint.
    lines.push(if color {
        "\x1b[2m  ↑/↓ move · Enter attach · Del/d close · Esc/q dismiss\x1b[0m".to_string()
    } else {
        "  ↑/↓ move · Enter attach · Del/d close · Esc/q dismiss".to_string()
    });

    let n = lines.len();
    let mut out = String::new();
    if let (true, Some(total)) =
        (crate::terminal::footer_active(), crate::terminal::screen_rows())
    {
        // Last scrolling body row = row directly above the footer rule.
        let body_bottom = total.saturating_sub(crate::terminal::FOOTER_ROWS).max(1);
        let avail = body_bottom as usize;
        // If the tray is taller than the body area, keep the BOTTOM `avail`
        // lines so it never writes into (and corrupts) the footer rows.
        let start = n.saturating_sub(avail);
        let visible = &lines[start..];
        let vn = visible.len() as u16;
        let top = body_bottom - vn + 1; // vn <= avail ⇒ top >= 1
        out.push_str("\x1b7"); // DECSC — save the caller's cursor
        // A shorter repaint (a worker closed) leaves stale rows above the new
        // top — clear them so the tray shrinks cleanly from the top.
        if prev_lines > vn as usize {
            let prev_top = body_bottom
                .saturating_sub(prev_lines as u16 - 1)
                .max(1);
            for row in prev_top..top {
                out.push_str(&format!("\x1b[{row};1H\x1b[2K"));
            }
        }
        for (i, line) in visible.iter().enumerate() {
            let row = top + i as u16;
            out.push_str(&format!("\x1b[{row};1H\x1b[2K"));
            out.push_str(line);
        }
        out.push_str("\x1b8"); // DECRC — restore the caller's cursor
        let _ = write!(io::stdout(), "{out}");
        let _ = io::stdout().flush();
        return vn as usize;
    }
    // Legacy in-place redraw (no footer region).
    if prev_lines > 0 {
        out.push_str(&format!("\x1b[{prev_lines}A"));
    }
    for line in &lines {
        out.push_str("\x1b[2K");
        out.push_str(line);
        out.push_str("\r\n");
    }
    let _ = write!(io::stdout(), "{out}");
    let _ = io::stdout().flush();
    n
}

/// Erase the bottom-anchored tray band on exit and leave the cursor on the last
/// body row so the RawGuard's trailing CRLF and the next prompt land just above
/// the footer rule. No-op without a footer region (the legacy path leaves its
/// in-place block in scrollback, matching the pre-tray behavior).
fn clear_tray(prev_lines: usize) {
    if prev_lines == 0 || !crate::terminal::footer_active() {
        return;
    }
    let Some(total) = crate::terminal::screen_rows() else {
        return;
    };
    let body_bottom = total.saturating_sub(crate::terminal::FOOTER_ROWS).max(1);
    let top = body_bottom.saturating_sub(prev_lines as u16 - 1).max(1);
    let mut out = String::new();
    for row in top..=body_bottom {
        out.push_str(&format!("\x1b[{row};1H\x1b[2K"));
    }
    out.push_str(&format!("\x1b[{body_bottom};1H"));
    let _ = write!(io::stdout(), "{out}");
    let _ = io::stdout().flush();
}

/// Clip to `max` display columns with an ellipsis.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

/// Run the interactive picker over `rows`. Enters cbreak, renders, and drives
/// the selection until the operator picks an action. Returns the chosen
/// [`ModalAction`]. If raw mode can't be entered, returns `Dismiss` (the caller
/// then falls back to the static table).
///
/// `initial_sel` seeds the highlighted row (clamped) so re-entry after a close
/// keeps roughly the same position.
pub fn run(rows: &[WorkerRow], initial_sel: usize) -> ModalAction {
    if rows.is_empty() {
        return ModalAction::Dismiss;
    }
    let Some(_guard) = RawGuard::install() else {
        return ModalAction::Dismiss;
    };
    let mut sel = initial_sel.min(rows.len() - 1);
    let mut state: u8 = 0;
    let mut queue: VecDeque<Key> = VecDeque::new();
    let mut prev_lines = 0usize;
    let action = loop {
        prev_lines = render(rows, sel, prev_lines);
        let Some(key) = read_key(&mut state, &mut queue) else {
            break ModalAction::Dismiss;
        };
        match key {
            Key::Up | Key::Down => sel = move_selection(sel, rows.len(), key),
            Key::Enter => break ModalAction::Attach(rows[sel].id.clone()),
            Key::Delete => break ModalAction::Close(rows[sel].id.clone()),
            Key::Dismiss => break ModalAction::Dismiss,
        }
    };
    // Erase the anchored tray band so the popup closes cleanly instead of
    // leaving a stale block covering the row above the rule.
    clear_tray(prev_lines);
    action
    // `_guard` drops here → cooked mode restored, cursor shown.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_up_down() {
        let (s, keys) = parse_modal_keys(0, b"\x1b[A\x1b[B");
        assert_eq!(s, 0);
        assert_eq!(keys, vec![Key::Up, Key::Down]);
    }

    #[test]
    fn vim_keys() {
        let (_, keys) = parse_modal_keys(0, b"kjkj");
        assert_eq!(keys, vec![Key::Up, Key::Down, Key::Up, Key::Down]);
    }

    #[test]
    fn enter_variants() {
        let (_, keys) = parse_modal_keys(0, b"\r\n");
        assert_eq!(keys, vec![Key::Enter, Key::Enter]);
    }

    #[test]
    fn delete_variants() {
        // DEL byte, `d`, and the CSI `ESC [ 3 ~` forward-delete all → Delete.
        let (_, keys) = parse_modal_keys(0, b"\x7fd\x1b[3~");
        assert_eq!(keys, vec![Key::Delete, Key::Delete, Key::Delete]);
    }

    #[test]
    fn q_dismisses() {
        let (_, keys) = parse_modal_keys(0, b"q");
        assert_eq!(keys, vec![Key::Dismiss]);
    }

    #[test]
    fn esc_then_char_dismisses_and_reprocesses() {
        // ESC followed by a non-`[` byte: the ESC is a real Escape (Dismiss) and
        // the trailing `j` is handled as Down.
        let (s, keys) = parse_modal_keys(0, b"\x1bj");
        assert_eq!(s, 0);
        assert_eq!(keys, vec![Key::Dismiss, Key::Down]);
    }

    #[test]
    fn lone_esc_leaves_pending_state() {
        // A lone ESC ending a chunk yields no key and a pending state; the poll
        // idiom then converts it to a Dismiss.
        let (mut s, keys) = parse_modal_keys(0, b"\x1b");
        assert_eq!(s, 1);
        assert!(keys.is_empty());
        assert_eq!(pending_esc_dismiss(&mut s), Some(Key::Dismiss));
        assert_eq!(s, 0);
        assert_eq!(pending_esc_dismiss(&mut s), None);
    }

    #[test]
    fn csi_fragmented_across_reads() {
        // `ESC` in one read, `[` in the next, `A` in a third → one Up.
        let (s1, k1) = parse_modal_keys(0, b"\x1b");
        assert_eq!((s1, k1.len()), (1, 0));
        let (s2, k2) = parse_modal_keys(s1, b"[");
        assert_eq!((s2, k2.len()), (2, 0));
        let (s3, k3) = parse_modal_keys(s2, b"A");
        assert_eq!(s3, 0);
        assert_eq!(k3, vec![Key::Up]);
    }

    #[test]
    fn double_esc_first_dismisses() {
        // ESC ESC: the first ESC is a bare Escape (Dismiss); the second re-arms.
        let (s, keys) = parse_modal_keys(0, b"\x1b\x1b");
        assert_eq!(s, 1);
        assert_eq!(keys, vec![Key::Dismiss]);
    }

    #[test]
    fn arrow_left_right_ignored() {
        // `ESC [ C` / `ESC [ D` (right/left) are not modal keys.
        let (s, keys) = parse_modal_keys(0, b"\x1b[C\x1b[D");
        assert_eq!(s, 0);
        assert!(keys.is_empty());
    }

    #[test]
    fn plain_text_ignored() {
        // Filler with no command bytes (avoids j/k/d/q which are live shortcuts).
        let (_, keys) = parse_modal_keys(0, b"abc xyz");
        assert!(keys.is_empty());
    }

    #[test]
    fn selection_saturates_at_bounds() {
        assert_eq!(move_selection(0, 3, Key::Up), 0); // clamp low
        assert_eq!(move_selection(2, 3, Key::Down), 2); // clamp high
        assert_eq!(move_selection(1, 3, Key::Up), 0);
        assert_eq!(move_selection(1, 3, Key::Down), 2);
    }

    #[test]
    fn selection_empty_list_pins_zero() {
        assert_eq!(move_selection(0, 0, Key::Down), 0);
        assert_eq!(move_selection(5, 0, Key::Up), 0);
    }

    #[test]
    fn display_width_strips_ansi() {
        assert_eq!(display_width("\x1b[1mhi\x1b[0m"), 2);
        assert_eq!(display_width("plain"), 5);
    }

    #[test]
    fn clip_adds_ellipsis() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }

    /// Minimal [`WorkerRow`] for forest tests — only `id`/`parent_id` matter to
    /// [`build_worker_forest`]; every other cell is a placeholder.
    fn wr(id: &str, parent: Option<&str>) -> WorkerRow {
        WorkerRow {
            id: id.into(),
            type_emoji: "🤖".into(),
            id_cell: id.into(),
            session_label: "—".into(),
            status: "running".into(),
            started_cell: "—".into(),
            runtime_cell: "—".into(),
            task: id.into(),
            result_cell: "—".into(),
            parent_id: parent.map(|p| p.into()),
            depth: 0,
        }
    }

    fn ids_depths(rows: &[WorkerRow]) -> Vec<(String, usize)> {
        rows.iter().map(|r| (r.id.clone(), r.depth)).collect()
    }

    #[test]
    fn forest_roots_only_preserve_input_order() {
        // No parents → every row is a depth-0 root, order preserved.
        let out = build_worker_forest(vec![wr("a", None), wr("b", None), wr("c", None)]);
        assert_eq!(
            ids_depths(&out),
            vec![("a".into(), 0), ("b".into(), 0), ("c".into(), 0)]
        );
    }

    #[test]
    fn forest_nests_children_under_parent_preorder() {
        // a ← b ← c (grandchild) and a ← d; root e. Pre-order with depth.
        let out = build_worker_forest(vec![
            wr("a", None),
            wr("e", None),
            wr("b", Some("a")),
            wr("c", Some("b")),
            wr("d", Some("a")),
        ]);
        assert_eq!(
            ids_depths(&out),
            vec![
                ("a".into(), 0),
                ("b".into(), 1),
                ("c".into(), 2),
                ("d".into(), 1),
                ("e".into(), 0),
            ]
        );
    }

    #[test]
    fn forest_unknown_parent_degrades_to_root() {
        // Parent id not present in the set → the child renders as a root.
        let out = build_worker_forest(vec![wr("x", Some("ghost"))]);
        assert_eq!(ids_depths(&out), vec![("x".into(), 0)]);
    }

    #[test]
    fn forest_cycle_is_guarded_no_spin_no_dup() {
        // a↔b mutual parents (pure cycle, no root entry): each emitted once,
        // appended as depth-0 roots rather than spinning or duplicating.
        let out = build_worker_forest(vec![wr("a", Some("b")), wr("b", Some("a"))]);
        assert_eq!(out.len(), 2);
        let mut ids: Vec<_> = out.iter().map(|r| r.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn forest_self_parent_is_root() {
        // A row that lists itself as parent must not nest under itself.
        let out = build_worker_forest(vec![wr("s", Some("s"))]);
        assert_eq!(ids_depths(&out), vec![("s".into(), 0)]);
    }
}
