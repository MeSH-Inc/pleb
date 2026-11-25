use anyhow::Result;
use clap::{Parser, Subcommand};
use mesh_wick::transport::{cef::CefRenderer, TransportClient};
use mesh_wick::{crawl, extract, fetch, search, session};
use std::str::FromStr;
use std::time::Duration;

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
        no_robots: bool,
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
    let proxy = cli.proxy.as_deref();
    let timeout = Duration::from_secs(30);
    let cef_timeout = Duration::from_secs(60);

    match cli.command {
        Command::Fetch {
            url,
            format,
            render,
            wait_for_selector,
            no_robots,
            json,
        } => {
            let client = TransportClient::new(&session::storage_path()?, proxy, timeout)?;
            let cef = CefRenderer::new(proxy.map(str::to_string), cef_timeout, false);
            let parsed_format = extract::OutputFormat::from_str(&format)?;
            let render_mode = fetch::RenderMode::from_str(&render);
            let selector = wait_for_selector.as_deref();

            if parsed_format == extract::OutputFormat::Raw {
                use std::io::{IsTerminal, Write};
                if std::io::stdout().is_terminal() {
                    anyhow::bail!("refusing to write raw bytes to a terminal.");
                }
                let result = fetch::fetch_raw(&client, &url, !no_robots).await?;
                std::io::stdout().write_all(&result.bytes)?;
                return Ok(());
            }

            let result = fetch::fetch(
                &client,
                &cef,
                &url,
                parsed_format,
                !no_robots,
                render_mode,
                selector,
            )
            .await?;

            if json {
                let out = serde_json::json!({
                    "url": result.url,
                    "status_code": result.status_code,
                    "timing_ms": result.timing_ms,
                    "content_bytes": result.content.len(),
                    "title": result.title,
                });
                println!("{}", out);
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
            let client = TransportClient::new(&session::storage_path()?, proxy, timeout)?;
            let results = search::search(&client, &query, num).await?;
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
            let client = TransportClient::new(&session::storage_path()?, proxy, timeout)?;
            let cef = CefRenderer::new(proxy.map(str::to_string), cef_timeout, false);
            let options = crawl::CrawlOptions {
                max_depth: depth.min(5),
                max_pages: max_pages.min(50),
                format: extract::OutputFormat::Markdown,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::crawl(&client, &cef, &url, options).await?;
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
            let client = TransportClient::new(&session::storage_path()?, proxy, timeout)?;
            let cef = CefRenderer::new(proxy.map(str::to_string), cef_timeout, false);
            let options = crawl::MapOptions {
                limit: limit.min(5000),
                use_sitemap: !no_sitemap,
                respect_robots: !no_robots,
                path_filter,
            };
            let result = crawl::map(&client, &cef, &url, options).await?;
            let host = url::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|s| s.to_string()))
                .unwrap_or_else(|| url.clone());
            print!("{}", crawl::format_map_output(&result, &host));
            Ok(())
        }
        Command::Session { action } => match action {
            SessionAction::Clear => session::clear(),
        },
        Command::Status => {
            let cef = CefRenderer::new(None, cef_timeout, false);
            println!("mesh-wick {}", env!("CARGO_PKG_VERSION"));
            println!(
                "CEF renderer: {}",
                if cef.is_available() { "installed" } else { "not installed" }
            );
            Ok(())
        }
    }
}
