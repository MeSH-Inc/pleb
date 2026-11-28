use clap::{Parser, Subcommand};
use pleb::auth::{self, CredentialProvider, CredentialSource, CredentialStorage, SecretString};
use pleb::context::{clear_state_dir, default_state_dir, RuntimeConfig, RuntimeContext};
use pleb::crawl;
#[cfg(unix)]
use pleb::daemon;
use pleb::extract::{format_page_section, ExpandPolicy, ExtractMode, OutputFormat};
use pleb::fetch::{
    self, BatchFetchOptions, BatchItemResult, FetchError, FetchOptions, FetchOutput,
};
use pleb::policy::{RenderMode, ResourcePolicy, SourceMode, ToolPolicy};
use pleb::search::{
    self, Freshness, SearchMode, SearchOptions, SearchProvider, SearchProviderStatus,
};
use pleb::setup;
use pleb::transport::cdp;
use pleb::transport::cef::{self, RendererProbe};
use pleb::transport::reveal::{RevealPolicy, MAX_SCROLL_STEPS};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "pleb",
    version,
    about = "Local browser-grade web fetch (CLI + MCP) that resolves agent 403 blocks and empty shells"
)]
struct Cli {
    /// SOCKS5/HTTP proxy URL (e.g., socks5://localhost:1080)
    #[arg(long, global = true, env = "PLEB_PROXY")]
    proxy: Option<String>,

    /// Network request timeout in seconds
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    /// Maximum HTTP response body bytes to read (defaults: 16 MiB document, 128 MiB raw)
    #[arg(long, global = true)]
    max_response_bytes: Option<usize>,

    /// CEF render timeout in seconds
    #[arg(long, global = true, default_value = "60")]
    cef_timeout: u64,

    /// Disable RFC 9421 Web Bot Auth request signing
    #[arg(long, global = true, env = "PLEB_NO_WEB_BOT_AUTH")]
    no_web_bot_auth: bool,

    /// WebSocket CDP / Kitesurf endpoint URL (ws:// loopback or wss:// remote)
    #[arg(long, global = true, env = "PLEB_CDP_URL")]
    cdp_url: Option<String>,

    /// CDP render timeout in seconds
    #[arg(long, global = true, env = "PLEB_CDP_TIMEOUT", default_value = "60")]
    cdp_timeout: u64,

    /// Prefer CDP over local CEF during --render auto escalation
    #[arg(long, global = true, env = "PLEB_PREFER_CDP")]
    prefer_cdp: bool,

    /// Directory for local cache and session state
    #[arg(long, global = true, env = "PLEB_HOME")]
    state_dir: Option<PathBuf>,

    /// Disable persistent background daemon sharing for this command
    #[arg(long, global = true, env = "PLEB_NO_DAEMON")]
    no_daemon: bool,

    /// Idle timeout in seconds for the auto-spawned background daemon
    #[arg(
        long,
        global = true,
        env = "PLEB_DAEMON_IDLE_SECONDS",
        default_value = "300"
    )]
    daemon_idle_timeout: u64,

    /// Enable verbose diagnostic logging to stderr
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Suppress non-error status metadata lines on stderr
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Emit structured JSON output on stdout and JSON error envelopes on stderr
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch one or more URLs (or '-' to read newline-separated URLs from stdin)
    Fetch {
        /// Target URL(s) (http/https), or '-' to read URLs from stdin (optional with --no-navigate as origin assertion)
        urls: Vec<String>,
        /// Attach read-only to an already-loaded loopback /devtools/page/<id> tab without navigating
        #[arg(long)]
        no_navigate: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Markdown, ignore_case = true)]
        format: OutputFormat,
        /// Content extraction scope
        #[arg(long, value_enum, default_value_t = ExtractMode::Clean, ignore_case = true)]
        extract: ExtractMode,
        /// In-landmark ARIA disclosure rescue policy
        #[arg(long, value_enum, default_value_t = ExpandPolicy::None, ignore_case = true)]
        expand: ExpandPolicy,
        /// Pre-capture DOM reveal policy for unmounted tabs and disclosures
        #[arg(long, value_enum, default_value_t = RevealPolicy::None, ignore_case = true)]
        reveal: RevealPolicy,
        /// Progressive viewport scroll steps before DOM capture (0-20)
        #[arg(long, default_value_t = 0)]
        scroll: usize,
        /// Transport mode
        #[arg(long, value_enum, default_value_t = RenderMode::Auto, ignore_case = true)]
        render: RenderMode,
        /// Structured source negotiation mode
        #[arg(long, value_enum, default_value_t = SourceMode::Auto, ignore_case = true)]
        source: SourceMode,
        /// Linked resource indexing and staging policy
        #[arg(long, value_enum, default_value_t = ResourcePolicy::None, ignore_case = true)]
        resources: ResourcePolicy,
        /// WebMCP tool indexing policy
        #[arg(long, value_enum, default_value_t = ToolPolicy::None, ignore_case = true)]
        tools: ToolPolicy,
        /// Selectively stage or inline specific resource IDs (comma-separated, e.g. r1,r3)
        #[arg(long, value_delimiter = ',')]
        stage_resources: Vec<String>,
        /// Optional parent page URL used to compute Referer and Sec-Fetch-* headers
        #[arg(long)]
        referer: Option<String>,
        /// Stage a single fetched asset into <state_dir>/artifacts/ and print its local path
        #[arg(long, conflicts_with_all = ["output", "metadata_only", "jsonl"])]
        stage: bool,
        /// CSS selector to wait for when CEF or CDP renders
        #[arg(long)]
        wait_for_selector: Option<String>,
        /// Enforce robots.txt for this fetch (skipped by default)
        #[arg(long)]
        respect_robots: bool,
        /// Disable host rewrite rules (from site-rules.json) and forge raw-source rewrites
        #[arg(long)]
        no_rewrite: bool,
        /// Strip image references from markdown output
        #[arg(long)]
        no_images: bool,
        /// Unwrap hyperlink URLs in markdown output
        #[arg(long)]
        no_links: bool,
        /// Truncate extracted content to at most N bytes
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Project long markdown/text output onto query-relevant sections (BM25 + bounded collapsed-section map; default 16000-byte budget or --max-bytes)
        #[arg(long)]
        focus: Option<String>,
        /// Extract exact Markdown section(s) matching a heading selector (exact, prefix, or substring match; includes subsections)
        #[arg(long)]
        section: Option<String>,
        /// Start byte offset within the extracted content for fence-safe sequential continuation (requires --format md, text, html, or axtree)
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Maximum concurrent requests in batch mode (1-16)
        #[arg(long, default_value = "4")]
        concurrency: usize,
        /// Stream one JSON object per completed URL in batch mode
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
        /// With --json or --jsonl, omit the content field and emit metadata only
        #[arg(long)]
        metadata_only: bool,
        /// Write output to file path instead of stdout (created only after fetch succeeds)
        #[arg(short = 'o', long, conflicts_with = "metadata_only")]
        output: Option<PathBuf>,
    },
    /// Extract clean Markdown, text, or HTML offline from stdin or a local file
    Extract {
        /// Input file path, or '-' to read from stdin
        #[arg(value_name = "INPUT")]
        input: Option<PathBuf>,
        /// Origin URL used for link resolution, base href fallback, and metadata
        #[arg(long)]
        url: String,
        /// MIME Content-Type of the input payload
        #[arg(long, default_value = "text/html; charset=utf-8")]
        content_type: String,
        /// Output format (markdown, text, or html)
        #[arg(long, value_enum, default_value_t = OutputFormat::Markdown, ignore_case = true)]
        format: OutputFormat,
        /// Content extraction scope
        #[arg(long, value_enum, default_value_t = ExtractMode::Clean, ignore_case = true)]
        extract: ExtractMode,
        /// In-landmark ARIA disclosure rescue policy
        #[arg(long, value_enum, default_value_t = ExpandPolicy::None, ignore_case = true)]
        expand: ExpandPolicy,
        /// Linked resource indexing policy (none or manifest)
        #[arg(long, value_enum, default_value_t = ResourcePolicy::None, ignore_case = true)]
        resources: ResourcePolicy,
        /// WebMCP tool indexing policy
        #[arg(long, value_enum, default_value_t = ToolPolicy::None, ignore_case = true)]
        tools: ToolPolicy,
        /// Strip image references from markdown output
        #[arg(long)]
        no_images: bool,
        /// Unwrap hyperlink URLs in markdown output
        #[arg(long)]
        no_links: bool,
        /// Truncate extracted content to at most N bytes
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Project long markdown/text output onto query-relevant sections (BM25 + bounded collapsed-section map; default 16000-byte budget or --max-bytes)
        #[arg(long)]
        focus: Option<String>,
        /// Extract exact Markdown section(s) matching a heading selector (exact, prefix, or substring match; includes subsections)
        #[arg(long)]
        section: Option<String>,
        /// Start byte offset within the extracted content for fence-safe sequential continuation
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Maximum raw input bytes to read before rejecting (default: 16 MiB)
        #[arg(long, default_value_t = pleb::transport::DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES)]
        max_input_bytes: usize,
        /// With --json, omit the content field and emit metadata only
        #[arg(long)]
        metadata_only: bool,
        /// Write output to file path instead of stdout (atomically written only after extraction succeeds)
        #[arg(short = 'o', long, conflicts_with = "metadata_only")]
        output: Option<PathBuf>,
    },
    /// Search the web and print results
    Search {
        /// Search query
        query: String,
        /// Maximum number of results (1-20)
        #[arg(short = 'n', long = "limit", default_value = "8")]
        limit: usize,
        /// Search provider
        #[arg(long, value_enum, default_value_t = SearchProvider::Auto, ignore_case = true)]
        provider: SearchProvider,
        /// Search routing intent mode
        #[arg(long, value_enum, default_value_t = SearchMode::Auto, ignore_case = true)]
        mode: SearchMode,
        /// Print only one bare URL per line (nothing on empty results)
        #[arg(long, conflicts_with = "json")]
        urls_only: bool,
        /// Filter search results by recency
        #[arg(long, value_enum, ignore_case = true)]
        freshness: Option<Freshness>,
        /// Restrict search results to a domain (prepends site:<domain>)
        #[arg(long)]
        site: Option<String>,
        /// Request additional snippet excerpts from supported search providers
        #[arg(long)]
        extra_snippets: bool,
    },
    /// Manage stored search, agent identity, and CDP Access credentials
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Crawl a website starting from a URL (per-page content capped at 10 KiB; plain output at 100 KiB)
    Crawl {
        /// Starting URL
        url: String,
        /// Maximum link depth (max 5)
        #[arg(short, long, default_value = "2")]
        depth: u32,
        /// Maximum pages to fetch (max 50)
        #[arg(short = 'n', long, default_value = "10")]
        max_pages: u32,
        /// Content extraction scope
        #[arg(long, value_enum, default_value_t = ExtractMode::Clean, ignore_case = true)]
        extract: ExtractMode,
        /// Only follow URLs whose path starts with this prefix
        #[arg(long)]
        path_filter: Option<String>,
        /// Ignore robots.txt restrictions during crawl
        #[arg(long)]
        no_robots: bool,
    },
    /// Discover URLs on a website via sitemap.xml and up to 20 HTML pages
    Map {
        /// Starting URL
        url: String,
        /// Maximum URLs to discover (max 5000)
        #[arg(short = 'n', long, default_value = "100")]
        limit: u32,
        /// Skip sitemap.xml discovery
        #[arg(long)]
        no_sitemap: bool,
        /// Only include URLs whose path starts with this prefix
        #[arg(long)]
        path_filter: Option<String>,
        /// Ignore robots.txt restrictions during discovery
        #[arg(long)]
        no_robots: bool,
    },
    /// Manage persistent unified transport daemon (Cronet/reqwest + CEF)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Manage local browser/network session state
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Show binary, Cronet, CEF, CDP, agent identity, and search configuration status
    Status,
    /// Print or install the canonical version-locked pleb agent skill
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Run a local stdio MCP server exposing pleb_fetch, pleb_search, pleb_crawl, pleb_map, and pleb_status
    Mcp,
    /// Register `pleb mcp` and bundled skill with Claude Code and Codex
    Setup {
        /// Target agent harness to configure
        #[arg(long, value_enum, default_value_t = setup::SetupTarget::All, ignore_case = true)]
        target: setup::SetupTarget,
        /// Claude Code MCP scope (user, local, or project)
        #[arg(long, value_enum, default_value_t = setup::ClaudeScope::User, ignore_case = true)]
        scope: setup::ClaudeScope,
        /// Claude Code WebFetch hook mode (redirect, fallback, or none)
        #[arg(
            long,
            value_enum,
            default_value_t = setup::HookMode::Redirect,
            default_missing_value = "redirect",
            num_args = 0..=1,
            ignore_case = true
        )]
        hooks: setup::HookMode,
        /// Skip installing or updating the bundled SKILL.md
        #[arg(long)]
        skip_skill: bool,
        /// Unregister `pleb` from the target harness(es)
        #[arg(long)]
        remove: bool,
        /// Print planned registration commands without modifying harness configs
        #[arg(long)]
        dry_run: bool,
    },
    /// Run harness hook adapters invoked by Claude Code
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
}

#[derive(Subcommand)]
enum HookAction {
    /// Claude Code WebFetch hook adapter (PreToolUse redirect or PostToolUseFailure fallback)
    ClaudeWebfetch {
        /// Hook mode configured in Claude Code settings
        #[arg(long, value_enum, default_value_t = setup::HookMode::Redirect, ignore_case = true)]
        mode: setup::HookMode,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Store a search, Web Bot Auth, or CDP Access credential (hidden TTY prompt, or read from --stdin)
    Set {
        /// Credential provider or slot to store
        #[arg(value_enum, ignore_case = true)]
        provider: CredentialProvider,
        /// Read credential from stdin (<= 4096 bytes; single line, or multi-line JSON for web-bot-auth-jwk)
        #[arg(long)]
        stdin: bool,
        /// Credential storage backend
        #[arg(long, value_enum, default_value_t = CredentialStorage::Auto, ignore_case = true)]
        storage: CredentialStorage,
    },
    /// Remove a stored search, Web Bot Auth, or CDP Access credential
    Remove {
        /// Credential provider or slot to remove
        #[arg(value_enum, ignore_case = true)]
        provider: CredentialProvider,
        /// Credential storage backend to remove from
        #[arg(long, value_enum, default_value_t = CredentialStorage::Auto, ignore_case = true)]
        storage: CredentialStorage,
    },
    /// Show search provider, Web Bot Auth, and CDP Access credential and cooldown status
    Status,
    /// Generate an Ed25519 Web Bot Auth keypair and print its public directory JSON
    InitWebBotAuth {
        /// HTTPS signature-agent URL for your crawler/agent identity
        #[arg(long)]
        agent: String,
        /// Credential storage backend
        #[arg(long, value_enum, default_value_t = CredentialStorage::Auto, ignore_case = true)]
        storage: CredentialStorage,
        /// Overwrite existing Web Bot Auth credentials
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum SkillAction {
    /// Print the bundled pleb SKILL.md to stdout
    Print,
    /// Install the bundled pleb SKILL.md into local agent skill directories (or --dir <PATH>)
    Install {
        /// Target directory to write <DIR>/SKILL.md into instead of default agent locations
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Overwrite existing modified SKILL.md files or symlinks
        #[arg(long)]
        force: bool,
    },
}

const BUNDLED_SKILL_MD: &str = include_str!("../skills/pleb/SKILL.md");

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the unified transport daemon (auto-started on demand)
    Run,
    /// Show unified transport daemon status
    Status,
    /// Stop the running unified transport daemon
    Stop,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Stop daemon and clear caches, artifacts, and site/search state (preserves credentials and site-rules.json)
    Clear,
}

#[derive(Debug)]
struct CliError {
    message: String,
    kind: &'static str,
    block_reason: Option<pleb::policy::BlockReason>,
    exit_code: u8,
}

impl CliError {
    fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "invalid_input",
            block_reason: None,
            exit_code: 1,
        }
    }

    fn from_fetch(err: FetchError) -> Self {
        Self {
            kind: err.kind(),
            block_reason: err.block_reason(),
            exit_code: err.exit_code().clamp(1, 255) as u8,
            message: err.to_string(),
        }
    }

    fn runtime(kind: &'static str, err: impl std::fmt::Display) -> Self {
        Self {
            message: err.to_string(),
            kind,
            block_reason: None,
            exit_code: 1,
        }
    }
}

#[derive(Serialize)]
struct FetchMetadataOutput<'a> {
    url: &'a str,
    final_url: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    representation_url: Option<&'a str>,
    status_code: u16,
    content_type: Option<&'a str>,
    title: Option<&'a str>,
    strategy: &'a str,
    content_outcome: fetch::ContentOutcome,
    acquired_sha256: &'a str,
    escalated: bool,
    truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    timing_ms: u64,
    content_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    warning: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    resources: Vec<pleb::extract::ResourceDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tools: Vec<pleb::extract::WebMcpToolDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ui_gates: Vec<pleb::extract::UiGateDescriptor>,
}

impl<'a> From<&'a FetchOutput> for FetchMetadataOutput<'a> {
    fn from(res: &'a FetchOutput) -> Self {
        Self {
            url: &res.url,
            final_url: &res.final_url,
            representation_url: res.representation_url.as_deref(),
            status_code: res.status_code,
            content_type: res.content_type.as_deref(),
            title: res.title.as_deref(),
            strategy: &res.strategy,
            content_outcome: res.content_outcome,
            acquired_sha256: &res.acquired_sha256,
            escalated: res.escalated,
            truncated: res.truncated,
            next_offset: res.next_offset,
            timing_ms: res.timing_ms,
            content_bytes: res.content_bytes,
            warning: res.warning.as_deref(),
            resources: res.resources.clone(),
            tools: res.tools.clone(),
            ui_gates: res.ui_gates.clone(),
        }
    }
}

#[derive(Serialize)]
struct ExtractOutput<'a> {
    url: &'a str,
    content_type: &'a str,
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    content_bytes: usize,
    acquired_sha256: &'a str,
    truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    timing_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    warning: Option<&'a str>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    resources: &'a [pleb::extract::ResourceDescriptor],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [pleb::extract::WebMcpToolDescriptor],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    ui_gates: &'a [pleb::extract::UiGateDescriptor],
}

#[derive(Default, Serialize)]
struct DaemonStatusJson {
    supported: bool,
    running: bool,
    pid: Option<u32>,
    protocol_version: Option<u32>,
    build_id: Option<String>,
    active_requests: usize,
    cronet_slots: usize,
    cef_renderers_alive: usize,
}

#[derive(Serialize)]
struct CdpStatusJson {
    configured: bool,
    url: Option<String>,
    prefer_cdp: bool,
    proxy_blocked: bool,
    access: auth::CdpAccessStatus,
    status_error: Option<String>,
}

#[derive(Serialize)]
struct StatusJson {
    version: &'static str,
    cronet_enabled: bool,
    cef_renderer: cef::RendererStatus,
    cdp_renderer: CdpStatusJson,
    daemon: DaemonStatusJson,
    state_dir: String,
    search_providers: BTreeMap<String, SearchProviderStatus>,
    agent_identity: auth::AgentIdentityStatus,
}

#[derive(Serialize)]
struct AuthStatusJson {
    #[serde(flatten)]
    search_providers: BTreeMap<String, SearchProviderStatus>,
    agent_identity: auth::AgentIdentityStatus,
    cdp_access: auth::CdpAccessStatus,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.use_stderr() {
                if std::env::args_os().any(|a| a == "--json" || a == "--jsonl") {
                    let cli_err = CliError::invalid_input(err.to_string().trim_end());
                    emit_cli_error(&cli_err, true);
                } else {
                    let _ = err.print();
                }
                return ExitCode::from(1);
            }
            let _ = err.print();
            return ExitCode::SUCCESS;
        }
    };

    init_tracing(cli.verbose);
    let json_error_mode = cli.json || matches!(&cli.command, Command::Fetch { jsonl: true, .. });

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            let cli_err = CliError::runtime("runtime_error", err);
            emit_cli_error(&cli_err, json_error_mode);
            return ExitCode::from(cli_err.exit_code);
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(err) => {
            drop(runtime);
            emit_cli_error(&err, json_error_mode);
            ExitCode::from(err.exit_code)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, CliError> {
    let state_dir = cli.state_dir.unwrap_or_else(default_state_dir);
    let json = cli.json;
    let quiet = cli.quiet;

    let config = RuntimeConfig {
        state_dir: state_dir.clone(),
        proxy: cli.proxy.filter(|s| !s.trim().is_empty()),
        timeout: Duration::from_secs(cli.timeout.max(1)),
        max_response_bytes: cli.max_response_bytes,
        cef_timeout: Duration::from_secs(cli.cef_timeout.max(1)),
        daemon_idle_timeout: Duration::from_secs(cli.daemon_idle_timeout.max(1)),
        use_daemon: !cli.no_daemon,
        verbose: cli.verbose,
        no_web_bot_auth: cli.no_web_bot_auth,
        cdp_url: cli.cdp_url.filter(|s| !s.trim().is_empty()),
        cdp_timeout: Duration::from_secs(cli.cdp_timeout.max(1)),
        prefer_cdp: cli.prefer_cdp,
        ..Default::default()
    };

    match cli.command {
        Command::Daemon { action } => match action {
            DaemonAction::Run => {
                #[cfg(unix)]
                {
                    daemon::run_daemon(
                        config.state_dir,
                        config.daemon_idle_timeout,
                        config.verbose,
                    )
                    .await
                    .map_err(|e| CliError::runtime("daemon_error", e))?;
                    Ok(ExitCode::SUCCESS)
                }
                #[cfg(not(unix))]
                {
                    Err(CliError::invalid_input(
                        "Transport daemon mode is only supported on Unix systems",
                    ))
                }
            }
            DaemonAction::Status => {
                let status = daemon_status_json(&config.state_dir).await;
                if json {
                    print_json(&status)?;
                } else {
                    print_daemon_status(&status);
                }
                Ok(ExitCode::SUCCESS)
            }
            DaemonAction::Stop => {
                #[cfg(unix)]
                {
                    let stopped = daemon::daemon_stop(&config.state_dir)
                        .await
                        .map_err(|e| CliError::runtime("daemon_error", e))?;
                    if json {
                        println!("{}", serde_json::json!({ "stopped": stopped }));
                    } else if !quiet {
                        if stopped {
                            println!("Stopped transport daemon");
                        } else {
                            println!("Transport daemon not running");
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
                #[cfg(not(unix))]
                {
                    Err(CliError::invalid_input(
                        "Transport daemon mode is only supported on Unix systems",
                    ))
                }
            }
        },
        Command::Session { action } => match action {
            SessionAction::Clear => {
                #[cfg(unix)]
                {
                    daemon::daemon_stop(&config.state_dir)
                        .await
                        .map_err(|e| CliError::runtime("session_error", e))?;
                    if daemon::is_daemon_running(&config.state_dir) {
                        return Err(CliError::runtime(
                            "session_error",
                            "daemon is still running after stop; cannot clear state directory",
                        ));
                    }
                }
                clear_state_dir(&config.state_dir)
                    .map_err(|e| CliError::runtime("session_error", e))?;
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "cleared": true,
                            "state_dir": config.state_dir.display().to_string(),
                        })
                    );
                } else if !quiet {
                    eprintln!("Cleared session state at {}", config.state_dir.display());
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Status => {
            let probe = cef::probe_renderer_in(&config.state_dir);
            let renderer_json = probe.to_status();
            let daemon_json = daemon_status_json(&config.state_dir).await;
            let all_creds = auth::resolve_all_provider_results(&config.state_dir)
                .map_err(|e| CliError::runtime("status_error", e))?;
            let search_providers =
                search::search_provider_statuses_from_results(&config.state_dir, &all_creds)
                    .map_err(|e| CliError::runtime("status_error", e))?;
            let agent_identity =
                auth::agent_identity_status_from_results(&all_creds, config.no_web_bot_auth);
            let cdp_access = auth::cdp_access_status_from_results(&all_creds);
            let cdp_renderer = build_cdp_status(&config, cdp_access);

            if json {
                let payload = StatusJson {
                    version: env!("CARGO_PKG_VERSION"),
                    cronet_enabled: cfg!(feature = "cronet"),
                    cef_renderer: renderer_json,
                    cdp_renderer,
                    daemon: daemon_json,
                    state_dir: config.state_dir.display().to_string(),
                    search_providers,
                    agent_identity,
                };
                print_json(&payload)?;
            } else {
                println!("pleb {}", env!("CARGO_PKG_VERSION"));
                println!(
                    "Cronet: {}",
                    if cfg!(feature = "cronet") {
                        "enabled"
                    } else {
                        "disabled (reqwest fallback — build with --features cronet for Chrome TLS/HTTP2/QUIC)"
                    }
                );
                match probe {
                    RendererProbe::Ready(path) => println!(
                        "CEF renderer: installed ({}, protocol {})",
                        path.display(),
                        cef::EXPECTED_PROTOCOL
                    ),
                    RendererProbe::ProtocolMismatch {
                        path,
                        found,
                        expected,
                    } => println!(
                        "CEF renderer: protocol mismatch at {} (found {}, expected {}; rebuild with: {})",
                        path.display(),
                        found.as_deref().unwrap_or("none"),
                        expected,
                        cef::rebuild_command_hint()
                    ),
                    RendererProbe::NotInstalled => println!("CEF renderer: not installed"),
                }
                print_cdp_status(&cdp_renderer);
                print_daemon_status(&daemon_json);
                println!("State dir: {}", config.state_dir.display());
                println!(
                    "Agent identity: {}",
                    format_agent_identity_summary(&agent_identity)
                );
                let configured: Vec<&str> = search_providers
                    .iter()
                    .filter(|(_, st)| st.configured)
                    .map(|(name, _)| name.as_str())
                    .collect();
                println!(
                    "Search providers: {}",
                    if configured.is_empty() {
                        "none configured (duckduckgo fallback)".to_string()
                    } else {
                        configured.join(", ")
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Skill { action } => match action {
            SkillAction::Print => {
                if json {
                    print_json(&serde_json::json!({
                        "name": "pleb",
                        "version": env!("CARGO_PKG_VERSION"),
                        "content": BUNDLED_SKILL_MD,
                    }))?;
                } else {
                    print!("{}", BUNDLED_SKILL_MD);
                }
                Ok(ExitCode::SUCCESS)
            }
            SkillAction::Install { dir, force } => {
                let entries = install_bundled_skill(dir, force)?;
                if json {
                    print_json(&serde_json::json!({
                        "name": "pleb",
                        "version": env!("CARGO_PKG_VERSION"),
                        "installed": entries,
                    }))?;
                } else if !quiet {
                    for entry in &entries {
                        match entry.status {
                            "up_to_date" => println!("Up to date: {}", entry.path),
                            _ => println!("Installed {}", entry.path),
                        }
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Mcp => {
            let ctx = Arc::new(
                RuntimeContext::new(config).map_err(|e| CliError::runtime("context_error", e))?,
            );
            pleb::mcp::serve_stdio(ctx)
                .await
                .map_err(|e| CliError::runtime("mcp_error", e))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Setup {
            target,
            scope,
            hooks,
            skip_skill,
            remove,
            dry_run,
        } => {
            let opts = setup::SetupOptions {
                target,
                claude_scope: scope,
                hooks,
                remove,
                dry_run,
                pleb_binary: None,
            };
            let mut report =
                setup::run_setup(&opts).map_err(|e| CliError::runtime("setup_error", e))?;
            if !skip_skill && !remove && !dry_run {
                let targets = resolve_setup_skill_targets(target)?;
                let installed = install_bundled_skill_targets(targets, false)?;
                report.skills = installed
                    .into_iter()
                    .map(|e| setup::SkillInstallStatus {
                        path: e.path,
                        status: e.status,
                    })
                    .collect();
            }
            if json {
                print_json(&report)?;
            } else if !quiet {
                let cef_probe = cef::probe_renderer_in(&config.state_dir);
                print_setup_report(&report, &cef_probe);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { action } => match action {
            HookAction::ClaudeWebfetch { mode } => {
                let mut stdin = std::io::stdin().lock();
                let mut stdout = std::io::stdout().lock();
                let mut stderr = std::io::stderr().lock();
                let code =
                    setup::run_claude_webfetch_hook(mode, &mut stdin, &mut stdout, &mut stderr)
                        .map_err(|e| CliError::runtime("hook_error", e))?;
                Ok(ExitCode::from(code))
            }
        },
        Command::Fetch {
            urls,
            no_navigate,
            format,
            extract,
            expand,
            reveal,
            scroll,
            render,
            source,
            resources,
            tools,
            stage_resources,
            referer,
            stage,
            wait_for_selector,
            respect_robots,
            no_rewrite,
            no_images,
            no_links,
            max_bytes,
            focus,
            section,
            offset,
            concurrency,
            jsonl,
            metadata_only,
            output,
        } => {
            if metadata_only && !(json || jsonl) {
                return Err(CliError::invalid_input(
                    "--metadata-only requires --json or --jsonl",
                ));
            }
            if !no_navigate && urls.is_empty() {
                return Err(CliError::invalid_input(
                    "a target URL (or '-' for stdin) is required unless --no-navigate is set",
                ));
            }
            let is_stdin = !no_navigate && urls.len() == 1 && urls[0] == "-";
            if !no_navigate && urls.len() > 1 && urls.iter().any(|u| u == "-") {
                return Err(CliError::invalid_input(
                    "stdin '-' cannot be mixed with positional URLs",
                ));
            }
            let batch_mode = !no_navigate && (is_stdin || urls.len() > 1);
            let mut expected_attach_origin: Option<String> = None;
            if no_navigate {
                if urls.len() > 1 {
                    return Err(CliError::invalid_input(
                        "--no-navigate accepts at most one optional expected-origin URL",
                    ));
                }
                if let Some(raw_expected) = urls.first() {
                    if raw_expected == "-" {
                        return Err(CliError::invalid_input(
                            "--no-navigate cannot be combined with stdin '-' batch mode",
                        ));
                    }
                    let parsed =
                        fetch::parse_http_url(raw_expected).map_err(CliError::from_fetch)?;
                    expected_attach_origin = Some(parsed.to_string());
                }
                if render != RenderMode::Cdp {
                    return Err(CliError::invalid_input(
                        "--no-navigate requires --render cdp",
                    ));
                }
                if config.proxy.is_some() {
                    return Err(CliError::invalid_input(
                        "--no-navigate cannot be combined with --proxy",
                    ));
                }
                let Some(cdp_raw) = config.cdp_url.as_deref() else {
                    return Err(CliError::invalid_input(
                        "--no-navigate requires --cdp-url or PLEB_CDP_URL pointing to ws://<loopback>/devtools/page/<id>",
                    ));
                };
                if let Err(err) = pleb::transport::cdp::validate_loopback_page_cdp_url(cdp_raw) {
                    return Err(CliError::invalid_input(err.to_string()));
                }
                if stage
                    || format == OutputFormat::Raw
                    || source == SourceMode::Negotiate
                    || respect_robots
                    || !matches!(resources, ResourcePolicy::None | ResourcePolicy::Manifest)
                    || !stage_resources.is_empty()
                {
                    return Err(CliError::invalid_input(
                        "--no-navigate forbids --stage, --format raw, --source negotiate, --respect-robots, --stage-resources, and network --resources modes",
                    ));
                }
            }

            if scroll > MAX_SCROLL_STEPS {
                return Err(CliError::invalid_input(format!(
                    "--scroll exceeds maximum {MAX_SCROLL_STEPS} steps"
                )));
            }
            let has_browser_actions = reveal != RevealPolicy::None || scroll > 0;
            if has_browser_actions && no_navigate {
                return Err(CliError::invalid_input(
                    "--no-navigate is passive and cannot be combined with --reveal or --scroll",
                ));
            }
            if has_browser_actions && render == RenderMode::Cronet {
                return Err(CliError::invalid_input(
                    "--reveal and --scroll require a browser renderer (--render cef, cdp, or auto)",
                ));
            }
            if has_browser_actions && (stage || format == OutputFormat::Raw) {
                return Err(CliError::invalid_input(
                    "--reveal and --scroll cannot be combined with --stage or --format raw",
                ));
            }
            if stage && batch_mode {
                return Err(CliError::invalid_input(
                    "--stage cannot be combined with batch mode",
                ));
            }
            if stage && format == OutputFormat::Raw {
                return Err(CliError::invalid_input(
                    "--stage cannot be combined with --format raw",
                ));
            }
            if tools == ToolPolicy::Manifest && (stage || format == OutputFormat::Raw) {
                return Err(CliError::invalid_input(
                    "--tools manifest cannot be combined with --stage or --format raw",
                ));
            }
            if expand != ExpandPolicy::None
                && (stage || matches!(format, OutputFormat::Raw | OutputFormat::Axtree))
            {
                return Err(CliError::invalid_input(
                    "--expand cannot be combined with --stage, --format raw, or --format axtree",
                ));
            }
            if format == OutputFormat::Axtree {
                if stage {
                    return Err(CliError::invalid_input(
                        "--format axtree cannot be combined with --stage",
                    ));
                }
                if matches!(render, RenderMode::Cronet | RenderMode::Cef) {
                    return Err(CliError::invalid_input(
                        "--format axtree requires CDP rendering (cannot use --render cronet or --render cef)",
                    ));
                }
                if config.cdp_url.is_none() {
                    return Err(CliError::invalid_input(
                        "--format axtree requires --cdp-url or PLEB_CDP_URL",
                    ));
                }
            }
            if format == OutputFormat::Raw && (json || jsonl || metadata_only || batch_mode) {
                return Err(CliError::invalid_input(
                    "--format raw cannot be combined with batch mode, --json, --jsonl, or --metadata-only",
                ));
            }
            let focus = normalize_optional_query(focus);
            let section = normalize_optional_query(section);
            validate_projection_flags(format, &focus, &section, offset, stage)?;
            if batch_mode && output.is_some() {
                return Err(CliError::invalid_input(
                    "--output cannot be combined with batch mode",
                ));
            }
            if format == OutputFormat::Raw && output.is_none() && std::io::stdout().is_terminal() {
                return Err(CliError::invalid_input(
                    "refusing to write raw bytes to a terminal; pass --output <path> or redirect stdout",
                ));
            }

            let target_urls = if is_stdin {
                let mut stdin_buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut stdin_buf)
                    .map_err(|e| CliError::runtime("io_error", e))?;
                let parsed = fetch::parse_batch_stdin(&stdin_buf);
                if parsed.is_empty() {
                    return Err(CliError::invalid_input("no URLs provided on stdin"));
                }
                parsed
            } else {
                urls
            };

            let ctx =
                RuntimeContext::new(config).map_err(|e| CliError::runtime("context_error", e))?;

            if stage {
                let url = &target_urls[0];
                let res = fetch::fetch_staged_asset(&ctx, url, referer.as_deref(), respect_robots)
                    .await
                    .map_err(CliError::from_fetch)?;
                if json {
                    print_json(&res)?;
                } else {
                    if !quiet {
                        eprintln!(
                            "Status: {} | Bytes: {} | Staged: {}",
                            res.status_code, res.byte_count, res.staged_path
                        );
                    }
                    println!("{}", res.staged_path);
                }
                return Ok(ExitCode::SUCCESS);
            }

            if format == OutputFormat::Raw {
                let url = &target_urls[0];
                let res = fetch::fetch_raw(&ctx, url, respect_robots)
                    .await
                    .map_err(CliError::from_fetch)?;

                if !quiet {
                    eprintln!(
                        "Status: {} | Time: {}ms | Bytes: {}",
                        res.status_code,
                        res.timing_ms,
                        res.bytes.len()
                    );
                }
                if let Some(ref path) = output {
                    std::fs::write(path, &res.bytes)
                        .map_err(|e| CliError::runtime("io_error", e))?;
                } else {
                    std::io::stdout()
                        .write_all(&res.bytes)
                        .map_err(|e| CliError::runtime("io_error", e))?;
                }
                return Ok(ExitCode::SUCCESS);
            }

            let stage_resources: Vec<String> = stage_resources
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            let opts = FetchOptions {
                extract: pleb::extract::ExtractOptions {
                    format,
                    mode: extract,
                    expand,
                    include_images: !no_images,
                    include_links: !no_links,
                    max_bytes,
                    focus,
                    section,
                    offset,
                },
                render,
                reveal,
                scroll_steps: scroll,
                source,
                resources,
                tools,
                stage_resources,
                referer,
                wait_for_selector,
                respect_robots,
                apply_rewrites: !no_rewrite,
                ..Default::default()
            };

            if !batch_mode {
                let (display_url, target) = if no_navigate {
                    (
                        expected_attach_origin
                            .clone()
                            .unwrap_or_else(|| "attached-cdp-page".to_string()),
                        fetch::FetchTarget::AttachCdp {
                            expected_origin: expected_attach_origin,
                        },
                    )
                } else {
                    let u = target_urls[0].clone();
                    (u.clone(), fetch::FetchTarget::Navigate(u))
                };
                let res = fetch::fetch_target(&ctx, target, &opts)
                    .await
                    .map_err(CliError::from_fetch)?;

                if jsonl {
                    let item = BatchItemResult::from_result(&display_url, Ok(res), metadata_only);
                    let encoded = serde_json::to_string(&item)
                        .map_err(|e| CliError::runtime("serialization_error", e))?;
                    println!("{encoded}");
                } else if json {
                    let encoded = if metadata_only {
                        serde_json::to_string(&FetchMetadataOutput::from(&res))
                    } else {
                        serde_json::to_string(&res)
                    }
                    .map_err(|e| CliError::runtime("serialization_error", e))?;

                    if let Some(ref path) = output {
                        std::fs::write(path, format!("{encoded}\n"))
                            .map_err(|e| CliError::runtime("io_error", e))?;
                    } else {
                        println!("{}", encoded);
                    }
                } else {
                    if !quiet {
                        if let Some(title) = &res.title {
                            eprintln!("Title: {}", title);
                        }
                        if opts.resources == ResourcePolicy::None && opts.stage_resources.is_empty()
                        {
                            if let Some(cue) = fetch::format_resource_cue(&res.resources) {
                                eprintln!("{cue}");
                            }
                        }
                        if opts.tools == ToolPolicy::None {
                            if let Some(cue) = pleb::extract::webmcp::format_webmcp_cue(&res.tools)
                            {
                                eprintln!("{cue}");
                            }
                        }
                        let allow_expand_hint =
                            matches!(opts.extract.mode, ExtractMode::Clean | ExtractMode::Article)
                                && matches!(
                                    opts.extract.format,
                                    OutputFormat::Markdown | OutputFormat::Text
                                );
                        if let Some(cue) =
                            pleb::extract::format_ui_gates_cue(&res.ui_gates, allow_expand_hint)
                        {
                            eprintln!("{cue}");
                        }
                        if let Some(staged) = fetch::format_staged_resources_line(&res.resources) {
                            eprintln!("{staged}");
                        }
                        if let Some(warning) = &res.warning {
                            eprintln!("Warning: {}", warning);
                        }
                        if let Some(next_offset) = res.next_offset {
                            eprintln!("Next-Offset: {}", next_offset);
                        }
                        eprintln!(
                            "Status: {} | Strategy: {} | Time: {}ms\n",
                            res.status_code, res.strategy, res.timing_ms
                        );
                    }
                    if let Some(ref path) = output {
                        std::fs::write(path, res.content.as_bytes())
                            .map_err(|e| CliError::runtime("io_error", e))?;
                    } else {
                        print!("{}", res.content);
                    }
                }
                return Ok(ExitCode::SUCCESS);
            }

            let batch_opts = BatchFetchOptions {
                fetch: opts,
                concurrency,
                metadata_only,
            };
            let mut results =
                fetch::fetch_batch(Arc::new(ctx), target_urls, batch_opts, |_, item| {
                    if jsonl {
                        if let Ok(line) = serde_json::to_string(item) {
                            let mut stdout = std::io::stdout().lock();
                            let _ = writeln!(stdout, "{line}");
                            let _ = stdout.flush();
                        }
                    }
                })
                .await;

            results.sort_by_key(|(idx, _)| *idx);
            let ordered: Vec<BatchItemResult> = results.into_iter().map(|(_, item)| item).collect();
            let exit_code = fetch::batch_exit_code(&ordered);

            if json {
                let encoded = serde_json::to_string(&ordered)
                    .map_err(|e| CliError::runtime("serialization_error", e))?;
                println!("{encoded}");
            } else if !jsonl {
                let total = ordered.len();
                let mut printed_any = false;
                for (i, item) in ordered.iter().enumerate() {
                    match item {
                        BatchItemResult::Success(s) => {
                            if !quiet {
                                if let Some(ref warning) = s.warning {
                                    eprintln!("warning [{}]: {}", s.url, warning);
                                }
                            }
                            if printed_any {
                                println!();
                            }
                            print!(
                                "{}",
                                format_page_section(
                                    i + 1,
                                    total,
                                    s.title.as_deref(),
                                    &s.final_url,
                                    s.content.as_deref().unwrap_or(""),
                                )
                            );
                            printed_any = true;
                        }
                        BatchItemResult::Failure(f) => {
                            eprintln!("error [{}]: {}", f.url, f.error);
                        }
                    }
                }
            }

            Ok(ExitCode::from(exit_code))
        }
        Command::Extract {
            input,
            url,
            content_type,
            format,
            extract,
            expand,
            resources,
            tools,
            no_images,
            no_links,
            max_bytes,
            focus,
            section,
            offset,
            max_input_bytes,
            metadata_only,
            output,
        } => {
            if config.max_response_bytes.is_some() {
                return Err(CliError::invalid_input(
                    "use --max-input-bytes with 'pleb extract' instead of --max-response-bytes",
                ));
            }
            if metadata_only && !json {
                return Err(CliError::invalid_input("--metadata-only requires --json"));
            }
            if matches!(format, OutputFormat::Raw | OutputFormat::Axtree) {
                return Err(CliError::invalid_input(
                    "offline extract supports only --format markdown, text, or html",
                ));
            }
            if !matches!(resources, ResourcePolicy::None | ResourcePolicy::Manifest) {
                return Err(CliError::invalid_input(
                    "offline extract supports only --resources none or manifest",
                ));
            }
            if max_input_bytes == 0 || max_input_bytes.checked_add(1).is_none() {
                return Err(CliError::invalid_input(
                    "--max-input-bytes must be a positive integer below usize::MAX",
                ));
            }
            let focus = normalize_optional_query(focus);
            let section = normalize_optional_query(section);
            validate_projection_flags(format, &focus, &section, offset, false)?;

            let parsed_url = fetch::parse_http_url(&url).map_err(CliError::from_fetch)?;
            let read_limit = max_input_bytes.checked_add(1).expect("checked above");
            let raw_bytes = match input.as_deref() {
                None => read_bounded_stdin(read_limit)?,
                Some(p) if p == std::path::Path::new("-") => read_bounded_stdin(read_limit)?,
                Some(path) => {
                    let file =
                        std::fs::File::open(path).map_err(|e| CliError::runtime("io_error", e))?;
                    let mut buf = Vec::new();
                    file.take(read_limit as u64)
                        .read_to_end(&mut buf)
                        .map_err(|e| CliError::runtime("io_error", e))?;
                    buf
                }
            };
            if raw_bytes.len() > max_input_bytes {
                return Err(CliError::runtime(
                    "response_too_large",
                    format!("input exceeded {max_input_bytes} byte limit"),
                ));
            }

            if let Some(detected_mime) = pleb::extract::detect_binary_content_with_magic_sniff(
                Some(&content_type),
                &raw_bytes,
                true,
            ) {
                return Err(CliError {
                    message: format!(
                        "unsupported binary content ({}) in offline extract ({} bytes)",
                        detected_mime.as_deref().unwrap_or("binary"),
                        raw_bytes.len()
                    ),
                    kind: "unsupported_content_type",
                    block_reason: None,
                    exit_code: 5,
                });
            }

            let start = std::time::Instant::now();
            let acquired_sha256 = fetch::sha256_hex(&raw_bytes);
            let extract_opts = pleb::extract::ExtractOptions {
                format,
                mode: extract,
                expand,
                include_images: !no_images,
                include_links: !no_links,
                max_bytes,
                focus,
                section,
                offset,
            };
            let initial_opts = pleb::extract::ExtractOptions {
                max_bytes: None,
                focus: None,
                section: None,
                offset: 0,
                ..extract_opts.clone()
            };
            let mut doc = pleb::extract::extract_document_bytes(
                &raw_bytes,
                &parsed_url,
                Some(&content_type),
                &initial_opts,
            )
            .map_err(|e| CliError::runtime("extract_error", e))?;

            pleb::extract::finalize_extracted_document(
                &mut doc,
                &extract_opts,
                resources == ResourcePolicy::Manifest,
                tools == ToolPolicy::Manifest,
            );
            let timing_ms = start.elapsed().as_millis() as u64;
            let content_bytes = doc.content.len();

            if json {
                let payload = ExtractOutput {
                    url: parsed_url.as_str(),
                    content_type: &content_type,
                    title: doc.title.as_deref(),
                    content: (!metadata_only).then_some(doc.content.as_str()),
                    content_bytes,
                    acquired_sha256: &acquired_sha256,
                    truncated: doc.truncated,
                    next_offset: doc.next_offset,
                    timing_ms,
                    warning: doc.warning.as_deref(),
                    resources: &doc.resources,
                    tools: &doc.tools,
                    ui_gates: &doc.ui_gates,
                };
                let encoded = serde_json::to_string(&payload)
                    .map_err(|e| CliError::runtime("serialization_error", e))?;
                if let Some(ref path) = output {
                    write_file_atomic(path, format!("{encoded}\n").as_bytes())
                        .map_err(|e| CliError::runtime("io_error", e))?;
                } else {
                    println!("{encoded}");
                }
            } else {
                if !quiet {
                    if let Some(ref title) = doc.title {
                        eprintln!("Title: {title}");
                    }
                    if resources == ResourcePolicy::None {
                        if let Some(cue) = fetch::format_resource_cue(&doc.resources) {
                            eprintln!("{cue}");
                        }
                    }
                    if tools == ToolPolicy::None {
                        if let Some(cue) = pleb::extract::webmcp::format_webmcp_cue(&doc.tools) {
                            eprintln!("{cue}");
                        }
                    }
                    let allow_expand_hint =
                        matches!(extract, ExtractMode::Clean | ExtractMode::Article)
                            && matches!(format, OutputFormat::Markdown | OutputFormat::Text);
                    if let Some(cue) =
                        pleb::extract::format_ui_gates_cue(&doc.ui_gates, allow_expand_hint)
                    {
                        eprintln!("{cue}");
                    }
                    if let Some(ref warning) = doc.warning {
                        eprintln!("Warning: {warning}");
                    }
                    if let Some(next_offset) = doc.next_offset {
                        eprintln!("Next-Offset: {next_offset}");
                    }
                    eprintln!("Bytes: {content_bytes} | Time: {timing_ms}ms\n");
                }
                if let Some(ref path) = output {
                    write_file_atomic(path, doc.content.as_bytes())
                        .map_err(|e| CliError::runtime("io_error", e))?;
                } else {
                    print!("{}", doc.content);
                }
            }

            Ok(ExitCode::SUCCESS)
        }
        Command::Search {
            query,
            limit,
            provider,
            mode,
            urls_only,
            freshness,
            site,
            extra_snippets,
        } => {
            let ctx =
                RuntimeContext::new(config).map_err(|e| CliError::runtime("context_error", e))?;
            let opts = SearchOptions {
                limit,
                provider,
                mode,
                freshness,
                site,
                extra_snippets,
            };
            let response = search::search(&ctx, &query, &opts)
                .await
                .map_err(|e| CliError::runtime("search_error", e))?;
            if urls_only {
                if !response.results.is_empty() {
                    println!("{}", search::format_urls_only(&response));
                }
            } else if json {
                print_json(&response)?;
            } else {
                println!("{}", search::format_results(&response));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Auth { action } => match action {
            AuthAction::Set {
                provider,
                stdin,
                storage,
            } => {
                let secret = if stdin {
                    read_secret_from_stdin(provider)?
                } else {
                    read_secret_from_tty(provider)?
                };
                let source =
                    auth::set_credential(&config.state_dir, provider, secret.as_str(), storage)
                        .map_err(|e| CliError::runtime("auth_error", e))?;
                drop(secret);
                if provider.is_search_provider() {
                    search::clear_provider_cooldown(&config.state_dir, provider)
                        .map_err(|e| CliError::runtime("auth_error", e))?;
                }
                if json {
                    print_json(&serde_json::json!({
                        "provider": provider.as_str(),
                        "stored": true,
                        "source": source,
                    }))?;
                } else if !quiet {
                    println!(
                        "Stored {} credential ({})",
                        provider.as_str(),
                        credential_source_label(source)
                    );
                }
                Ok(ExitCode::SUCCESS)
            }
            AuthAction::Remove { provider, storage } => {
                let removed = auth::remove_credential(&config.state_dir, provider, storage)
                    .map_err(|e| CliError::runtime("auth_error", e))?;
                if provider.is_search_provider() {
                    search::clear_provider_cooldown(&config.state_dir, provider)
                        .map_err(|e| CliError::runtime("auth_error", e))?;
                }
                let env_var_still_set = std::env::var(provider.env_var())
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false);
                if env_var_still_set {
                    eprintln!(
                        "warning: {} is still set in the environment and will continue to take precedence",
                        provider.env_var()
                    );
                }
                if json {
                    print_json(&serde_json::json!({
                        "provider": provider.as_str(),
                        "removed": !removed.is_empty(),
                        "removed_from": removed,
                        "env_var_still_set": env_var_still_set,
                    }))?;
                } else if !quiet {
                    if removed.is_empty() {
                        println!("No stored {} credential found", provider.as_str());
                    } else {
                        let stores: Vec<&'static str> = removed
                            .iter()
                            .copied()
                            .map(credential_source_label)
                            .collect();
                        println!(
                            "Removed {} credential ({})",
                            provider.as_str(),
                            stores.join(", ")
                        );
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            AuthAction::Status => {
                let all_creds = auth::resolve_all_provider_results(&config.state_dir)
                    .map_err(|e| CliError::runtime("auth_error", e))?;
                let search_providers =
                    search::search_provider_statuses_from_results(&config.state_dir, &all_creds)
                        .map_err(|e| CliError::runtime("auth_error", e))?;
                let agent_identity =
                    auth::agent_identity_status_from_results(&all_creds, config.no_web_bot_auth);
                let cdp_access = auth::cdp_access_status_from_results(&all_creds);
                let payload = AuthStatusJson {
                    search_providers,
                    agent_identity,
                    cdp_access,
                };
                if json {
                    print_json(&payload)?;
                } else {
                    print_auth_statuses(&payload);
                }
                Ok(ExitCode::SUCCESS)
            }
            AuthAction::InitWebBotAuth {
                agent,
                storage,
                force,
            } => {
                let res = auth::init_web_bot_auth(&config.state_dir, &agent, storage, force)
                    .map_err(|e| CliError::runtime("auth_error", e))?;
                for provider in [
                    CredentialProvider::WebBotAuthJwk,
                    CredentialProvider::WebBotAuthAgent,
                ] {
                    if std::env::var(provider.env_var())
                        .map(|v| !v.trim().is_empty())
                        .unwrap_or(false)
                    {
                        eprintln!(
                            "warning: {} is set in the environment and will take precedence over the stored credential",
                            provider.env_var()
                        );
                    }
                }
                if json {
                    print_json(&res)?;
                } else if !quiet {
                    println!("Initialized Web Bot Auth identity (keyid: {})", res.keyid);
                    println!(
                        "  Agent URL: {} ({})",
                        res.agent_url,
                        credential_source_label(res.agent_source)
                    );
                    println!(
                        "  Private JWK: stored ({})",
                        credential_source_label(res.jwk_source)
                    );
                    println!("  Publish this JSON at {}:", res.directory_url);
                    let pretty = serde_json::to_string_pretty(&res.directory_json)
                        .map_err(|e| CliError::runtime("serialization_error", e))?;
                    println!("{pretty}");
                }
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Crawl {
            url,
            depth,
            max_pages,
            extract,
            path_filter,
            no_robots,
        } => {
            let ctx =
                RuntimeContext::new(config).map_err(|e| CliError::runtime("context_error", e))?;
            let options = crawl::CrawlOptions {
                max_depth: depth.min(5),
                max_pages: max_pages.min(50),
                format: OutputFormat::Markdown,
                extract_mode: extract,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::crawl(&ctx, &url, options).await.map_err(|e| {
                match e.downcast::<FetchError>() {
                    Ok(fetch_err) => CliError::from_fetch(fetch_err),
                    Err(other) => CliError::runtime("crawl_error", other),
                }
            })?;
            if json {
                print_json(&result)?;
            } else {
                let host = crawl::url_host_or_raw(&url);
                if !quiet {
                    eprintln!(
                        "Crawled {} pages in {:.1}s\n",
                        result.pages.len(),
                        result.timing_ms as f64 / 1000.0
                    );
                }
                print!("{}", crawl::format_crawl_output(&result, &host));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Map {
            url,
            limit,
            no_sitemap,
            path_filter,
            no_robots,
        } => {
            let ctx =
                RuntimeContext::new(config).map_err(|e| CliError::runtime("context_error", e))?;
            let options = crawl::MapOptions {
                limit: limit.min(5000),
                use_sitemap: !no_sitemap,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::map(&ctx, &url, options)
                .await
                .map_err(|e| CliError::runtime("map_error", e))?;
            if json {
                print_json(&result)?;
            } else {
                let host = crawl::url_host_or_raw(&url);
                if !quiet {
                    eprintln!(
                        "Found {} URLs in {:.1}s\n",
                        result.urls.len(),
                        result.timing_ms as f64 / 1000.0
                    );
                }
                print!("{}", crawl::format_map_output(&result, &host));
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<(), CliError> {
    let encoded =
        serde_json::to_string(value).map_err(|e| CliError::runtime("serialization_error", e))?;
    println!("{encoded}");
    Ok(())
}

fn print_daemon_status(status: &DaemonStatusJson) {
    if !status.supported {
        println!("Transport daemon: unsupported on this OS");
    } else if status.running {
        println!(
            "Transport daemon: running (pid {}, {} active, {} slot(s), {} CEF warm)",
            status.pid.unwrap_or(0),
            status.active_requests,
            status.cronet_slots,
            status.cef_renderers_alive
        );
    } else {
        println!("Transport daemon: stopped");
    }
}

async fn daemon_status_json(state_dir: &std::path::Path) -> DaemonStatusJson {
    #[cfg(unix)]
    {
        match daemon::daemon_ping(state_dir).await {
            Some(st) => DaemonStatusJson {
                supported: true,
                running: true,
                pid: Some(st.pid),
                protocol_version: Some(st.protocol_version),
                build_id: Some(st.build_id),
                active_requests: st.active_requests,
                cronet_slots: st.cronet_slots,
                cef_renderers_alive: st.cef_renderers_alive,
            },
            None => DaemonStatusJson {
                supported: true,
                ..Default::default()
            },
        }
    }
    #[cfg(not(unix))]
    {
        let _ = state_dir;
        DaemonStatusJson::default()
    }
}

const MAX_SECRET_INPUT_BYTES: usize = 4096;

fn validate_secret_input(
    provider: CredentialProvider,
    raw: &mut [u8],
) -> Result<SecretString, CliError> {
    if raw.len() > MAX_SECRET_INPUT_BYTES {
        auth::zeroize_bytes(raw);
        return Err(CliError::invalid_input(
            "credential input exceeds 4096-byte limit",
        ));
    }
    let result = match std::str::from_utf8(raw) {
        Ok(text) => {
            let trimmed = text.trim();
            let allow_json_ws = provider == CredentialProvider::WebBotAuthJwk;
            let has_disallowed_byte = trimmed.bytes().any(|b| {
                !b.is_ascii()
                    || (b.is_ascii_control()
                        && !(allow_json_ws && matches!(b, b'\n' | b'\r' | b'\t')))
            });
            if trimmed.is_empty() {
                Err(CliError::invalid_input("credential cannot be empty"))
            } else if has_disallowed_byte {
                Err(CliError::invalid_input(if allow_json_ws {
                    "credential must contain printable ASCII or JSON whitespace only"
                } else {
                    "credential must be a single line of printable ASCII characters"
                }))
            } else {
                Ok(SecretString::new(trimmed.to_string()))
            }
        }
        Err(_) => Err(CliError::invalid_input(
            "credential must be printable ASCII",
        )),
    };
    auth::zeroize_bytes(raw);
    result
}

fn read_secret_from_stdin(provider: CredentialProvider) -> Result<SecretString, CliError> {
    let mut buf = Vec::new();
    let mut limited = std::io::stdin().take((MAX_SECRET_INPUT_BYTES + 1) as u64);
    if let Err(err) = limited.read_to_end(&mut buf) {
        auth::zeroize_bytes(&mut buf);
        return Err(CliError::runtime("io_error", err));
    }
    validate_secret_input(provider, &mut buf)
}

#[cfg(unix)]
fn read_secret_from_tty(provider: CredentialProvider) -> Result<SecretString, CliError> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::AtomicBool;

    static TTY_INTERRUPTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn handle_tty_interrupt(_sig: libc::c_int) {
        TTY_INTERRUPTED.store(true, Ordering::SeqCst);
    }

    struct EchoGuard {
        fd: libc::c_int,
        orig_termios: libc::termios,
        prev_sigint: libc::sigaction,
        prev_sigterm: libc::sigaction,
    }

    impl Drop for EchoGuard {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.orig_termios);
                libc::sigaction(libc::SIGINT, &self.prev_sigint, std::ptr::null_mut());
                libc::sigaction(libc::SIGTERM, &self.prev_sigterm, std::ptr::null_mut());
            }
            let _ = writeln!(std::io::stderr());
        }
    }

    if !std::io::stdin().is_terminal() {
        return Err(CliError::invalid_input(
            "stdin is not a terminal; pass --stdin to read a credential from a pipe",
        ));
    }

    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty")
        .map_err(|e| CliError::runtime("io_error", e))?;
    let fd = tty.as_raw_fd();

    let mut orig_termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut orig_termios) } != 0 {
        return Err(CliError::runtime(
            "io_error",
            std::io::Error::last_os_error(),
        ));
    }

    TTY_INTERRUPTED.store(false, Ordering::SeqCst);

    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = handle_tty_interrupt as *const () as libc::sighandler_t;
    sa.sa_flags = 0; // Explicitly omit SA_RESTART so Ctrl-C interrupts read() with EINTR.
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
    }

    let mut prev_sigint: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGINT, &sa, &mut prev_sigint) } != 0 {
        return Err(CliError::runtime(
            "io_error",
            std::io::Error::last_os_error(),
        ));
    }

    let mut prev_sigterm: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGTERM, &sa, &mut prev_sigterm) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::sigaction(libc::SIGINT, &prev_sigint, std::ptr::null_mut());
        }
        return Err(CliError::runtime("io_error", err));
    }

    let mut noecho = orig_termios;
    noecho.c_lflag &= !(libc::ECHO | libc::ECHONL);
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &noecho) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::sigaction(libc::SIGINT, &prev_sigint, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &prev_sigterm, std::ptr::null_mut());
        }
        return Err(CliError::runtime("io_error", err));
    }

    let guard = EchoGuard {
        fd,
        orig_termios,
        prev_sigint,
        prev_sigterm,
    };

    eprint!("Enter {} ({}): ", provider.as_str(), provider.env_var());
    let _ = std::io::stderr().flush();

    let mut buf = Vec::new();
    let mut byte = 0u8;
    loop {
        let n = unsafe { libc::read(fd, (&mut byte as *mut u8).cast::<libc::c_void>(), 1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) || TTY_INTERRUPTED.load(Ordering::SeqCst) {
                auth::zeroize_bytes(&mut buf);
                drop(guard);
                return Err(CliError {
                    message: "interrupted".to_string(),
                    kind: "interrupted",
                    block_reason: None,
                    exit_code: 130,
                });
            }
            auth::zeroize_bytes(&mut buf);
            drop(guard);
            return Err(CliError::runtime("io_error", err));
        }
        if n == 0 || byte == b'\n' {
            break;
        }
        buf.push(byte);
        if buf.len() > MAX_SECRET_INPUT_BYTES {
            break;
        }
    }

    drop(guard);
    if TTY_INTERRUPTED.load(Ordering::SeqCst) {
        auth::zeroize_bytes(&mut buf);
        return Err(CliError {
            message: "interrupted".to_string(),
            kind: "interrupted",
            block_reason: None,
            exit_code: 130,
        });
    }

    validate_secret_input(provider, &mut buf)
}

#[cfg(not(unix))]
fn read_secret_from_tty(_provider: CredentialProvider) -> Result<SecretString, CliError> {
    if !std::io::stdin().is_terminal() {
        return Err(CliError::invalid_input(
            "stdin is not a terminal; pass --stdin to read a credential from a pipe",
        ));
    }
    Err(CliError::invalid_input(
        "interactive TTY credential prompts are only supported on Unix; pass --stdin",
    ))
}

fn credential_source_label(source: CredentialSource) -> &'static str {
    match source {
        CredentialSource::Environment => "environment",
        CredentialSource::Keychain => "keychain",
        CredentialSource::File => "file",
    }
}

fn build_cdp_status(config: &RuntimeConfig, access: auth::CdpAccessStatus) -> CdpStatusJson {
    let configured = config.cdp_url.is_some();
    let url = config
        .cdp_url
        .as_deref()
        .map(cdp::sanitize_cdp_url_for_display);
    let proxy_blocked = configured && config.proxy.is_some();
    let status_error = if let Some(raw) = config.cdp_url.as_deref() {
        if let Err(err) = cdp::validate_cdp_url(raw) {
            Some(err.to_string())
        } else if config.proxy.is_some() {
            Some("CDP renderer disabled while --proxy / PLEB_PROXY is active".to_string())
        } else {
            access.status_error.clone()
        }
    } else {
        access.status_error.clone()
    };
    CdpStatusJson {
        configured,
        url,
        prefer_cdp: config.prefer_cdp,
        proxy_blocked,
        access,
        status_error,
    }
}

fn print_cdp_status(status: &CdpStatusJson) {
    if !status.configured {
        println!("CDP renderer: not configured");
    } else {
        let mut line = format!(
            "CDP renderer: {} (prefer_cdp: {})",
            status.url.as_deref().unwrap_or("[invalid-cdp-url]"),
            status.prefer_cdp
        );
        if status.proxy_blocked {
            line.push_str(" [blocked by proxy]");
        }
        if status.access.configured {
            if let Some(ref host) = status.access.access_host {
                line.push_str(&format!(" | access_host: {host}"));
            }
        }
        if let Some(ref err) = status.status_error {
            line.push_str(&format!(" | error: {err}"));
        }
        println!("{line}");
    }
}

fn format_agent_identity_summary(status: &auth::AgentIdentityStatus) -> String {
    let mode = if !status.web_bot_auth_enabled {
        "disabled (--no-web-bot-auth)"
    } else if status.jwk_configured && status.agent_configured {
        "configured"
    } else if status.jwk_configured || status.agent_configured {
        "incomplete"
    } else {
        "not configured"
    };
    let mut summary = mode.to_string();
    if let Some(ref keyid) = status.keyid {
        summary.push_str(&format!(" (keyid: {keyid})"));
    }
    if let Some(ref err) = status.status_error {
        summary.push_str(&format!(" | error: {err}"));
    }
    summary
}

fn format_cdp_access_summary(status: &auth::CdpAccessStatus) -> String {
    let mut summary = if status.configured {
        match status.access_host.as_deref() {
            Some(host) => format!("configured (host: {host})"),
            None => "configured".to_string(),
        }
    } else if status.client_id_configured
        || status.client_secret_configured
        || status.host_configured
    {
        "incomplete".to_string()
    } else {
        "not configured".to_string()
    };
    if let Some(ref err) = status.status_error {
        summary.push_str(&format!(" | error: {err}"));
    }
    summary
}

fn print_auth_statuses(statuses: &AuthStatusJson) {
    for (provider, status) in &statuses.search_providers {
        let mut line = if status.configured {
            let source = status
                .source
                .map(credential_source_label)
                .unwrap_or("configured");
            format!("{provider}: configured ({source})")
        } else {
            format!("{provider}: not configured")
        };
        if status.month_requests > 0 {
            line.push_str(&format!(" | month_requests: {}", status.month_requests));
        }
        if status.cooldown_remaining_secs > 0 {
            line.push_str(&format!(" | cooldown: {}s", status.cooldown_remaining_secs));
        }
        if let Some(code) = status.last_error_status {
            line.push_str(&format!(" | last_error: HTTP {code}"));
        }
        println!("{line}");
    }
    println!(
        "web-bot-auth: {}",
        format_agent_identity_summary(&statuses.agent_identity)
    );
    println!(
        "cdp-access: {}",
        format_cdp_access_summary(&statuses.cdp_access)
    );
}

fn init_tracing(verbose: bool) {
    let default_spec = if verbose {
        "warn,pleb=debug,reqwest=warn,hyper=warn,hyper_util=warn,h2=warn,rustls=warn"
    } else {
        "warn,reqwest=warn,hyper=warn,hyper_util=warn,h2=warn,rustls=warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_spec));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[derive(Serialize)]
struct SkillInstallEntry {
    path: String,
    status: &'static str,
}

fn resolve_home_dir() -> Result<PathBuf, CliError> {
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty());
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()));
    home.map(PathBuf::from).ok_or_else(|| {
        CliError::runtime(
            "skill_error",
            "cannot determine home directory (HOME is unset); pass --dir <PATH>",
        )
    })
}

fn resolve_setup_skill_targets(target: setup::SetupTarget) -> Result<Vec<PathBuf>, CliError> {
    match target {
        setup::SetupTarget::All => resolve_skill_install_targets(None),
        setup::SetupTarget::Claude => Ok(vec![resolve_home_dir()?.join(".claude/skills/pleb")]),
        setup::SetupTarget::Codex => Ok(vec![resolve_home_dir()?.join(".codex/skills/pleb")]),
    }
}

fn print_setup_report(report: &setup::SetupReport, cef_probe: &RendererProbe) {
    println!("pleb setup (binary: {})", report.pleb_binary);
    for entry in &report.results {
        let mut line = format!("  {}: {}", entry.target, entry.status);
        if let Some(ref detail) = entry.detail {
            line.push_str(&format!(" ({detail})"));
        }
        if entry.status == "planned" || entry.status == "not_found" {
            line.push_str(&format!(" -> {}", entry.command));
        }
        println!("{line}");
    }
    for skill in &report.skills {
        println!("  skill ({}): {}", skill.status, skill.path);
    }
    if let Some(ref hook) = report.hook {
        let mut line = format!(
            "  claude hook ({}, mode={}): {}",
            hook.status, hook.mode, hook.settings_path
        );
        if let Some(ref detail) = hook.detail {
            line.push_str(&format!(" ({detail})"));
        }
        println!("{line}");
    }
    println!(
        "  Cronet TLS: {}",
        if cfg!(feature = "cronet") {
            "enabled (Chromium 154)"
        } else {
            "disabled (reqwest fallback — build with --features cronet for Chrome TLS/HTTP2/QUIC)"
        }
    );
    match cef_probe {
        RendererProbe::Ready(path) => {
            println!("  CEF renderer: ready ({})", path.display());
        }
        RendererProbe::ProtocolMismatch { path, .. } => {
            println!(
                "  CEF renderer: protocol mismatch at {} (rebuild: {})",
                path.display(),
                cef::rebuild_command_hint()
            );
        }
        RendererProbe::NotInstalled => {
            println!(
                "  CEF renderer: not installed (optional SPA escalation: {})",
                cef::rebuild_command_hint()
            );
        }
    }
}

fn resolve_skill_install_targets(dir: Option<PathBuf>) -> Result<Vec<PathBuf>, CliError> {
    if let Some(explicit) = dir {
        if explicit.as_os_str().is_empty() {
            return Err(CliError::invalid_input("--dir must not be empty"));
        }
        return Ok(vec![explicit]);
    }
    let home = resolve_home_dir()?;
    Ok(vec![
        home.join(".agents/skills/pleb"),
        home.join(".gemini/config/skills/pleb"),
        home.join(".claude/skills/pleb"),
        home.join(".codex/skills/pleb"),
        home.join(".config/opencode/skills/pleb"),
    ])
}

fn normalize_optional_query(val: Option<String>) -> Option<String> {
    val.map(|q| q.trim().to_string()).filter(|q| !q.is_empty())
}

fn validate_projection_flags(
    format: OutputFormat,
    focus: &Option<String>,
    section: &Option<String>,
    offset: usize,
    stage: bool,
) -> Result<(), CliError> {
    if focus.is_some() && (stage || !matches!(format, OutputFormat::Markdown | OutputFormat::Text))
    {
        return Err(CliError::invalid_input(
            "--focus is only supported with --format markdown or --format text (cannot be combined with --stage, --format html, --format raw, or --format axtree)",
        ));
    }
    if section.is_some() && focus.is_some() {
        return Err(CliError::invalid_input(
            "--section cannot be combined with --focus",
        ));
    }
    if offset > 0 && focus.is_some() {
        return Err(CliError::invalid_input(
            "--offset cannot be combined with --focus",
        ));
    }
    if section.is_some() && (stage || format != OutputFormat::Markdown) {
        return Err(CliError::invalid_input(
            "--section is only supported with --format markdown (cannot be combined with --stage, --format text, --format html, --format raw, or --format axtree)",
        ));
    }
    if offset > 0 && (stage || format == OutputFormat::Raw) {
        return Err(CliError::invalid_input(
            "--offset cannot be combined with --stage or --format raw",
        ));
    }
    Ok(())
}

fn read_bounded_stdin(read_limit: usize) -> Result<Vec<u8>, CliError> {
    if std::io::stdin().is_terminal() {
        return Err(CliError::invalid_input(
            "no input provided on stdin; pipe content or pass a file path",
        ));
    }
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .take(read_limit as u64)
        .read_to_end(&mut buf)
        .map_err(|e| CliError::runtime("io_error", e))?;
    Ok(buf)
}

fn write_file_atomic(dest: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)?;
    if let Err(err) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    drop(file);
    if let Err(err) = std::fs::rename(&tmp_path, dest) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

fn write_skill_file_atomic(skill_path: &std::path::Path, content: &str) -> std::io::Result<()> {
    write_file_atomic(skill_path, content.as_bytes())
}

fn install_bundled_skill(
    dir: Option<PathBuf>,
    force: bool,
) -> Result<Vec<SkillInstallEntry>, CliError> {
    let targets = resolve_skill_install_targets(dir)?;
    install_bundled_skill_targets(targets, force)
}

fn install_bundled_skill_targets(
    targets: Vec<PathBuf>,
    force: bool,
) -> Result<Vec<SkillInstallEntry>, CliError> {
    let mut entries = Vec::with_capacity(targets.len());
    for dest in targets {
        let skill_path = dest.join("SKILL.md");
        match std::fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if !force {
                    if let Ok(existing) = std::fs::read_to_string(&skill_path) {
                        if existing == BUNDLED_SKILL_MD {
                            entries.push(SkillInstallEntry {
                                path: skill_path.display().to_string(),
                                status: "up_to_date",
                            });
                            continue;
                        }
                    }
                    return Err(CliError::runtime(
                        "skill_error",
                        format!(
                            "refusing to overwrite symlink at {}; pass --force to replace",
                            dest.display()
                        ),
                    ));
                }
                std::fs::remove_file(&dest).map_err(|e| CliError::runtime("skill_error", e))?;
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(CliError::runtime(
                    "skill_error",
                    format!(
                        "skill target exists and is not a directory: {}",
                        dest.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(CliError::runtime("skill_error", err)),
        }

        std::fs::create_dir_all(&dest).map_err(|e| CliError::runtime("skill_error", e))?;

        match std::fs::symlink_metadata(&skill_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if !force {
                    return Err(CliError::runtime(
                        "skill_error",
                        format!(
                            "refusing to overwrite symlink {}; pass --force to replace",
                            skill_path.display()
                        ),
                    ));
                }
                std::fs::remove_file(&skill_path)
                    .map_err(|e| CliError::runtime("skill_error", e))?;
            }
            Ok(meta) if !meta.is_file() => {
                return Err(CliError::runtime(
                    "skill_error",
                    format!(
                        "refusing to overwrite non-file path {}",
                        skill_path.display()
                    ),
                ));
            }
            Ok(_) => {
                let existing = std::fs::read_to_string(&skill_path)
                    .map_err(|e| CliError::runtime("skill_error", e))?;
                if existing == BUNDLED_SKILL_MD {
                    entries.push(SkillInstallEntry {
                        path: skill_path.display().to_string(),
                        status: "up_to_date",
                    });
                    continue;
                }
                if !force {
                    return Err(CliError::runtime(
                        "skill_error",
                        format!(
                            "refusing to overwrite modified {}; pass --force to overwrite",
                            skill_path.display()
                        ),
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(CliError::runtime("skill_error", err)),
        }

        write_skill_file_atomic(&skill_path, BUNDLED_SKILL_MD)
            .map_err(|e| CliError::runtime("skill_error", e))?;
        entries.push(SkillInstallEntry {
            path: skill_path.display().to_string(),
            status: "installed",
        });
    }
    Ok(entries)
}

fn emit_cli_error(err: &CliError, json: bool) {
    if json {
        let mut payload = serde_json::json!({
            "error": err.message,
            "kind": err.kind,
            "exit_code": err.exit_code,
        });
        if let Some(reason) = err.block_reason {
            payload["block_reason"] = serde_json::json!(reason);
        }
        eprintln!("{}", payload);
    } else {
        eprintln!("error: {}", err.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_secret_input_allows_multiline_only_for_web_bot_auth_jwk() {
        let mut multiline_jwk =
            b"{\n  \"kty\": \"OKP\",\n  \"crv\": \"Ed25519\",\n  \"x\": \"abc\",\n  \"d\": \"def\"\n}\n"
                .to_vec();
        let ok = validate_secret_input(CredentialProvider::WebBotAuthJwk, &mut multiline_jwk)
            .expect("WebBotAuthJwk should accept JSON whitespace");
        assert!(ok.as_str().contains("\"kty\""));
        assert!(multiline_jwk.iter().all(|&b| b == 0));

        let mut multiline_brave = b"secret-1\nsecret-2\n".to_vec();
        assert!(validate_secret_input(CredentialProvider::Brave, &mut multiline_brave).is_err());
        assert!(multiline_brave.iter().all(|&b| b == 0));

        let mut nul_jwk = b"{\"kty\":\"OKP\"\x00}".to_vec();
        assert!(validate_secret_input(CredentialProvider::WebBotAuthJwk, &mut nul_jwk).is_err());
        assert!(nul_jwk.iter().all(|&b| b == 0));
    }
}
