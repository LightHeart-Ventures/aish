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
    /// PgUp / `Ctrl-b`-style jump — moves a whole viewport page up.
    PageUp,
    /// PgDn — moves a whole viewport page down.
    PageDown,
    /// `Home` / `g` — jump to the first row.
    Home,
    /// `End` / `G` — jump to the last row.
    End,
    Enter,
    Delete,
    Dismiss,
}

/// Rows a PgUp/PgDn jump covers when the caller doesn't know the live viewport
/// height (the default used by [`move_selection`]).
pub const DEFAULT_PAGE: usize = 10;

/// Move `sel` within `[0, len)` for a key, saturating at both ends (no wrap —
/// matches the spec). Pure so the clamp logic is unit-tested. `len == 0` pins 0.
/// Paging uses [`DEFAULT_PAGE`]; pass a live viewport height to
/// [`move_selection_page`] to page by exactly one screenful.
pub fn move_selection(sel: usize, len: usize, key: Key) -> usize {
    move_selection_page(sel, len, key, DEFAULT_PAGE)
}

/// [`move_selection`] with an explicit `page` height for PgUp/PgDn, so the modal
/// can page by the rows the viewport is actually showing. `page == 0` is treated
/// as 1 (a page jump always moves at least one row). Pure → unit-tested.
pub fn move_selection_page(sel: usize, len: usize, key: Key, page: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len - 1;
    let page = page.max(1);
    match key {
        Key::Up => sel.saturating_sub(1),
        Key::Down => (sel + 1).min(last),
        Key::PageUp => sel.saturating_sub(page),
        Key::PageDown => (sel + page).min(last),
        Key::Home => 0,
        Key::End => last,
        _ => sel.min(last),
    }
}

/// Chrome rows the tray spends on non-row content: title + column header +
/// key-hint footer. Subtracted from the available band to size the row viewport.
pub const CHROME_ROWS: usize = 3;

/// Max lines the SELECTED row's task cell may occupy (1 opening + up to 2
/// continuation lines) before it is ellipsized. Unselected rows stay one line.
pub const MAX_TASK_LINES: usize = 3;

/// Pick the window of rows to paint: returns `(first, count)` such that `sel` is
/// always inside `[first, first + count)` and `count <= avail`.
///
/// The window is **centered** on the selection (then clamped to the ends), which
/// is stateless — no scroll offset to carry between redraws — and guarantees the
/// selected row is on screen, which is the bug the old "keep the bottom N lines"
/// crop had: paging past the fold scrolled the selection out of view, and the
/// title/header were the first rows sacrificed. Pure → unit-tested.
pub fn viewport(len: usize, sel: usize, avail: usize) -> (usize, usize) {
    if len == 0 || avail == 0 {
        return (0, 0);
    }
    if len <= avail {
        return (0, len);
    }
    let sel = sel.min(len - 1);
    let first = sel.saturating_sub(avail / 2).min(len.saturating_sub(avail));
    (first, avail)
}

/// Word-wrap `text` into at most `max_lines` lines of `width` display columns,
/// hard-splitting any single word longer than the width and ellipsizing the last
/// line when the text still overflows. Returns at least one line for non-empty
/// input. Pure → unit-tested; used to let the selected row's task breathe over a
/// few lines instead of being hard-clipped at the column edge.
pub fn wrap_cell(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let ww = word.chars().count();
        if ww > width {
            // Hard-split an over-long token (a URL, a path) across lines.
            if cur_w > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            for ch in word.chars() {
                if cur_w == width {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += 1;
            }
            continue;
        }
        let need = if cur_w == 0 { ww } else { cur_w + 1 + ww };
        if need > width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if cur_w > 0 {
            cur.push(' ');
            cur_w += 1;
        }
        cur.push_str(word);
        cur_w += ww;
    }
    if cur_w > 0 || lines.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines {
        let overflow_tail = lines[max_lines - 1].clone();
        lines.truncate(max_lines);
        lines[max_lines - 1] = clip(&overflow_tail, width);
        // Mark truncation even when the kept tail happened to fit exactly.
        if !lines[max_lines - 1].ends_with('…') {
            lines[max_lines - 1] = clip(&format!("{overflow_tail} …"), width);
        }
    }
    lines
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
/// State: `0` ground, `1` saw ESC, `2` saw `ESC [`, `3` saw `ESC [ 3`,
/// `4` saw `ESC [ 5` (PgUp), `5` saw `ESC [ 6` (PgDn).
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
                        b'H' => keys.push(Key::Home), // CSI H — Home
                        b'F' => keys.push(Key::End),  // CSI F — End
                        b'3' => {
                            state = 3;
                            break;
                        }
                        b'5' => {
                            state = 4;
                            break;
                        }
                        b'6' => {
                            state = 5;
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
                4 => {
                    // Saw `ESC [ 5` — `~` completes PageUp.
                    if b == b'~' {
                        keys.push(Key::PageUp);
                    }
                    state = 0;
                    break;
                }
                5 => {
                    // Saw `ESC [ 6` — `~` completes PageDown.
                    if b == b'~' {
                        keys.push(Key::PageDown);
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
                        b'g' => keys.push(Key::Home),
                        b'G' => keys.push(Key::End),
                        0x02 => keys.push(Key::PageUp),   // Ctrl-b
                        0x06 => keys.push(Key::PageDown), // Ctrl-f
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
        if *state == 1
            && !poll_readable(40)
            && let Some(k) = pending_esc_dismiss(state)
        {
            return Some(k);
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

    // Width of the Task cell. DERIVED from this terminal rather than the former
    // hardcoded 60: `activity_summary::current_budget()` subtracts this row's
    // real chrome (`WORKERS_ROW_CHROME`, kept in sync with the format string
    // below) from the live width and clamps it, which is the SAME budget the
    // summarizer was told to write to — so a summary that fit when it was
    // generated also fits when it is painted, and a narrow terminal shrinks the
    // cell instead of wrapping the row.
    let task_w = crate::activity_summary::current_budget();

    // Columns of chrome painted before the Task cell (gutter + type glyph + the
    // three padded data columns and their 2-space separators). Continuation
    // lines of a wrapped task indent to exactly here so they sit under the Task
    // column instead of restarting at the left margin. Kept in sync with the
    // `body` format string below.
    let prefix_w = 2 + 2 + 2 + id_w + 2 + (st_w + 2) + 2 + rt_w + 2;

    // Display budget for the task cell at a given nesting depth (the `  …└ `
    // elbow eats from the cell, and we never shrink below a readable floor).
    let task_budget = |depth: usize| -> usize {
        if depth > 0 {
            task_w.saturating_sub(2 * depth + 2).max(8)
        } else {
            task_w.max(8)
        }
    };

    // Rows the tray may paint. With a footer scroll region live the band is the
    // body area above the rule; the chrome rows and the selected row's extra
    // wrap lines are reserved FIRST so the tray always fits instead of being
    // bottom-cropped (which used to eat the title/header and could scroll the
    // selection off screen). Unknown band (piped / legacy redraw) ⇒ show all.
    let band = match (
        crate::terminal::footer_active(),
        crate::terminal::screen_rows(),
    ) {
        (true, Some(total)) => {
            Some(total.saturating_sub(crate::terminal::FOOTER_ROWS).max(1) as usize)
        }
        _ => None,
    };
    let sel_extra = rows
        .get(sel)
        .map(|r| wrap_cell(&r.task, task_budget(r.depth), MAX_TASK_LINES).len())
        .unwrap_or(1)
        .saturating_sub(1);
    let avail_rows = band
        .map(|b| b.saturating_sub(CHROME_ROWS + sel_extra).max(1))
        .unwrap_or_else(|| rows.len());
    let (first, count) = viewport(rows.len(), sel, avail_rows);
    let hidden_above = first;
    let hidden_below = rows.len().saturating_sub(first + count);

    let mut lines: Vec<String> = Vec::new();
    // Title — carries ▲/▼ counts when rows are scrolled out of the viewport so
    // the operator knows the list continues past the window.
    let mut scroll = String::new();
    if hidden_above > 0 {
        scroll.push_str(&format!(" · ▲{hidden_above}"));
    }
    if hidden_below > 0 {
        scroll.push_str(&format!(" · ▼{hidden_below}"));
    }
    lines.push(if color {
        format!(
            "\x1b[1m:workers\x1b[0m \x1b[2m({} live{scroll})\x1b[0m",
            rows.len()
        )
    } else {
        format!(":workers ({} live{scroll})", rows.len())
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

    for (i, r) in rows.iter().enumerate().skip(first).take(count) {
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
        let indent = if r.depth > 0 {
            format!("{}└ ", "  ".repeat(r.depth))
        } else {
            String::new()
        };
        let budget = task_budget(r.depth);
        // The SELECTED row's task WRAPS over up to `MAX_TASK_LINES` lines so the
        // operator can read the whole summary of the row they're on; every other
        // row stays a single clipped line (dense list, readable focus).
        let wrapped: Vec<String> = if selected {
            wrap_cell(&r.task, budget, MAX_TASK_LINES)
        } else {
            vec![clip(&r.task, budget)]
        };
        let task = format!(
            "{indent}{}",
            wrapped.first().map(String::as_str).unwrap_or("")
        );
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
        // Continuation lines of a wrapped (selected) task, indented under the
        // Task column so the cell reads as one block.
        for cont in wrapped.iter().skip(1) {
            lines.push(format!(
                "{}{}{cont}",
                " ".repeat(prefix_w),
                " ".repeat(indent.chars().count())
            ));
        }
    }

    // Footer hint.
    let hint =
        "  ↑/↓ move · PgUp/PgDn page · g/G top/end · Enter attach · Del/d close · Esc/q dismiss";
    lines.push(if color {
        format!("\x1b[2m{hint}\x1b[0m")
    } else {
        hint.to_string()
    });

    let n = lines.len();
    let mut out = String::new();
    if let (true, Some(total)) = (
        crate::terminal::footer_active(),
        crate::terminal::screen_rows(),
    ) {
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
            let prev_top = body_bottom.saturating_sub(prev_lines as u16 - 1).max(1);
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
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
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
            Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End => {
                sel = move_selection(sel, rows.len(), key)
            }
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

    // -----------------------------------------------------------------------
    // Viewport scrolling + task wrapping (TUI item 4)
    // -----------------------------------------------------------------------

    #[test]
    fn viewport_shows_everything_when_it_fits() {
        assert_eq!(viewport(4, 0, 10), (0, 4));
        assert_eq!(viewport(4, 3, 4), (0, 4));
    }

    #[test]
    fn viewport_degenerate_inputs() {
        assert_eq!(viewport(0, 0, 5), (0, 0));
        assert_eq!(viewport(5, 0, 0), (0, 0));
    }

    #[test]
    fn viewport_keeps_selection_visible_at_both_ends() {
        // 20 rows, 5-row window: selection is ALWAYS inside the returned window,
        // which is the regression the old bottom-crop had.
        for sel in 0..20 {
            let (first, count) = viewport(20, sel, 5);
            assert_eq!(count, 5, "sel={sel}");
            assert!(
                sel >= first && sel < first + count,
                "sel={sel} window={first}..{}",
                first + count
            );
            assert!(first + count <= 20);
        }
    }

    #[test]
    fn viewport_clamps_to_list_ends() {
        assert_eq!(viewport(20, 0, 5), (0, 5)); // top
        assert_eq!(viewport(20, 19, 5), (15, 5)); // bottom
        assert_eq!(viewport(20, 10, 5), (8, 5)); // centered
    }

    #[test]
    fn wrap_cell_wraps_on_word_boundaries() {
        let out = wrap_cell("run a security audit across services", 12, 3);
        assert!(out.iter().all(|l| l.chars().count() <= 12), "{out:?}");
        assert_eq!(out[0], "run a");
        assert!(out.len() > 1);
    }

    #[test]
    fn wrap_cell_respects_max_lines_and_marks_truncation() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let out = wrap_cell(text, 10, 2);
        assert_eq!(out.len(), 2);
        assert!(out[1].ends_with('…'), "{out:?}");
        assert!(out.iter().all(|l| l.chars().count() <= 10));
    }

    #[test]
    fn wrap_cell_hard_splits_long_token() {
        let out = wrap_cell("https://example.com/a/very/long/path", 10, 4);
        assert!(out.len() > 1);
        assert!(out.iter().all(|l| l.chars().count() <= 10), "{out:?}");
    }

    #[test]
    fn wrap_cell_degenerate_inputs() {
        assert!(wrap_cell("x", 0, 3).is_empty());
        assert!(wrap_cell("x", 10, 0).is_empty());
        assert_eq!(wrap_cell("", 10, 3), vec![String::new()]);
    }

    #[test]
    fn page_keys_parse_from_csi() {
        let (s, keys) = parse_modal_keys(0, b"\x1b[5~\x1b[6~");
        assert_eq!(s, 0);
        assert_eq!(keys, vec![Key::PageUp, Key::PageDown]);
    }

    #[test]
    fn home_end_parse_from_csi_and_vim() {
        let (_, keys) = parse_modal_keys(0, b"\x1b[H\x1b[F");
        assert_eq!(keys, vec![Key::Home, Key::End]);
        let (_, vim) = parse_modal_keys(0, b"gG");
        assert_eq!(vim, vec![Key::Home, Key::End]);
    }

    #[test]
    fn page_csi_fragmented_across_reads() {
        let (s1, k1) = parse_modal_keys(0, b"\x1b[5");
        assert!(k1.is_empty());
        let (s2, k2) = parse_modal_keys(s1, b"~");
        assert_eq!(s2, 0);
        assert_eq!(k2, vec![Key::PageUp]);
    }

    #[test]
    fn page_selection_jumps_by_page_and_clamps() {
        assert_eq!(move_selection_page(0, 20, Key::PageDown, 5), 5);
        assert_eq!(move_selection_page(18, 20, Key::PageDown, 5), 19);
        assert_eq!(move_selection_page(3, 20, Key::PageUp, 5), 0);
        assert_eq!(move_selection_page(12, 20, Key::PageUp, 5), 7);
        // page 0 still advances one row
        assert_eq!(move_selection_page(0, 20, Key::PageDown, 0), 1);
    }

    #[test]
    fn home_end_selection_jumps_to_bounds() {
        assert_eq!(move_selection(7, 20, Key::Home), 0);
        assert_eq!(move_selection(7, 20, Key::End), 19);
        assert_eq!(move_selection(7, 0, Key::End), 0);
    }
}
