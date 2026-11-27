use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static SETTINGS_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

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

/// Claude Code WebFetch hook mode installed into Claude settings.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    #[default]
    Fallback,
    Redirect,
    None,
}

impl HookMode {
    /// Returns the CLI/JSON kebab/snake identifier for this hook mode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Redirect => "redirect",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub target: SetupTarget,
    pub claude_scope: ClaudeScope,
    pub hooks: HookMode,
    pub remove: bool,
    pub dry_run: bool,
    pub pleb_binary: Option<PathBuf>,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            target: SetupTarget::All,
            claude_scope: ClaudeScope::User,
            hooks: HookMode::Fallback,
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

/// Outcome of configuring or removing the Claude Code WebFetch hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookSetupResult {
    pub target: &'static str,
    pub mode: &'static str,
    pub status: &'static str,
    pub settings_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetupReport {
    pub pleb_binary: String,
    pub dry_run: bool,
    pub remove: bool,
    pub results: Vec<HarnessSetupResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<HookSetupResult>,
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

    let hook = if matches!(options.target, SetupTarget::All | SetupTarget::Claude)
        && options.hooks != HookMode::None
    {
        let claude_detected = results
            .iter()
            .any(|r| r.target == "claude" && r.status != "not_found");
        configure_claude_hooks(
            options.claude_scope,
            options.hooks,
            &pleb_str,
            options.remove,
            options.dry_run,
            options.target == SetupTarget::Claude || claude_detected,
        )?
    } else {
        None
    };

    Ok(SetupReport {
        pleb_binary: pleb_str,
        dry_run: options.dry_run,
        remove: options.remove,
        results,
        hook,
        skills: Vec::new(),
    })
}

const HOOK_EVENTS: [&str; 2] = ["PreToolUse", "PostToolUseFailure"];
const MAX_HOOK_STDIN_BYTES: u64 = 256 * 1024;

/// Executes the Claude Code `WebFetch` hook protocol for `PreToolUse` (`redirect`) or `PostToolUseFailure` (`fallback`).
pub fn run_claude_webfetch_hook(
    mode: HookMode,
    stdin: &mut impl Read,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<u8> {
    let mut buf = Vec::new();
    stdin.take(MAX_HOOK_STDIN_BYTES).read_to_end(&mut buf)?;
    if mode == HookMode::None {
        return Ok(0);
    }

    let parsed = (!buf.is_empty())
        .then(|| serde_json::from_slice::<Value>(&buf).ok())
        .flatten();
    if parsed
        .as_ref()
        .and_then(|v| v.get("is_interrupt"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(0);
    }

    let sanitized_url = parsed
        .as_ref()
        .and_then(|v| v.get("tool_input"))
        .and_then(|ti| ti.get("url"))
        .and_then(Value::as_str)
        .map(|s| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(1024)
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty());

    match mode {
        HookMode::Fallback => {
            let msg = match sanitized_url {
                Some(url) => format!(
                    "WebFetch failed for {url}. Retry with the local `pleb_fetch` MCP tool (url=\"{url}\") — `pleb` runs from the host machine over Chrome TLS (Cronet) with automatic 403/SPA escalation — or run `pleb fetch`."
                ),
                None => "WebFetch failed. Retry with the local `pleb_fetch` MCP tool or `pleb fetch <url>` (`pleb` uses host Chrome TLS and local escalation instead of datacenter IPs).".to_string(),
            };
            let payload = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUseFailure",
                    "additionalContext": msg,
                }
            });
            writeln!(stdout, "{payload}")?;
            stdout.flush()?;
            Ok(0)
        }
        HookMode::Redirect => {
            let msg = match sanitized_url {
                Some(url) => format!(
                    "Use the local `pleb_fetch` MCP tool (url=\"{url}\") or `pleb fetch \"{url}\"` instead of built-in WebFetch: `pleb` fetches from the host machine over Chrome TLS (Cronet) with automatic 403/SPA escalation."
                ),
                None => "Use the local `pleb_fetch` MCP tool or `pleb fetch <url>` instead of built-in WebFetch.".to_string(),
            };
            writeln!(stderr, "{msg}")?;
            stderr.flush()?;
            Ok(2)
        }
        HookMode::None => Ok(0),
    }
}

fn configure_claude_hooks(
    scope: ClaudeScope,
    mode: HookMode,
    pleb_bin: &str,
    remove: bool,
    dry_run: bool,
    claude_explicit_or_present: bool,
) -> Result<Option<HookSetupResult>> {
    let settings_override = std::env::var_os("PLEB_CLAUDE_SETTINGS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let settings_path = match &settings_override {
        Some(p) => p.clone(),
        None => resolve_claude_settings_path(scope)?,
    };
    let path_str = settings_path.display().to_string();

    let exists = settings_path.exists();
    if !claude_explicit_or_present && settings_override.is_none() && !exists {
        return Ok(None);
    }

    let hook_cmd = format_hook_command(pleb_bin, mode);
    if dry_run {
        return Ok(Some(HookSetupResult {
            target: "claude",
            mode: mode.as_str(),
            status: "planned",
            settings_path: path_str,
            detail: Some(if remove {
                "remove pleb WebFetch hook".to_string()
            } else {
                format!("{} -> {}", hook_event_name(mode), hook_cmd)
            }),
        }));
    }

    if !exists && remove {
        return Ok(Some(HookSetupResult {
            target: "claude",
            mode: mode.as_str(),
            status: "not_present",
            settings_path: path_str,
            detail: None,
        }));
    }

    let mut root: Value = if exists {
        let raw = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("failed to read {}", settings_path.display()))?;
        if raw.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            let parsed: Value = serde_json::from_str(&raw)
                .with_context(|| format!("invalid JSON in {}", settings_path.display()))?;
            if !parsed.is_object() {
                bail!(
                    "expected top-level JSON object in {}",
                    settings_path.display()
                );
            }
            parsed
        }
    } else {
        Value::Object(Map::new())
    };

    let original = root.clone();
    let removed_any = strip_pleb_webfetch_hooks(&mut root);

    let status = if remove {
        if removed_any {
            "removed"
        } else {
            "not_present"
        }
    } else {
        insert_pleb_webfetch_hook(&mut root, mode, &hook_cmd, &settings_path)?;
        if root == original {
            "up_to_date"
        } else {
            "configured"
        }
    };

    if root != original || (!exists && !remove) {
        let mut encoded = serde_json::to_string_pretty(&root)?;
        encoded.push('\n');
        write_settings_atomic(&settings_path, encoded.as_bytes())?;
    }

    Ok(Some(HookSetupResult {
        target: "claude",
        mode: mode.as_str(),
        status,
        settings_path: path_str,
        detail: (!remove).then(|| format!("event={}", hook_event_name(mode))),
    }))
}

fn hook_event_name(mode: HookMode) -> &'static str {
    match mode {
        HookMode::Fallback => "PostToolUseFailure",
        HookMode::Redirect => "PreToolUse",
        HookMode::None => "",
    }
}

fn format_hook_command(pleb_bin: &str, mode: HookMode) -> String {
    let quoted = if !pleb_bin.is_empty()
        && pleb_bin
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.' | b':'))
    {
        pleb_bin.to_string()
    } else {
        format!("'{}'", pleb_bin.replace('\'', "'\\''"))
    };
    format!("{quoted} hook claude-webfetch --mode {}", mode.as_str())
}

fn is_pleb_webfetch_hook_command(cmd: &str) -> bool {
    cmd.contains("hook claude-webfetch") && cmd.contains("pleb")
}

fn strip_pleb_webfetch_hooks(root: &mut Value) -> bool {
    let Some(root_obj) = root.as_object_mut() else {
        return false;
    };
    let Some(hooks_obj) = root_obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };

    let mut removed_any = false;
    for event in HOOK_EVENTS {
        let Some(entries) = hooks_obj.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        entries.retain_mut(|entry| {
            let Some(entry_obj) = entry.as_object_mut() else {
                return true;
            };
            let Some(inner_hooks) = entry_obj.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before_len = inner_hooks.len();
            inner_hooks.retain(|h| {
                let is_ours = h
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_pleb_webfetch_hook_command);
                if is_ours {
                    removed_any = true;
                }
                !is_ours
            });
            !(inner_hooks.len() != before_len && inner_hooks.is_empty())
        });
        if entries.is_empty() {
            hooks_obj.remove(event);
        }
    }

    if hooks_obj.is_empty() {
        root_obj.remove("hooks");
    }
    removed_any
}

fn insert_pleb_webfetch_hook(
    root: &mut Value,
    mode: HookMode,
    command: &str,
    settings_path: &Path,
) -> Result<()> {
    let root_obj = root.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "expected top-level JSON object in {}",
            settings_path.display()
        )
    })?;
    let hooks_entry = root_obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = hooks_entry.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "expected \"hooks\" to be a JSON object in {}",
            settings_path.display()
        )
    })?;
    let event = hook_event_name(mode);
    let event_entry = hooks_obj
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()));
    let event_arr = event_entry.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "expected hooks.{event} to be a JSON array in {}",
            settings_path.display()
        )
    })?;
    event_arr.push(json!({
        "matcher": "WebFetch",
        "hooks": [{
            "type": "command",
            "command": command,
        }],
    }));
    Ok(())
}

fn resolve_claude_settings_path(scope: ClaudeScope) -> Result<PathBuf> {
    match scope {
        ClaudeScope::User => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .context("cannot resolve HOME for Claude settings path")?;
            Ok(home.join(".claude").join("settings.json"))
        }
        ClaudeScope::Project => Ok(PathBuf::from(".claude").join("settings.json")),
        ClaudeScope::Local => Ok(PathBuf::from(".claude").join("settings.local.json")),
    }
}

fn write_settings_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("settings path {} missing parent directory", path.display())
    })?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json");
    let seq = SETTINGS_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));
    std::fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write temporary settings {}", tmp_path.display()))?;
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err).with_context(|| format!("failed to replace {}", path.display()));
    }
    Ok(())
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
