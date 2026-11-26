use clap::{Parser, Subcommand};
use mesh_wick::context::{clear_state_dir, default_state_dir, RuntimeConfig, RuntimeContext};
use mesh_wick::crawl;
#[cfg(unix)]
use mesh_wick::daemon;
use mesh_wick::extract::{format_page_section, ExtractMode, OutputFormat};
use mesh_wick::fetch::{
    self, BatchFetchOptions, BatchItemResult, FetchError, FetchOptions, FetchOutput,
};
use mesh_wick::policy::RenderMode;
use mesh_wick::search::{self, SearchProvider};
use mesh_wick::transport::cef::{self, RendererProbe};
use serde::Serialize;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "wick",
    version,
    about = "Local browser-grade web fetch and search CLI for AI agents"
)]
struct Cli {
    /// SOCKS5/HTTP proxy URL (e.g., socks5://localhost:1080)
    #[arg(long, global = true, env = "MESH_WICK_PROXY")]
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
    #[arg(long, global = true, env = "MESH_WICK_HOME")]
    state_dir: Option<PathBuf>,

    /// Disable persistent background daemon sharing for this command
    #[arg(long, global = true, env = "MESH_WICK_NO_DAEMON")]
    no_daemon: bool,

    /// Idle timeout in seconds for the auto-spawned background daemon
    #[arg(
        long,
        global = true,
        env = "MESH_WICK_DAEMON_IDLE_SECONDS",
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
}

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

#[derive(Serialize)]
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
    brave_api_key_configured: bool,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if err.use_stderr()
                && std::env::args_os().any(|a| a == "--json" || a == "--jsonl")
            {
                let cli_err = CliError::invalid_input(err.to_string().trim_end());
                emit_cli_error(&cli_err, true);
                return ExitCode::from(cli_err.exit_code);
            }
            err.exit();
        }
    };

    init_tracing(cli.verbose);
    let json_error_mode =
        cli.json || matches!(&cli.command, Command::Fetch { jsonl: true, .. });

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
    };

    match cli.command {
        Command::Daemon { action } => match action {
            DaemonAction::Run => {
                #[cfg(unix)]
                {
                    daemon::run_daemon(config.state_dir, config.daemon_idle_timeout, config.verbose)
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
                    println!(
                        "{}",
                        serde_json::to_string(&status)
                            .map_err(|e| CliError::runtime("serialization_error", e))?
                    );
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

            if json {
                let payload = StatusJson {
                    version: env!("CARGO_PKG_VERSION"),
                    cronet_enabled: cfg!(feature = "cronet"),
                    cef_renderer: renderer_json,
                    daemon: daemon_json,
                    state_dir: config.state_dir.display().to_string(),
                    brave_api_key_configured: search::brave_api_key_configured(),
                };
                println!(
                    "{}",
                    serde_json::to_string(&payload)
                        .map_err(|e| CliError::runtime("serialization_error", e))?
                );
            } else {
                println!("wick {} (mesh-wick)", env!("CARGO_PKG_VERSION"));
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
                println!(
                    "Brave Search API key: {}",
                    if search::brave_api_key_configured() {
                        "configured"
                    } else {
                        "unset"
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Fetch {
            urls,
            format,
            extract,
            render,
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
                use std::io::Read;
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

            let ctx = RuntimeContext::new(config)
                .map_err(|e| CliError::runtime("context_error", e))?;

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

            let opts = FetchOptions {
                extract: mesh_wick::extract::ExtractOptions {
                    format,
                    mode: extract,
                    include_images: !no_images,
                    include_links: !no_links,
                    max_bytes,
                },
                render,
                wait_for_selector,
                respect_robots,
                apply_rewrites: !no_rewrite,
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
            let mut results = fetch::fetch_batch(Arc::new(ctx), target_urls, batch_opts, |_, item| {
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
            let ordered: Vec<BatchItemResult> =
                results.into_iter().map(|(_, item)| item).collect();
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
        } => {
            let ctx = RuntimeContext::new(config)
                .map_err(|e| CliError::runtime("context_error", e))?;
            let response = search::search(&ctx, &query, limit, provider)
                .await
                .map_err(|e| CliError::runtime("search_error", e))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&response)
                        .map_err(|e| CliError::runtime("serialization_error", e))?
                );
            } else {
                println!("{}", search::format_results(&response));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Crawl {
            url,
            depth,
            max_pages,
            extract,
            path_filter,
            no_robots,
        } => {
            let ctx = RuntimeContext::new(config)
                .map_err(|e| CliError::runtime("context_error", e))?;
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
                println!(
                    "{}",
                    serde_json::to_string(&result)
                        .map_err(|e| CliError::runtime("serialization_error", e))?
                );
            } else {
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| url.clone());
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
            let ctx = RuntimeContext::new(config)
                .map_err(|e| CliError::runtime("context_error", e))?;
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
                println!(
                    "{}",
                    serde_json::to_string(&result)
                        .map_err(|e| CliError::runtime("serialization_error", e))?
                );
            } else {
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| url.clone());
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
                running: false,
                pid: None,
                protocol_version: None,
                build_id: None,
                active_requests: 0,
                cronet_slots: 0,
                cef_renderers_alive: 0,
            },
        }
    }
    #[cfg(not(unix))]
    {
        let _ = state_dir;
        DaemonStatusJson {
            supported: false,
            running: false,
            pid: None,
            protocol_version: None,
            build_id: None,
            active_requests: 0,
            cronet_slots: 0,
            cef_renderers_alive: 0,
        }
    }
}

fn init_tracing(verbose: bool) {
    let default_level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(default_level.into());
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
