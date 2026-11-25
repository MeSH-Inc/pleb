use anyhow::Result;
use clap::{Parser, Subcommand};
use mesh_wick::context::{default_state_dir, RuntimeConfig, RuntimeContext};
use mesh_wick::extract::{ExtractMode, ExtractOptions, OutputFormat};
use mesh_wick::fetch::{self, FetchOptions};
use mesh_wick::policy::RenderMode;
use mesh_wick::{crawl, search};
use std::str::FromStr;

#[derive(Parser)]
#[command(name = "mesh-wick", version, about = "Local browser-grade web fetch and search CLI for AI agents")]
struct Cli {
    /// SOCKS5 proxy (e.g., socks5://localhost:1080)
    #[arg(long, global = true, env = "MESH_WICK_PROXY")]
    proxy: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch a URL and print content to stdout
    Fetch {
        url: String,
        #[arg(long, default_value = "markdown")]
        format: String,
        #[arg(long, default_value = "auto")]
        render: String,
        #[arg(long)]
        wait_for_selector: Option<String>,
        #[arg(long)]
        respect_robots: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search the web and print results
    Search {
        query: String,
        #[arg(short, long, default_value = "5")]
        num: usize,
    },
    /// Crawl a website and return content from multiple pages
    Crawl {
        url: String,
        #[arg(short, long, default_value = "2")]
        depth: u32,
        #[arg(short = 'n', long, default_value = "10")]
        max_pages: u32,
        #[arg(long)]
        path_filter: Option<String>,
        #[arg(long)]
        no_robots: bool,
    },
    /// Discover all URLs on a website
    Map {
        url: String,
        #[arg(short = 'n', long, default_value = "100")]
        limit: u32,
        #[arg(long)]
        no_sitemap: bool,
        #[arg(long)]
        path_filter: Option<String>,
        #[arg(long)]
        no_robots: bool,
    },
    /// Manage local browser/network session state
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Show component status
    Status,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Clear cookies and cached site state
    Clear,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = RuntimeConfig {
        state_dir: default_state_dir(),
        proxy: cli.proxy,
        ..Default::default()
    };
    let ctx = RuntimeContext::new(config)?;

    match cli.command {
        Command::Fetch {
            url,
            format,
            render,
            wait_for_selector,
            respect_robots,
            json,
        } => {
            let parsed_format = OutputFormat::from_str(&format)?;
            let render_mode = RenderMode::from_str(&render)?;

            if parsed_format == OutputFormat::Raw {
                use std::io::{IsTerminal, Write};
                if std::io::stdout().is_terminal() {
                    anyhow::bail!("refusing to write raw bytes to a terminal.");
                }
                let result = fetch::fetch_raw(&ctx, &url, respect_robots).await?;
                std::io::stdout().write_all(&result.bytes)?;
                return Ok(());
            }

            let opts = FetchOptions {
                extract: ExtractOptions {
                    format: parsed_format,
                    ..Default::default()
                },
                render: render_mode,
                wait_for_selector,
                respect_robots,
                apply_rewrites: true,
            };
            let result = fetch::fetch_document(&ctx, &url, &opts).await?;

            if json {
                println!("{}", serde_json::to_string(&result)?);
                return Ok(());
            }

            if let Some(title) = &result.title {
                eprintln!("Title: {}", title);
            }
            eprintln!("Status: {} | Time: {}ms\n", result.status_code, result.timing_ms);
            print!("{}", result.content);
            Ok(())
        }
        Command::Search { query, num } => {
            let results = search::search(&ctx.client, &query, num).await?;
            println!("{}", search::format_results(&results));
            Ok(())
        }
        Command::Crawl {
            url,
            depth,
            max_pages,
            path_filter,
            no_robots,
        } => {
            let options = crawl::CrawlOptions {
                max_depth: depth.min(5),
                max_pages: max_pages.min(50),
                format: OutputFormat::Markdown,
                extract_mode: ExtractMode::Clean,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::crawl(&ctx, &url, options).await?;
            let host = url::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|s| s.to_string()))
                .unwrap_or_else(|| url.clone());
            print!("{}", crawl::format_crawl_output(&result, &host));
            Ok(())
        }
        Command::Map {
            url,
            limit,
            no_sitemap,
            path_filter,
            no_robots,
        } => {
            let options = crawl::MapOptions {
                limit: limit.min(5000),
                use_sitemap: !no_sitemap,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::map(&ctx, &url, options).await?;
            let host = url::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|s| s.to_string()))
                .unwrap_or_else(|| url.clone());
            print!("{}", crawl::format_map_output(&result, &host));
            Ok(())
        }
        Command::Session { action } => match action {
            SessionAction::Clear => ctx.clear_session(),
        },
        Command::Status => {
            println!("mesh-wick {}", env!("CARGO_PKG_VERSION"));
            println!(
                "CEF renderer: {}",
                if ctx.cef.is_available() { "installed" } else { "not installed" }
            );
            Ok(())
        }
    }
}
