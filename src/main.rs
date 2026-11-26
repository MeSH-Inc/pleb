use clap::{Parser, Subcommand};
use pleb::auth::{self, CredentialProvider, CredentialSource, CredentialStorage, SecretString};
use pleb::context::{clear_state_dir, default_state_dir, RuntimeConfig, RuntimeContext};
use pleb::crawl;
#[cfg(unix)]
use pleb::daemon;
use pleb::extract::{format_page_section, ExtractMode, OutputFormat};
use pleb::fetch::{
    self, BatchFetchOptions, BatchItemResult, FetchError, FetchOptions, FetchOutput,
};
use pleb::policy::{RenderMode, ResourcePolicy, SourceMode};
use pleb::search::{
    self, Freshness, SearchMode, SearchOptions, SearchProvider, SearchProviderStatus,
};
use pleb::transport::cef::{self, RendererProbe};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{compiler_fence, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "pleb",
    version,
    about = "Local browser-grade web fetch and search CLI for AI agents"
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
        /// Target URL(s) (http/https), or '-' to read URLs from stdin
        #[arg(required = true)]
        urls: Vec<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Markdown, ignore_case = true)]
        format: OutputFormat,
        /// Content extraction scope
        #[arg(long, value_enum, default_value_t = ExtractMode::Clean, ignore_case = true)]
        extract: ExtractMode,
        /// Transport mode
        #[arg(long, value_enum, default_value_t = RenderMode::Auto, ignore_case = true)]
        render: RenderMode,
        /// Structured source negotiation mode
        #[arg(long, value_enum, default_value_t = SourceMode::Auto, ignore_case = true)]
        source: SourceMode,
        /// Linked resource indexing and staging policy
        #[arg(long, value_enum, default_value_t = ResourcePolicy::None, ignore_case = true)]
        resources: ResourcePolicy,
        /// Selectively stage or inline specific resource IDs (comma-separated, e.g. r1,r3)
        #[arg(long, value_delimiter = ',')]
        stage_resources: Vec<String>,
        /// Optional parent page URL used to compute Referer and Sec-Fetch-* headers
        #[arg(long)]
        referer: Option<String>,
        /// Stage a single fetched asset into <state_dir>/artifacts/ and print its local path
        #[arg(long, conflicts_with_all = ["output", "metadata_only", "jsonl"])]
        stage: bool,
        /// CSS selector to wait for when CEF renders
        #[arg(long)]
        wait_for_selector: Option<String>,
        /// Enforce robots.txt for this fetch (skipped by default)
        #[arg(long)]
        respect_robots: bool,
        /// Disable bundled/local host rewrite rules (e.g., old.reddit.com)
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
    /// Manage stored search provider credentials
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Crawl a website starting from a URL
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
    /// Discover URLs on a website via sitemap and links
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
    /// Manage persistent background CEF renderer daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Manage local browser/network session state
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Show binary, Cronet, CEF, and search configuration status
    Status,
    /// Emit the canonical version-locked pleb agent skill
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Store a search provider credential (hidden TTY prompt, or read from --stdin)
    Set {
        /// Search provider credential to store
        #[arg(value_enum, ignore_case = true)]
        provider: CredentialProvider,
        /// Read credential from stdin (single line, <= 4096 bytes)
        #[arg(long)]
        stdin: bool,
        /// Credential storage backend
        #[arg(long, value_enum, default_value_t = CredentialStorage::Auto, ignore_case = true)]
        storage: CredentialStorage,
    },
    /// Remove a stored search provider credential
    Remove {
        /// Search provider credential to remove
        #[arg(value_enum, ignore_case = true)]
        provider: CredentialProvider,
        /// Credential storage backend to remove from
        #[arg(long, value_enum, default_value_t = CredentialStorage::Auto, ignore_case = true)]
        storage: CredentialStorage,
    },
    /// Show search provider credential and cooldown status
    Status,
}

#[derive(Subcommand)]
enum SkillAction {
    /// Print the bundled pleb SKILL.md to stdout
    Print,
}

const BUNDLED_SKILL_MD: &str = include_str!("../skills/pleb/SKILL.md");

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the background CEF daemon (auto-started on demand)
    Run,
    /// Show background CEF daemon status
    Status,
    /// Stop the running background CEF daemon
    Stop,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Clear network storage and cached per-site strategies
    Clear,
}

#[derive(Debug)]
struct CliError {
    message: String,
    kind: &'static str,
    exit_code: u8,
}

impl CliError {
    fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "invalid_input",
            exit_code: 1,
        }
    }

    fn from_fetch(err: FetchError) -> Self {
        Self {
            kind: err.kind(),
            exit_code: err.exit_code().clamp(1, 255) as u8,
            message: err.to_string(),
        }
    }

    fn runtime(kind: &'static str, err: impl std::fmt::Display) -> Self {
        Self {
            message: err.to_string(),
            kind,
            exit_code: 1,
        }
    }
}

#[derive(Serialize)]
struct FetchMetadataOutput<'a> {
    url: &'a str,
    final_url: &'a str,
    status_code: u16,
    content_type: Option<&'a str>,
    title: Option<&'a str>,
    strategy: &'a str,
    escalated: bool,
    truncated: bool,
    timing_ms: u64,
    content_bytes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    resources: Vec<pleb::extract::ResourceDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tools: Vec<pleb::extract::WebMcpToolDescriptor>,
}

impl<'a> From<&'a FetchOutput> for FetchMetadataOutput<'a> {
    fn from(res: &'a FetchOutput) -> Self {
        Self {
            url: &res.url,
            final_url: &res.final_url,
            status_code: res.status_code,
            content_type: res.content_type.as_deref(),
            title: res.title.as_deref(),
            strategy: &res.strategy,
            escalated: res.escalated,
            truncated: res.truncated,
            timing_ms: res.timing_ms,
            content_bytes: res.content_bytes,
            resources: res.resources.clone(),
            tools: res.tools.clone(),
        }
    }
}

#[derive(Serialize)]
struct RendererStatusJson {
    status: &'static str,
    path: Option<String>,
    found_protocol: Option<String>,
    expected_protocol: &'static str,
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
struct StatusJson {
    version: &'static str,
    cronet_enabled: bool,
    cef_renderer: RendererStatusJson,
    daemon: DaemonStatusJson,
    state_dir: String,
    search_providers: BTreeMap<String, SearchProviderStatus>,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.use_stderr() && std::env::args_os().any(|a| a == "--json" || a == "--jsonl") {
                let cli_err = CliError::invalid_input(err.to_string().trim_end());
                emit_cli_error(&cli_err, true);
                return ExitCode::from(cli_err.exit_code);
            }
            err.exit();
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
            let renderer_json = match &probe {
                RendererProbe::Ready(path) => RendererStatusJson {
                    status: "ready",
                    path: Some(path.display().to_string()),
                    found_protocol: Some(cef::EXPECTED_PROTOCOL.to_string()),
                    expected_protocol: cef::EXPECTED_PROTOCOL,
                },
                RendererProbe::ProtocolMismatch {
                    path,
                    found,
                    expected,
                } => RendererStatusJson {
                    status: "protocol_mismatch",
                    path: Some(path.display().to_string()),
                    found_protocol: found.clone(),
                    expected_protocol: expected,
                },
                RendererProbe::NotInstalled => RendererStatusJson {
                    status: "not_installed",
                    path: None,
                    found_protocol: None,
                    expected_protocol: cef::EXPECTED_PROTOCOL,
                },
            };
            let daemon_json = daemon_status_json(&config.state_dir).await;
            let search_providers = search::search_provider_statuses(&config.state_dir)
                .map_err(|e| CliError::runtime("status_error", e))?;

            if json {
                let payload = StatusJson {
                    version: env!("CARGO_PKG_VERSION"),
                    cronet_enabled: cfg!(feature = "cronet"),
                    cef_renderer: renderer_json,
                    daemon: daemon_json,
                    state_dir: config.state_dir.display().to_string(),
                    search_providers,
                };
                print_json(&payload)?;
            } else {
                println!("pleb {}", env!("CARGO_PKG_VERSION"));
                println!(
                    "Cronet: {}",
                    if cfg!(feature = "cronet") {
                        "enabled"
                    } else {
                        "disabled (reqwest fallback)"
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
                print_daemon_status(&daemon_json);
                println!("State dir: {}", config.state_dir.display());
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
        },
        Command::Fetch {
            urls,
            format,
            extract,
            render,
            source,
            resources,
            stage_resources,
            referer,
            stage,
            wait_for_selector,
            respect_robots,
            no_rewrite,
            no_images,
            no_links,
            max_bytes,
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
            let is_stdin = urls.len() == 1 && urls[0] == "-";
            if urls.len() > 1 && urls.iter().any(|u| u == "-") {
                return Err(CliError::invalid_input(
                    "stdin '-' cannot be mixed with positional URLs",
                ));
            }
            let batch_mode = is_stdin || urls.len() > 1;

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
            if format == OutputFormat::Raw && (json || jsonl || metadata_only || batch_mode) {
                return Err(CliError::invalid_input(
                    "--format raw cannot be combined with batch mode, --json, --jsonl, or --metadata-only",
                ));
            }
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
                    include_images: !no_images,
                    include_links: !no_links,
                    max_bytes,
                },
                render,
                source,
                resources,
                stage_resources,
                referer,
                wait_for_selector,
                respect_robots,
                apply_rewrites: !no_rewrite,
                ..Default::default()
            };

            if !batch_mode {
                let url = &target_urls[0];
                let res = fetch::fetch_document(&ctx, url, &opts)
                    .await
                    .map_err(CliError::from_fetch)?;

                if jsonl {
                    let item = BatchItemResult::from_result(url, Ok(res), metadata_only);
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
                    read_secret_from_stdin()?
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
                let statuses = search::search_provider_statuses(&config.state_dir)
                    .map_err(|e| CliError::runtime("auth_error", e))?;
                if json {
                    print_json(&statuses)?;
                } else {
                    print_auth_statuses(&statuses);
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
            let result = crawl::crawl(&ctx, &url, options)
                .await
                .map_err(|e| CliError::runtime("crawl_error", e))?;
            if json {
                print_json(&result)?;
            } else {
                let host = url_host(&url);
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
                let host = url_host(&url);
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

fn url_host(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| raw.to_string())
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

fn zeroize_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

fn validate_secret_input(raw: &mut [u8]) -> Result<SecretString, CliError> {
    if raw.len() > MAX_SECRET_INPUT_BYTES {
        zeroize_bytes(raw);
        return Err(CliError::invalid_input(
            "credential input exceeds 4096-byte limit",
        ));
    }
    let result = match std::str::from_utf8(raw) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Err(CliError::invalid_input("credential cannot be empty"))
            } else if trimmed
                .bytes()
                .any(|b| !b.is_ascii() || b.is_ascii_control())
            {
                Err(CliError::invalid_input(
                    "credential must be a single line of printable ASCII characters",
                ))
            } else {
                Ok(SecretString::new(trimmed.to_string()))
            }
        }
        Err(_) => Err(CliError::invalid_input(
            "credential must be printable ASCII",
        )),
    };
    zeroize_bytes(raw);
    result
}

fn read_secret_from_stdin() -> Result<SecretString, CliError> {
    let mut buf = Vec::new();
    let mut limited = std::io::stdin().take((MAX_SECRET_INPUT_BYTES + 1) as u64);
    if let Err(err) = limited.read_to_end(&mut buf) {
        zeroize_bytes(&mut buf);
        return Err(CliError::runtime("io_error", err));
    }
    validate_secret_input(&mut buf)
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
                zeroize_bytes(&mut buf);
                drop(guard);
                return Err(CliError {
                    message: "interrupted".to_string(),
                    kind: "interrupted",
                    exit_code: 130,
                });
            }
            zeroize_bytes(&mut buf);
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
        zeroize_bytes(&mut buf);
        return Err(CliError {
            message: "interrupted".to_string(),
            kind: "interrupted",
            exit_code: 130,
        });
    }

    validate_secret_input(&mut buf)
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

fn print_auth_statuses(statuses: &BTreeMap<String, SearchProviderStatus>) {
    for (provider, status) in statuses {
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

fn emit_cli_error(err: &CliError, json: bool) {
    if json {
        let payload = serde_json::json!({
            "error": err.message,
            "kind": err.kind,
            "exit_code": err.exit_code,
        });
        eprintln!("{}", payload);
    } else {
        eprintln!("error: {}", err.message);
    }
}
