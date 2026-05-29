//! Embed the SKILL.md template + the four docs into the binary, install them
//! onto a Claude Code and/or Codex CLI skill directory on disk.
//!
//! Both clients use the same on-disk layout
//! (`<base>/skills/tabd/{SKILL.md, commands.md, cookbook.md, operations.md,
//! architecture.md}`) so the same template works for both — only the
//! `{{SKILL_DIR}}` placeholder inside SKILL.md is rewritten per install.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const SKILL_MD: &str = include_str!("../../../.claude/skills/tabd/SKILL.md");
const COMMANDS_MD: &str = include_str!("../../../docs/commands.md");
const COOKBOOK_MD: &str = include_str!("../../../docs/cookbook.md");
const OPERATIONS_MD: &str = include_str!("../../../docs/operations.md");
const ARCHITECTURE_MD: &str = include_str!("../../../docs/architecture.md");

const FILES: &[(&str, &str)] = &[
    ("SKILL.md", SKILL_MD),
    ("commands.md", COMMANDS_MD),
    ("cookbook.md", COOKBOOK_MD),
    ("operations.md", OPERATIONS_MD),
    ("architecture.md", ARCHITECTURE_MD),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Claude,
    Codex,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Claude => "claude",
            Target::Codex => "codex",
        }
    }

    /// Default install dir under $HOME — `~/.claude/skills/tabd` or
    /// `~/.codex/skills/tabd`. Both clients auto-discover SKILL.md in this
    /// layout.
    pub fn default_path(self) -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let sub = match self {
            Target::Claude => ".claude/skills/tabd",
            Target::Codex => ".codex/skills/tabd",
        };
        Ok(PathBuf::from(home).join(sub))
    }

    /// Detected when either the client config directory exists or the binary
    /// is on PATH. Either signal is enough — a fresh user that hasn't run the
    /// client yet still gets covered if the CLI is installed.
    pub fn detected(self) -> bool {
        let home = std::env::var_os("HOME");
        let config_dir = home.as_ref().map(|h| {
            let mut p = PathBuf::from(h);
            p.push(match self {
                Target::Claude => ".claude",
                Target::Codex => ".codex",
            });
            p
        });
        if let Some(p) = config_dir {
            if p.exists() {
                return true;
            }
        }
        binary_on_path(self.name())
    }
}

fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            candidate
                .metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            candidate.is_file()
        }
    })
}

/// Detect every client present on the machine. Order is fixed so output is
/// reproducible.
pub fn detect_targets() -> Vec<Target> {
    [Target::Claude, Target::Codex]
        .into_iter()
        .filter(|t| t.detected())
        .collect()
}

/// Parse `--target` argument: `"claude"` / `"codex"` / `"claude,codex"`.
pub fn parse_targets(raw: &str) -> Result<Vec<Target>> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let target = match part {
            "claude" => Target::Claude,
            "codex" => Target::Codex,
            other => bail!("unknown target '{other}' (expected: claude, codex)"),
        };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    if out.is_empty() {
        bail!("--target requires at least one of: claude, codex");
    }
    Ok(out)
}

/// Result of resolving `--target` / `--no-claude` / `--no-codex` / `--path`
/// into an actual install plan.
#[derive(Debug)]
pub struct InstallPlan {
    pub entries: Vec<InstallEntry>,
}

#[derive(Debug)]
pub struct InstallEntry {
    pub target: Option<Target>,
    pub dir: PathBuf,
}

pub fn build_plan(
    explicit_target: Option<&str>,
    no_claude: bool,
    no_codex: bool,
    explicit_path: Option<&str>,
) -> Result<InstallPlan> {
    // --path overrides everything else: install to one specific directory.
    if let Some(path) = explicit_path {
        if explicit_target.is_some() && explicit_target != Some("claude") && explicit_target != Some("codex") {
            // Allow --target claude/codex with --path so the user can pick the
            // {{SKILL_DIR}} substitution explicitly, but it is informational
            // only — the destination is the --path value either way.
        }
        let target = match explicit_target {
            Some("claude") => Some(Target::Claude),
            Some("codex") => Some(Target::Codex),
            Some(other) => bail!("with --path, --target must be 'claude' or 'codex' (got '{other}')"),
            None => None,
        };
        return Ok(InstallPlan {
            entries: vec![InstallEntry {
                target,
                dir: PathBuf::from(path),
            }],
        });
    }

    // Otherwise: explicit --target wins, then auto-detect.
    let targets: Vec<Target> = if let Some(raw) = explicit_target {
        parse_targets(raw)?
    } else {
        let detected = detect_targets();
        if detected.is_empty() {
            bail!(
                "no skill clients detected.\n\
                 Install Claude Code or Codex CLI first, or pass --path DIR to install \
                 to a custom directory."
            );
        }
        detected
    };

    // --no-claude / --no-codex are skip flags applied after detection.
    let targets: Vec<Target> = targets
        .into_iter()
        .filter(|t| !(no_claude && *t == Target::Claude))
        .filter(|t| !(no_codex && *t == Target::Codex))
        .collect();

    if targets.is_empty() {
        bail!("all targets were excluded by --no-claude / --no-codex");
    }

    let mut entries = Vec::with_capacity(targets.len());
    for t in targets {
        entries.push(InstallEntry {
            target: Some(t),
            dir: t.default_path()?,
        });
    }
    Ok(InstallPlan { entries })
}

pub fn install(plan: &InstallPlan, force: bool) -> Result<()> {
    for entry in &plan.entries {
        install_one(&entry.dir, entry.target, force)?;
    }
    Ok(())
}

fn install_one(dir: &Path, target: Option<Target>, force: bool) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create skill directory {}", dir.display()))?;

    // Pre-flight: refuse to overwrite without --force. Check every file up
    // front so we don't half-install on collision.
    if !force {
        for (name, _) in FILES {
            let path = dir.join(name);
            if path.exists() {
                bail!(
                    "{} already exists. Re-run with --force to overwrite.",
                    path.display()
                );
            }
        }
    }

    let dir_str = dir.display().to_string();
    for (name, content) in FILES {
        let resolved = content.replace("{{SKILL_DIR}}", &dir_str);
        let path = dir.join(name);
        std::fs::write(&path, resolved)
            .with_context(|| format!("write {}", path.display()))?;
    }

    let label = target.map(|t| t.name()).unwrap_or("custom");
    println!(
        "installed {} skill ({} files) to {}",
        label,
        FILES.len(),
        dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_targets_accepts_single_and_csv() {
        assert_eq!(parse_targets("claude").unwrap(), vec![Target::Claude]);
        assert_eq!(parse_targets("codex").unwrap(), vec![Target::Codex]);
        assert_eq!(
            parse_targets("claude,codex").unwrap(),
            vec![Target::Claude, Target::Codex]
        );
        assert_eq!(
            parse_targets("codex, claude").unwrap(),
            vec![Target::Codex, Target::Claude]
        );
    }

    #[test]
    fn parse_targets_rejects_unknown() {
        assert!(parse_targets("cursor").is_err());
        assert!(parse_targets("").is_err());
        assert!(parse_targets(",").is_err());
    }

    #[test]
    fn parse_targets_dedupes() {
        assert_eq!(parse_targets("claude,claude").unwrap(), vec![Target::Claude]);
    }

    #[test]
    fn build_plan_explicit_path_wins() {
        let plan = build_plan(None, false, false, Some("/tmp/x")).unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].dir, PathBuf::from("/tmp/x"));
        assert_eq!(plan.entries[0].target, None);
    }

    #[test]
    fn build_plan_explicit_target_and_path() {
        let plan = build_plan(Some("codex"), false, false, Some("/tmp/y")).unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].dir, PathBuf::from("/tmp/y"));
        assert_eq!(plan.entries[0].target, Some(Target::Codex));
    }

    #[test]
    fn build_plan_explicit_target_no_path_uses_defaults() {
        let plan = build_plan(Some("claude,codex"), false, false, None).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan.entries[0].dir.to_string_lossy().contains(".claude/skills/tabd"));
        assert!(plan.entries[1].dir.to_string_lossy().contains(".codex/skills/tabd"));
    }

    #[test]
    fn build_plan_no_claude_flag_skips_claude() {
        let plan = build_plan(Some("claude,codex"), true, false, None).unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].target, Some(Target::Codex));
    }

    #[test]
    fn build_plan_all_excluded_errors() {
        let err = build_plan(Some("claude"), true, false, None).unwrap_err();
        assert!(err.to_string().contains("excluded"));
    }

    #[test]
    fn install_one_writes_five_files_with_placeholder_substituted() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills/tabd");
        install_one(&dir, Some(Target::Claude), false).unwrap();

        for (name, _) in FILES {
            let p = dir.join(name);
            assert!(p.exists(), "missing {}", p.display());
        }

        let skill_md = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(
            !skill_md.contains("{{SKILL_DIR}}"),
            "{{SKILL_DIR}} placeholder was not substituted"
        );
        assert!(
            skill_md.contains(&dir.display().to_string()),
            "substituted path missing from SKILL.md"
        );
    }

    #[test]
    fn install_one_refuses_overwrite_without_force() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills/tabd");
        install_one(&dir, None, false).unwrap();
        let err = install_one(&dir, None, false).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "expected 'already exists' error, got: {err}"
        );
    }

    #[test]
    fn install_one_force_overwrites() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills/tabd");
        install_one(&dir, None, false).unwrap();
        // Mutate a file then re-install with --force; content should be rewritten.
        std::fs::write(dir.join("SKILL.md"), "user edit").unwrap();
        install_one(&dir, None, true).unwrap();
        let after = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert_ne!(after, "user edit", "force install should have rewritten SKILL.md");
        assert!(after.contains("tabd"), "rewritten SKILL.md should contain the template");
    }
}
