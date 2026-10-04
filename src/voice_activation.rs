//! Voice **activation word** matching and **voice-mode toggle** state.
//!
//! aish's voice input (FR-334 / SPR-068) shipped as a *one-shot* capture:
//! Ctrl-S recorded exactly one utterance, transcribed it, pre-filled the
//! buffer, and that was the whole interaction. This module backs the model
//! operators actually asked for:
//!
//! * **Toggle** — Ctrl-S flips voice mode ON/OFF ([`VoiceMode`]). While ON the
//!   REPL keeps listening, utterance after utterance, until it is flipped OFF.
//! * **Activation word** — while listening, only an utterance that *begins*
//!   with the activation word (default `aish`) is treated as a command. Every
//!   other utterance is ignored, so ordinary room conversation can never run
//!   anything ([`match_activation`]).
//!
//! ## Why this module is NOT behind `--features voice`
//!
//! The audio/STT plumbing in [`crate::voice`] is feature-gated because it pulls
//! `cpal` + `whisper-rs` (heavy native deps). The *decision* logic here has no
//! dependencies at all, and gating it would mean the canonical CI gate
//! (`cargo test --no-default-features --locked`) never compiles — let alone
//! tests — the part most likely to be wrong. So everything decision-shaped
//! lives here, ungated and unit-tested; `crate::voice::activation` re-exports
//! it so voice builds still have a single obvious path to it.

// Consumers of this logic all live behind `--features voice`; in a voice-less
// build everything here is legitimately unreferenced (but still compiled and
// unit-tested, which is the whole point of keeping it ungated).
#![cfg_attr(not(feature = "voice"), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Default activation word. Overridable with `voice.activation_word` in
/// `~/.aish/config` (see [`crate::voice::config`]).
pub const DEFAULT_ACTIVATION_WORD: &str = "aish";

/// How many consecutive empty/silent captures before voice mode disables
/// itself. Prevents an unattended terminal from holding the microphone open
/// forever after the operator walks away.
pub const MAX_IDLE_CAPTURES: u32 = 20;

/// What an utterance turned out to be, once the activation word was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// The utterance began with the activation word and carried a command:
    /// the payload is the text AFTER the word, punctuation-trimmed.
    Command(String),
    /// The activation word was spoken with nothing after it ("aish."). Worth a
    /// nudge to the operator rather than silence — the shell *was* addressed.
    Bare,
    /// Not addressed to aish. Ignore it completely: nothing runs, nothing is
    /// echoed into the buffer, voice mode stays ON.
    Ignored,
}

/// Decide whether `transcript` is addressed to aish, and extract the command.
///
/// Matching is deliberately forgiving about how Whisper renders the word but
/// strict about *position*:
///
/// * case-insensitive (`Aish`, `AISH`, `aish`);
/// * leading noise is skipped (`"… aish run tests"`, `"- aish ls"`);
/// * separators *inside* the word are tolerated, because Whisper routinely
///   hyphenates or splits an unfamiliar proper noun (`a-ish`, `a ish`);
/// * the word must end on a word boundary, so `aisha` is NOT an activation;
/// * it must come FIRST. `"I told aish to run tests"` is ignored — per spec,
///   the transcript has to *begin* with the activation word.
///
/// Pure and total: every input maps to exactly one [`Activation`].
pub fn match_activation(transcript: &str, activation_word: &str) -> Activation {
    let word: Vec<char> = activation_word
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();

    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        return Activation::Ignored;
    }
    // An empty activation word disables the gate entirely — every utterance is
    // a command. Defensive only: `VoiceConfig` never stores an empty word.
    if word.is_empty() {
        return Activation::Command(trimmed.to_string());
    }

    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut i = 0usize;
    // Skip leading non-alphanumeric noise ("…", quotes, a stray dash).
    while i < chars.len() && !chars[i].1.is_alphanumeric() {
        i += 1;
    }

    let mut matched = 0usize;
    while i < chars.len() && matched < word.len() {
        let c = chars[i].1;
        if !c.is_alphanumeric() {
            // Tolerate a separator Whisper injected mid-word ("a-ish", "a ish").
            i += 1;
            continue;
        }
        if c.to_ascii_lowercase() != word[matched] {
            return Activation::Ignored;
        }
        matched += 1;
        i += 1;
    }
    if matched < word.len() {
        return Activation::Ignored;
    }
    // Word boundary: "aisha" must not activate.
    if let Some(&(_, c)) = chars.get(i) {
        if c.is_alphanumeric() {
            return Activation::Ignored;
        }
    }

    let rest_at = chars.get(i).map(|(b, _)| *b).unwrap_or(trimmed.len());
    let rest = trimmed[rest_at..]
        .trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, ',' | '.' | ':' | ';' | '!' | '?' | '-' | '—' | '–')
        })
        .trim();
    if rest.is_empty() {
        Activation::Bare
    } else {
        Activation::Command(rest.to_string())
    }
}

/// Shared ON/OFF state for voice mode.
///
/// A cheap `Arc<AtomicBool>` newtype so the line editor (which owns the Ctrl-S
/// keybinding and reports the state) and the REPL's listen loop (which reads it
/// every iteration) look at the SAME bit — two copies of this flag would be a
/// guaranteed desync bug. Cloning shares the state.
#[derive(Debug, Clone, Default)]
pub struct VoiceMode {
    flag: Arc<AtomicBool>,
}

impl VoiceMode {
    /// A fresh, OFF voice mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt an existing shared flag (used to rebuild a `VoiceMode` from the
    /// handle the editor hands out).
    #[allow(dead_code)] // shared-flag plumbing: exercised by tests, kept for callers
    pub fn from_handle(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }

    /// The shared flag, for handing to another owner.
    #[allow(dead_code)] // shared-flag plumbing: exercised by tests, kept for callers
    pub fn handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    /// Is voice mode currently ON?
    pub fn is_on(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Flip the state and return the NEW value (`true` == now listening).
    pub fn toggle(&self) -> bool {
        !self.flag.fetch_xor(true, Ordering::SeqCst)
    }

    /// Force a specific state.
    pub fn set(&self, on: bool) {
        self.flag.store(on, Ordering::SeqCst);
    }

    /// Force OFF (used by the auto-disable and the cancel paths).
    pub fn turn_off(&self) {
        self.set(false);
    }
}

/// The one-line banner printed when voice mode flips. Dim so it reads as
/// chrome, not output.
pub fn banner(on: bool, activation_word: &str) -> String {
    if on {
        format!(
            "\x1b[2m🎤 voice mode ON — say “{activation_word} <command>” · Ctrl-S: off · Esc: skip utterance\x1b[0m"
        )
    } else {
        "\x1b[2m🎤 voice mode OFF\x1b[0m".to_string()
    }
}

/// Should voice mode disable itself after `idle_rounds` consecutive silent
/// captures? Keeps an abandoned terminal from holding the mic open.
pub fn should_auto_disable(idle_rounds: u32) -> bool {
    idle_rounds >= MAX_IDLE_CAPTURES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Activation {
        Activation::Command(s.to_string())
    }

    #[test]
    fn activation_word_extracts_the_command() {
        assert_eq!(
            match_activation("aish list the files", "aish"),
            cmd("list the files")
        );
        // case-insensitive + trailing punctuation around the word
        assert_eq!(
            match_activation("Aish, list the files.", "aish"),
            cmd("list the files.")
        );
        assert_eq!(
            match_activation("AISH run the tests", "aish"),
            cmd("run the tests")
        );
        // surrounding whitespace is irrelevant
        assert_eq!(
            match_activation("   aish   git status  ", "aish"),
            cmd("git status")
        );
    }

    #[test]
    fn activation_tolerates_whisper_mangling_the_word() {
        // Whisper loves to hyphenate or split an unfamiliar proper noun.
        assert_eq!(
            match_activation("a-ish show me the logs", "aish"),
            cmd("show me the logs")
        );
        assert_eq!(
            match_activation("A ish show me the logs", "aish"),
            cmd("show me the logs")
        );
        // leading transcription noise is skipped
        assert_eq!(match_activation("… aish deploy", "aish"), cmd("deploy"));
        assert_eq!(match_activation("- aish deploy", "aish"), cmd("deploy"));
    }

    #[test]
    fn utterances_without_the_activation_word_are_ignored() {
        assert_eq!(match_activation("hello there", "aish"), Activation::Ignored);
        assert_eq!(
            match_activation("list the files", "aish"),
            Activation::Ignored
        );
        // must come FIRST — a mid-sentence mention is still conversation
        assert_eq!(
            match_activation("I told aish to run the tests", "aish"),
            Activation::Ignored
        );
        // word boundary: "aisha" is a name, not an activation
        assert_eq!(
            match_activation("aisha run tests", "aish"),
            Activation::Ignored
        );
        // a near-miss prefix is not enough
        assert_eq!(
            match_activation("ai run tests", "aish"),
            Activation::Ignored
        );
        assert_eq!(
            match_activation("aim ish run tests", "aish"),
            Activation::Ignored
        );
    }

    #[test]
    fn empty_and_bare_utterances() {
        assert_eq!(match_activation("", "aish"), Activation::Ignored);
        assert_eq!(match_activation("   ", "aish"), Activation::Ignored);
        // addressed, but no command carried
        assert_eq!(match_activation("aish", "aish"), Activation::Bare);
        assert_eq!(match_activation("Aish.", "aish"), Activation::Bare);
        assert_eq!(match_activation("aish?", "aish"), Activation::Bare);
    }

    #[test]
    fn activation_word_is_configurable() {
        assert_eq!(
            match_activation("computer open the pod bay", "computer"),
            cmd("open the pod bay")
        );
        // the default word no longer activates once overridden
        assert_eq!(
            match_activation("aish open the pod bay", "computer"),
            Activation::Ignored
        );
        // an empty word disables the gate (defensive — config never stores it)
        assert_eq!(match_activation("just do it", ""), cmd("just do it"));
    }

    #[test]
    fn voice_mode_starts_off_and_toggles() {
        let m = VoiceMode::new();
        assert!(!m.is_on(), "voice mode must start OFF");
        assert!(m.toggle(), "first toggle turns it ON");
        assert!(m.is_on());
        assert!(!m.toggle(), "second toggle turns it OFF");
        assert!(!m.is_on());
    }

    #[test]
    fn voice_mode_clones_share_one_bit() {
        // The editor's copy and the REPL loop's copy MUST be the same bit.
        let editor_side = VoiceMode::new();
        let repl_side = VoiceMode::from_handle(editor_side.handle());
        editor_side.toggle();
        assert!(repl_side.is_on(), "a clone must observe the toggle");
        repl_side.turn_off();
        assert!(
            !editor_side.is_on(),
            "turn_off must be visible to the other side"
        );
    }

    #[test]
    fn auto_disable_only_after_sustained_silence() {
        assert!(!should_auto_disable(0));
        assert!(!should_auto_disable(MAX_IDLE_CAPTURES - 1));
        assert!(should_auto_disable(MAX_IDLE_CAPTURES));
        assert!(should_auto_disable(MAX_IDLE_CAPTURES + 5));
    }

    #[test]
    fn banner_reflects_state_and_word() {
        let on = banner(true, "computer");
        assert!(on.contains("ON"), "ON banner must say ON: {on}");
        assert!(
            on.contains("computer"),
            "ON banner must name the activation word"
        );
        let off = banner(false, "computer");
        assert!(off.contains("OFF"), "OFF banner must say OFF: {off}");
    }
}
