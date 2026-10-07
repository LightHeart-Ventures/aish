//! `:plugin create <id>` — the plugin scaffold generator (TASK-275, Phase 12.3).
//!
//! Writes a ready-to-edit plugin into the plugins directory
//! (`~/.aish/plugins/<id>/`) using the same layout as the canonical
//! `plugins/hello-world` plugin:
//!
//! ```text
//! <id>/
//!   plugin.json                 minimal valid manifest (+ config_schema, one webhook)
//!   README.md                   what to edit next
//!   skills/<id>/SKILL.md        one skill, picked up by discovery
//!   handlers/ping.sh            webhook handler (stdin JSON → stdout flash)
//! ```
//!
//! The generator never overwrites: an existing `<id>` directory is an error.
//! The full manifest reference lives in `docs/PLUGIN_DEVELOPER.md`.

use std::path::{Path, PathBuf};

/// Why a scaffold could not be created.
#[derive(Debug)]
pub enum ScaffoldError {
    /// The id is empty or contains characters outside `[a-z0-9._-]`.
    InvalidId(String),
    /// `<plugins_dir>/<id>` already exists — never clobber a plugin.
    AlreadyExists(PathBuf),
    /// Filesystem failure while writing the scaffold.
    Io(std::io::Error),
}

impl std::fmt::Display for ScaffoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScaffoldError::InvalidId(id) => write!(
                f,
                "invalid plugin id `{id}` — use lowercase letters, digits, `-`, `_` or `.` \
                 (an optional `scope/` prefix is dropped), starting with a letter or digit"
            ),
            ScaffoldError::AlreadyExists(p) => {
                write!(f, "{} already exists — refusing to overwrite", p.display())
            }
            ScaffoldError::Io(e) => write!(f, "could not write scaffold: {e}"),
        }
    }
}

impl std::error::Error for ScaffoldError {}

impl From<std::io::Error> for ScaffoldError {
    fn from(e: std::io::Error) -> Self {
        ScaffoldError::Io(e)
    }
}

/// Normalize a user-supplied id: an optional `scope/` prefix (e.g.
/// `mycompany/slack`) is dropped — plugins are keyed by a flat directory
/// name — and the remainder must match `[a-z0-9][a-z0-9._-]{0,63}`.
pub fn normalize_id(raw: &str) -> Result<String, ScaffoldError> {
    let raw = raw.trim();
    fn segment_ok(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 64
            && s.chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && s.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')
            })
    }
    // At most one `scope/` prefix, and the scope must itself be a valid
    // segment — so `../x` and `a/b/c` are rejected rather than silently trimmed.
    let (scope_ok, id) = match raw.split_once('/') {
        Some((scope, id)) => (segment_ok(scope) && !id.contains('/'), id),
        None => (true, raw),
    };
    let valid = scope_ok && segment_ok(id);
    if valid {
        Ok(id.to_string())
    } else {
        Err(ScaffoldError::InvalidId(raw.to_string()))
    }
}

/// The scaffold's `plugin.json`: a minimal but complete manifest that the
/// shell loader (`crate::plugins::discover`) and the webhook client
/// (`aish_webhook_client::PluginRegistry`) both accept.
pub fn manifest_json(id: &str) -> String {
    let manifest = serde_json::json!({
        "id": id,
        "name": id,
        "version": "0.1.0",
        "description": format!("{id} — scaffolded by `:plugin create`. Describe what it does."),
        "enabled": true,
        "config_schema": {
            "type": "object",
            "properties": {
                "greeting": { "type": "string", "default": "Hello" }
            }
        },
        "webhooks": [
            {
                "event_type": "ping",
                "command": ["handlers/ping.sh"],
                "timeout_secs": 10
            }
        ]
    });
    let mut s = serde_json::to_string_pretty(&manifest).expect("static manifest serializes");
    s.push('\n');
    s
}

fn readme(id: &str) -> String {
    format!(
        "# {id}\n\n\
Scaffolded by `:plugin create {id}`. Everything here is a starting point — edit freely.\n\n\
## Layout\n\n\
| Path | Purpose |\n\
|---|---|\n\
| `plugin.json` | Manifest: id, metadata, `config_schema`, `webhooks`, `provides` |\n\
| `skills/{id}/SKILL.md` | A skill merged into the agent's catalog |\n\
| `handlers/ping.sh` | Webhook handler: event JSON on stdin, first stdout line flashes on the statusline |\n\n\
## Next steps\n\n\
1. Fill in `description` in `plugin.json` and rewrite the skill.\n\
2. Add capabilities under `provides` (lifecycle hooks, timers, a statusline segment)\n\
   or a `.mcp.json` — see `docs/PLUGIN_DEVELOPER.md`.\n\
3. Subscribe to real webhook events in `webhooks` — see `docs/WEBHOOK_HANDLERS.md`.\n\
4. Check it loads: `:plugin info {id}`.\n\
5. Test the handler offline:\n\n\
   ```sh\n\
   echo '{{\"message\":\"hi\"}}' | ./handlers/ping.sh\n\
   ```\n"
    )
}

fn skill_md(id: &str) -> String {
    format!(
        "---\n\
name: {id}\n\
description: Describe when the agent should use the {id} plugin's skill (one sentence).\n\
---\n\n\
# {id}\n\n\
Replace this body with instructions for the agent. This file is discovered from\n\
`~/.aish/plugins/{id}/skills/{id}/SKILL.md` and merged into the skill registry.\n"
    )
}

fn ping_handler(id: &str) -> String {
    format!(
        "#!/bin/sh\n\
# {id} ping handler — receives the webhook payload as JSON on stdin.\n\
# The first non-empty stdout line becomes the statusline flash; stderr is logged.\n\
# Exit 0 on success; non-zero is recorded as a handler failure.\n\
payload=$(cat)\n\
[ -n \"$payload\" ] || payload='{{}}'\n\
echo \"👋 {id}: ping received\"\n"
    )
}

/// Create `<plugins_dir>/<id>/` from the scaffold. Returns the created
/// directory. Fails (without touching anything) if the id is invalid or the
/// directory already exists.
pub fn create(plugins_dir: &Path, raw_id: &str) -> Result<PathBuf, ScaffoldError> {
    let id = normalize_id(raw_id)?;
    let dir = plugins_dir.join(&id);
    if dir.exists() {
        return Err(ScaffoldError::AlreadyExists(dir));
    }
    std::fs::create_dir_all(dir.join("skills").join(&id))?;
    std::fs::create_dir_all(dir.join("handlers"))?;
    std::fs::write(dir.join("plugin.json"), manifest_json(&id))?;
    std::fs::write(dir.join("README.md"), readme(&id))?;
    std::fs::write(dir.join("skills").join(&id).join("SKILL.md"), skill_md(&id))?;
    let handler = dir.join("handlers").join("ping.sh");
    std::fs::write(&handler, ping_handler(&id))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&handler, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(dir)
}

/// REPL entry point for `:plugin create <id>`.
pub fn run(plugins_dir: &Path, id: Option<&str>) {
    let Some(id) = id else {
        println!(
            "usage: :plugin create <id>   (creates {}/<id>/)",
            plugins_dir.display()
        );
        return;
    };
    match create(plugins_dir, id) {
        Ok(dir) => {
            println!(
                "\x1b[32m✓\x1b[0m created plugin scaffold at {}",
                dir.display()
            );
            println!(
                "  edit plugin.json, then `:plugin info <id>` — guide: docs/PLUGIN_DEVELOPER.md"
            );
            println!("  restart aish (`:restart`) to load its skill and webhook handler");
        }
        Err(e) => eprintln!("\x1b[31m✗\x1b[0m {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aish-scaffold-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn normalize_id_accepts_flat_and_scoped_ids() {
        assert_eq!(normalize_id("slack").unwrap(), "slack");
        assert_eq!(normalize_id("mycompany/slack").unwrap(), "slack");
        assert_eq!(normalize_id("my-plugin_2.x").unwrap(), "my-plugin_2.x");
    }

    #[test]
    fn normalize_id_rejects_bad_ids() {
        for bad in [
            "", "Slack", "../evil", "a/b/c", "a b", "-lead", ".hidden", "x/", "é",
        ] {
            assert!(normalize_id(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(normalize_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn scaffold_is_discovered_with_skill_and_valid_config() {
        let root = tempdir();
        let dir = create(&root, "acme/demo").unwrap();
        assert_eq!(dir, root.join("demo"));
        for f in [
            "plugin.json",
            "README.md",
            "skills/demo/SKILL.md",
            "handlers/ping.sh",
        ] {
            assert!(dir.join(f).is_file(), "missing {f}");
        }

        let plugins = crate::plugins::discover(&root);
        assert_eq!(plugins.len(), 1);
        let p = &plugins[0];
        assert_eq!(p.manifest.id, "demo");
        assert!(p.manifest.is_enabled());
        assert_eq!(p.skills.len(), 1, "scaffold skill must load");
        let cfg = crate::plugins::load_config(&p.dir, &p.manifest).unwrap();
        assert_eq!(cfg["greeting"], "Hello");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scaffold_webhook_handler_registers_with_webhook_client() {
        let root = tempdir();
        create(&root, "demo").unwrap();
        let reg = aish_webhook_client::dispatcher::PluginRegistry::load_dir(&root).unwrap();
        let matches = reg.matching("ping");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "demo");
        assert!(matches[0].1.command[0].ends_with("handlers/ping.sh"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn scaffold_handler_is_executable_and_prints_flash() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir();
        let dir = create(&root, "demo").unwrap();
        let h = dir.join("handlers/ping.sh");
        assert_eq!(
            std::fs::metadata(&h).unwrap().permissions().mode() & 0o111,
            0o111
        );
        let mut child = std::process::Command::new(&h)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"{}").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "👋 demo: ping received"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_refuses_to_overwrite() {
        let root = tempdir();
        create(&root, "demo").unwrap();
        std::fs::write(root.join("demo/plugin.json"), "{\"id\":\"demo\"}").unwrap();
        let err = create(&root, "demo").unwrap_err();
        assert!(matches!(err, ScaffoldError::AlreadyExists(_)));
        assert_eq!(
            std::fs::read_to_string(root.join("demo/plugin.json")).unwrap(),
            "{\"id\":\"demo\"}",
            "existing plugin must be untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_rejects_invalid_id_without_writing() {
        let root = tempdir();
        assert!(matches!(
            create(&root, "../escape"),
            Err(ScaffoldError::InvalidId(_))
        ));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
