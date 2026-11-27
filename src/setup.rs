use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupTarget {
    #[default]
    All,
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeScope {
    #[default]
    User,
    Local,
    Project,
}

impl ClaudeScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Local => "local",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub target: SetupTarget,
    pub claude_scope: ClaudeScope,
    pub remove: bool,
    pub dry_run: bool,
    pub pleb_binary: Option<PathBuf>,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            target: SetupTarget::All,
            claude_scope: ClaudeScope::User,
            remove: false,
            dry_run: false,
            pleb_binary: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessSetupResult {
    pub target: &'static str,
    pub status: &'static str,
    pub binary: Option<String>,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillInstallStatus {
    pub path: String,
    pub status: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetupReport {
    pub pleb_binary: String,
    pub dry_run: bool,
    pub remove: bool,
    pub results: Vec<HarnessSetupResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillInstallStatus>,
}

struct HarnessSpec<'a> {
    target: &'static str,
    display_name: &'static str,
    env_var: &'static str,
    cli_name: &'static str,
    install_hint: &'static str,
    remove_args: &'a [&'a str],
    add_args: &'a [&'a str],
    detail: Option<String>,
}

pub fn run_setup(options: &SetupOptions) -> Result<SetupReport> {
    let pleb_bin = resolve_pleb_binary(options.pleb_binary.as_deref())?;
    let pleb_str = pleb_bin.to_string_lossy().into_owned();
    let explicit_single = options.target != SetupTarget::All;
    let scope_str = options.claude_scope.as_str();

    let mut results = Vec::new();
    if matches!(options.target, SetupTarget::All | SetupTarget::Claude) {
        let remove_args = ["mcp", "remove", "-s", scope_str, "pleb"];
        let add_args = [
            "mcp", "add", "-s", scope_str, "-t", "stdio", "pleb", "--", &pleb_str, "mcp",
        ];
        results.push(setup_harness(
            HarnessSpec {
                target: "claude",
                display_name: "Claude Code",
                env_var: "PLEB_CLAUDE_BIN",
                cli_name: "claude",
                install_hint: "Claude Code",
                remove_args: &remove_args,
                add_args: &add_args,
                detail: Some(format!("scope={scope_str}")),
            },
            options.remove,
            options.dry_run,
            explicit_single,
        )?);
    }

    if matches!(options.target, SetupTarget::All | SetupTarget::Codex) {
        let remove_args = ["mcp", "remove", "pleb"];
        let add_args = ["mcp", "add", "pleb", "--", &pleb_str, "mcp"];
        results.push(setup_harness(
            HarnessSpec {
                target: "codex",
                display_name: "Codex",
                env_var: "PLEB_CODEX_BIN",
                cli_name: "codex",
                install_hint: "Codex CLI",
                remove_args: &remove_args,
                add_args: &add_args,
                detail: None,
            },
            options.remove,
            options.dry_run,
            explicit_single,
        )?);
    }

    Ok(SetupReport {
        pleb_binary: pleb_str,
        dry_run: options.dry_run,
        remove: options.remove,
        results,
        skills: Vec::new(),
    })
}

fn format_manual_command(cli_name: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(cli_name.to_string());
    for arg in args {
        if arg.contains(char::is_whitespace) {
            parts.push(format!("{arg:?}"));
        } else {
            parts.push((*arg).to_string());
        }
    }
    parts.join(" ")
}

fn setup_harness(
    spec: HarnessSpec<'_>,
    remove: bool,
    dry_run: bool,
    fail_if_missing: bool,
) -> Result<HarnessSetupResult> {
    let active_args = if remove {
        spec.remove_args
    } else {
        spec.add_args
    };
    let manual_cmd = format_manual_command(spec.cli_name, active_args);
    let bin = resolve_harness_binary(spec.env_var, spec.cli_name);

    if dry_run {
        return Ok(HarnessSetupResult {
            target: spec.target,
            status: "planned",
            binary: bin.map(|p| p.display().to_string()),
            command: manual_cmd,
            detail: None,
        });
    }

    let Some(cli_bin) = bin else {
        if fail_if_missing {
            bail!(
                "{} CLI not found on PATH; install {} or run manually: {}",
                spec.cli_name,
                spec.install_hint,
                manual_cmd
            );
        }
        return Ok(HarnessSetupResult {
            target: spec.target,
            status: "not_found",
            binary: None,
            command: manual_cmd,
            detail: Some(format!("{} CLI not found on PATH", spec.cli_name)),
        });
    };

    if remove {
        let out = run_harness_cmd(&cli_bin, spec.remove_args)?;
        let status = if out.success {
            "removed"
        } else {
            "not_present"
        };
        let detail = (!out.success)
            .then_some(out.combined_output)
            .filter(|s| !s.is_empty());
        return Ok(HarnessSetupResult {
            target: spec.target,
            status,
            binary: Some(cli_bin.display().to_string()),
            command: manual_cmd,
            detail,
        });
    }

    let _ = run_harness_cmd(&cli_bin, spec.remove_args);
    let add_out = run_harness_cmd(&cli_bin, spec.add_args)?;
    if !add_out.success {
        bail!(
            "failed to register pleb MCP server with {} ({}): {}",
            spec.display_name,
            cli_bin.display(),
            if add_out.combined_output.is_empty() {
                "non-zero exit".to_string()
            } else {
                add_out.combined_output
            }
        );
    }

    Ok(HarnessSetupResult {
        target: spec.target,
        status: "configured",
        binary: Some(cli_bin.display().to_string()),
        command: manual_cmd,
        detail: spec.detail,
    })
}

struct CmdOutput {
    success: bool,
    combined_output: String,
}

fn run_harness_cmd(bin: &Path, args: &[&str]) -> Result<CmdOutput> {
    let mut cmd = Command::new(bin);
    crate::auth::scrub_sensitive_env(&mut cmd);
    cmd.args(args);
    let out = cmd
        .output()
        .with_context(|| format!("failed to execute {}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let combined = match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}; {stderr}"),
    };
    Ok(CmdOutput {
        success: out.status.success(),
        combined_output: combined,
    })
}

fn resolve_pleb_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if !path.as_os_str().is_empty() {
            return Ok(path.to_path_buf());
        }
    }
    if let Some(env_path) = std::env::var_os("PLEB_SETUP_BINARY") {
        if !env_path.is_empty() {
            return Ok(PathBuf::from(env_path));
        }
    }
    std::env::current_exe().context("failed to resolve current pleb executable")
}

fn resolve_harness_binary(env_var: &str, name: &str) -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os(env_var) {
        if !override_path.is_empty() {
            let path = PathBuf::from(override_path);
            return is_executable_file(&path).then_some(path);
        }
    }
    find_in_path(name)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if is_executable_file(&exe) {
                return Some(exe);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
