use anyhow::Result;
use clap::{Parser, Subcommand};
use mesh_wick::context::{clear_state_dir, default_state_dir, RuntimeConfig, RuntimeContext};
use mesh_wick::extract::{ExtractMode, ExtractOptions, OutputFormat};
use mesh_wick::fetch::{self, FetchError, FetchOptions};
use mesh_wick::policy::RenderMode;
use mesh_wick::search::{self, SearchProvider};
use mesh_wick::transport::cef;
#[cfg(unix)]
use mesh_wick::transport::cef_daemon;
use mesh_wick::crawl;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "wick",
    version,
    about = "Local browser-grade web fetch and search CLI for AI agents"
)]
struct Cli {
    /// SOCKS5 proxy (e.g., socks5://localhost:1080). Also reads MESH_WICK_PROXY or WICK_PROXY.
    #[arg(long, global = true, env = "MESH_WICK_PROXY")]
    proxy: Option<String>,

    /// Network request timeout in seconds
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    /// CEF render timeout in seconds
    #[arg(long, global = true, default_value = "60")]
    cef_timeout: u64,

    /// Directory for local cache and session state
    #[arg(long, global = true, env = "MESH_WICK_HOME")]
    state_dir: Option<PathBuf>,

    /// Disable persistent background CEF daemon sharing for this command
    #[arg(long, global = true, env = "MESH_WICK_NO_CEF_DAEMON")]
    no_cef_daemon: bool,

    /// Idle timeout in seconds for the auto-spawned CEF daemon
    #[arg(long, global = true, env = "MESH_WICK_CEF_IDLE_SECS", default_value = "300")]
    cef_idle_timeout: u64,

    /// Enable verbose diagnostic logging to stderr
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Suppress non-error status metadata lines on stderr
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch a URL and print extracted content to stdout
    Fetch {
        /// Target URL (http or https)
        url: String,
        /// Output format: markdown (default), html, text, or raw
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Content extraction scope: clean (default), article, or full
        #[arg(long, default_value = "clean")]
        extract: String,
        /// Transport mode: auto (default), cef, or cronet
        #[arg(long, default_value = "auto")]
        render: String,
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
        /// Emit structured JSON output
        #[arg(long)]
        json: bool,
        /// With --json, omit the content field and emit metadata only
        #[arg(long)]
        meta_only: bool,
    },
    /// Search the web and print results
    Search {
        /// Search query
        query: String,
        /// Maximum number of results (1-20)
        #[arg(short, long, default_value = "8")]
        num: usize,
        /// Provider: auto (Brave when BRAVE_API_KEY is set, else DuckDuckGo), brave, or ddg
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Emit structured JSON output
        #[arg(long)]
        json: bool,
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
        /// Content extraction scope: clean (default), article, or full
        #[arg(long, default_value = "clean")]
        extract: String,
        /// Only follow URLs whose path starts with this prefix
        #[arg(long)]
        path_filter: Option<String>,
        /// Ignore robots.txt restrictions during crawl
        #[arg(long)]
        no_robots: bool,
        /// Emit structured JSON output
        #[arg(long)]
        json: bool,
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
        /// Emit structured JSON output
        #[arg(long)]
        json: bool,
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
    Run {
        /// Exit after this many seconds with no render requests
        #[arg(long, default_value = "300")]
        idle_secs: u64,
    },
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let proxy = cli
        .proxy
        .or_else(|| std::env::var("WICK_PROXY").ok().filter(|s| !s.trim().is_empty()));
    let state_dir = cli.state_dir.unwrap_or_else(default_state_dir);

    let config = RuntimeConfig {
        state_dir,
        proxy,
        timeout: Duration::from_secs(cli.timeout.max(1)),
        cef_timeout: Duration::from_secs(cli.cef_timeout.max(1)),
        cef_idle_timeout: Duration::from_secs(cli.cef_idle_timeout.max(1)),
        cef_daemon: !cli.no_cef_daemon,
        verbose: cli.verbose,
    };

    match cli.command {
        Command::Daemon { action } => match action {
            DaemonAction::Run { idle_secs } => {
                #[cfg(unix)]
                {
                    let idle = Duration::from_secs(idle_secs.max(1));
                    cef_daemon::run_daemon(config.state_dir, idle, cli.verbose).await
                }
                #[cfg(not(unix))]
                {
                    let _ = idle_secs;
                    anyhow::bail!("CEF daemon mode is only supported on Unix systems")
                }
            }
            DaemonAction::Status => {
                #[cfg(unix)]
                {
                    match cef_daemon::daemon_ping(&config.state_dir).await {
                        Some(st) => println!(
                            "CEF daemon: running (pid {}, renderer {})",
                            st.pid,
                            if st.renderer_alive { "warm" } else { "idle" }
                        ),
                        None => println!("CEF daemon: stopped"),
                    }
                    Ok(())
                }
                #[cfg(not(unix))]
                {
                    println!("CEF daemon: unsupported on this OS");
                    Ok(())
                }
            }
            DaemonAction::Stop => {
                #[cfg(unix)]
                {
                    let stopped = cef_daemon::daemon_stop(&config.state_dir).await?;
                    if !cli.quiet {
                        if stopped {
                            println!("Stopped CEF daemon");
                        } else {
                            println!("CEF daemon not running");
                        }
                    }
                    Ok(())
                }
                #[cfg(not(unix))]
                {
                    anyhow::bail!("CEF daemon mode is only supported on Unix systems")
                }
            }
        },
        Command::Session { action } => match action {
            SessionAction::Clear => {
                #[cfg(unix)]
                let _ = cef_daemon::daemon_stop(&config.state_dir).await;
                clear_state_dir(&config.state_dir)?;
                if !cli.quiet {
                    eprintln!("Cleared session state at {}", config.state_dir.display());
                }
                Ok(())
            }
        },
        Command::Status => {
            let renderer_path = cef::find_renderer_in(&config.state_dir);
            println!("wick {} (mesh-wick)", env!("CARGO_PKG_VERSION"));
            println!(
                "Cronet: {}",
                if cfg!(feature = "cronet") {
                    "enabled"
                } else {
                    "disabled (reqwest fallback)"
                }
            );
            match renderer_path {
                Some(path) => println!("CEF renderer: installed ({})", path.display()),
                None => println!("CEF renderer: not installed"),
            }
            #[cfg(unix)]
            match cef_daemon::daemon_ping(&config.state_dir).await {
                Some(st) => println!(
                    "CEF daemon: running (pid {}, renderer {})",
                    st.pid,
                    if st.renderer_alive { "warm" } else { "idle" }
                ),
                None => println!("CEF daemon: stopped"),
            }
            println!("State dir: {}", config.state_dir.display());
            println!(
                "Brave Search API key: {}",
                if search::brave_api_key_configured() {
                    "configured"
                } else {
                    "unset"
                }
            );
            Ok(())
        }
        Command::Fetch {
            url,
            format,
            extract,
            render,
            wait_for_selector,
            respect_robots,
            no_rewrite,
            no_images,
            no_links,
            max_bytes,
            json,
            meta_only,
        } => {
            let ctx = RuntimeContext::new(config)?;
            let parsed_format = OutputFormat::from_str(&format)?;
            let extract_mode = ExtractMode::from_str(&extract)?;
            let render_mode = RenderMode::from_str(&render)?;

            if parsed_format == OutputFormat::Raw {
                use std::io::{IsTerminal, Write};
                if std::io::stdout().is_terminal() {
                    anyhow::bail!(
                        "refusing to write raw bytes to a terminal; redirect stdout to a file"
                    );
                }
                match fetch::fetch_raw(&ctx, &url, respect_robots).await {
                    Ok(res) => {
                        if !cli.quiet {
                            eprintln!(
                                "Status: {} | Time: {}ms | Bytes: {}",
                                res.status_code,
                                res.timing_ms,
                                res.bytes.len()
                            );
                        }
                        std::io::stdout().write_all(&res.bytes)?;
                        return Ok(());
                    }
                    Err(err) => emit_fetch_error_and_exit(&err, json),
                }
            }

            let opts = FetchOptions {
                extract: ExtractOptions {
                    format: parsed_format,
                    mode: extract_mode,
                    include_images: !no_images,
                    include_links: !no_links,
                    max_bytes,
                },
                render: render_mode,
                wait_for_selector,
                respect_robots,
                apply_rewrites: !no_rewrite,
            };

            match fetch::fetch_document(&ctx, &url, &opts).await {
                Ok(res) => {
                    if json {
                        let out = if meta_only {
                            serde_json::json!({
                                "url": res.url,
                                "final_url": res.final_url,
                                "status_code": res.status_code,
                                "content_type": res.content_type,
                                "title": res.title,
                                "strategy": res.strategy,
                                "escalated": res.escalated,
                                "truncated": res.truncated,
                                "timing_ms": res.timing_ms,
                                "content_bytes": res.content.len(),
                            })
                        } else {
                            serde_json::json!({
                                "url": res.url,
                                "final_url": res.final_url,
                                "status_code": res.status_code,
                                "content_type": res.content_type,
                                "title": res.title,
                                "strategy": res.strategy,
                                "escalated": res.escalated,
                                "truncated": res.truncated,
                                "timing_ms": res.timing_ms,
                                "content_bytes": res.content.len(),
                                "content": res.content,
                            })
                        };
                        println!("{}", out);
                    } else {
                        if !cli.quiet {
                            if let Some(title) = &res.title {
                                eprintln!("Title: {}", title);
                            }
                            eprintln!(
                                "Status: {} | Strategy: {} | Time: {}ms\n",
                                res.status_code, res.strategy, res.timing_ms
                            );
                        }
                        print!("{}", res.content);
                    }
                    Ok(())
                }
                Err(err) => emit_fetch_error_and_exit(&err, json),
            }
        }
        Command::Search {
            query,
            num,
            provider,
            json,
        } => {
            let ctx = RuntimeContext::new(config)?;
            let parsed_provider = SearchProvider::from_str(&provider)?;
            let response = search::search(&ctx, &query, num, parsed_provider).await?;
            if json {
                println!("{}", serde_json::to_string(&response)?);
            } else {
                println!("{}", search::format_results(&response));
            }
            Ok(())
        }
        Command::Crawl {
            url,
            depth,
            max_pages,
            extract,
            path_filter,
            no_robots,
            json,
        } => {
            let ctx = RuntimeContext::new(config)?;
            let extract_mode = ExtractMode::from_str(&extract)?;
            let options = crawl::CrawlOptions {
                max_depth: depth.min(5),
                max_pages: max_pages.min(50),
                format: OutputFormat::Markdown,
                extract_mode,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::crawl(&ctx, &url, options).await?;
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| url.clone());
                if !cli.quiet {
                    eprintln!(
                        "Crawled {} pages in {:.1}s\n",
                        result.pages.len(),
                        result.timing_ms as f64 / 1000.0
                    );
                }
                print!("{}", crawl::format_crawl_output(&result, &host));
            }
            Ok(())
        }
        Command::Map {
            url,
            limit,
            no_sitemap,
            path_filter,
            no_robots,
            json,
        } => {
            let ctx = RuntimeContext::new(config)?;
            let options = crawl::MapOptions {
                limit: limit.min(5000),
                use_sitemap: !no_sitemap,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::map(&ctx, &url, options).await?;
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                let host = url::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| url.clone());
                if !cli.quiet {
                    eprintln!(
                        "Found {} URLs in {:.1}s\n",
                        result.urls.len(),
                        result.timing_ms as f64 / 1000.0
                    );
                }
                print!("{}", crawl::format_map_output(&result, &host));
            }
            Ok(())
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

fn emit_fetch_error_and_exit(err: &FetchError, json: bool) -> ! {
    if json {
        let payload = serde_json::json!({
            "error": err.to_string(),
            "exit_code": err.exit_code(),
        });
        eprintln!("{}", payload);
    } else {
        eprintln!("error: {}", err);
    }
    std::process::exit(err.exit_code());
}
