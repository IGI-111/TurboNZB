//! Headless CLI harness for testing Newznab search.
//!
//! Usage:
//!   turbonbz-search --indexer "Name1=https://url1/api:apikey1" \
//!              --indexer "Name2=https://url2/api:apikey2" \
//!              --query "some search" [--tv --season 1 --episode 5] [--movie --imdb tt0058935]

use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;
use turbonbz_index::aggregate::SearchAggregator;
use turbonbz_index::newznab::NewznabClient;
use turbonbz_index::types::*;

#[derive(Debug, Clone, ValueEnum)]
enum SearchTypeArg {
    Search,
    Tv,
    Movie,
    Music,
    Book,
}

#[derive(Parser)]
#[command(name = "turbonbz-search", about = "Newznab search CLI harness")]
struct Cli {
    /// Indexer spec: "Name=https://api.url/api:apikey" (repeatable)
    #[arg(long, required = true)]
    indexer: Vec<String>,

    /// Search query text
    #[arg(long)]
    query: Option<String>,

    /// Search type
    #[arg(long, value_enum, default_value = "search")]
    ty: SearchTypeArg,

    /// TV season
    #[arg(long)]
    season: Option<u32>,

    /// TV episode
    #[arg(long)]
    episode: Option<u32>,

    /// IMDB id for movie search
    #[arg(long)]
    imdb: Option<String>,

    /// Max age in days
    #[arg(long)]
    max_age: Option<u32>,

    /// Per-indexer timeout in seconds
    #[arg(long, default_value = "15")]
    timeout: u64,

    /// Show caps for each indexer instead of searching
    #[arg(long)]
    caps: bool,
}

fn parse_indexer(spec: &str) -> anyhow::Result<IndexerConfig> {
    // Format: Name=https://url:apikey
    let eq_idx = spec
        .find('=')
        .ok_or_else(|| anyhow::anyhow!("indexer spec must be Name=url:apikey"))?;
    let name = spec[..eq_idx].to_string();
    let rest = &spec[eq_idx + 1..];
    let colon_idx = rest
        .rfind(':')
        .ok_or_else(|| anyhow::anyhow!("indexer spec must be Name=url:apikey"))?;
    let url = rest[..colon_idx].to_string();
    let api_key = rest[colon_idx + 1..].to_string();

    Ok(IndexerConfig {
        name,
        url,
        api_key,
        max_concurrent: 1,
        timeout_s: 15,
        priority: 0,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let configs: Vec<IndexerConfig> = cli
        .indexer
        .iter()
        .map(|s| parse_indexer(s))
        .collect::<Result<_, _>>()?;

    let clients: Vec<NewznabClient> = configs
        .into_iter()
        .map(|c| {
            NewznabClient::new(IndexerConfig {
                timeout_s: cli.timeout,
                ..c
            })
        })
        .collect();

    // Build aggregator
    let mut aggregator = SearchAggregator::new(cli.timeout + 5);
    for client in &clients {
        aggregator.add_provider(Box::new(client.clone()));
    }

    if cli.caps {
        // Show caps for each indexer
        for client in &clients {
            match client.caps().await {
                Ok(caps) => {
                    println!("=== {} ===", client.name());
                    println!("  server version: {}", caps.server_version);
                    println!("  protocol: {}", caps.protocol_version);
                    println!("  retention: {} days", caps.retention_days.unwrap_or(0));
                    println!("  max results: {}", caps.max_results);
                    if let Some(ref s) = caps.search {
                        println!(
                            "  search: available={}, params={:?}",
                            s.available, s.supported_params
                        );
                    }
                    if let Some(ref s) = caps.tv_search {
                        println!(
                            "  tv-search: available={}, params={:?}",
                            s.available, s.supported_params
                        );
                    }
                    if let Some(ref s) = caps.movie_search {
                        println!(
                            "  movie-search: available={}, params={:?}",
                            s.available, s.supported_params
                        );
                    }
                    println!("  categories: {}", caps.categories.len());
                    for cat in &caps.categories {
                        println!("    {} ({})", cat.name, cat.id);
                        for sc in &cat.subcats {
                            println!("      {} ({})", sc.name, sc.id);
                        }
                    }
                    println!();
                }
                Err(e) => {
                    eprintln!("caps failed for {}: {e}", client.name());
                }
            }
        }
        return Ok(());
    }

    // Build query
    let ty = match cli.ty {
        SearchTypeArg::Search => SearchType::Search,
        SearchTypeArg::Tv => SearchType::TvSearch,
        SearchTypeArg::Movie => SearchType::Movie,
        SearchTypeArg::Music => SearchType::Music,
        SearchTypeArg::Book => SearchType::Book,
    };

    let query = SearchQuery {
        ty,
        q: cli.query,
        season: cli.season,
        episode: cli.episode,
        imdb_id: cli.imdb,
        max_age_days: cli.max_age,
        ..Default::default()
    };

    println!("Searching {} indexer(s)...", clients.len());
    let results = aggregator.search(&query).await;

    println!("\n=== {} result(s) ===\n", results.len());
    for (i, r) in results.iter().enumerate() {
        let size_mb = r.result.size as f64 / 1_048_576.0;
        let age_days = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            (now - r.result.post_date) / 86400
        };
        println!(
            "{}. {} [{:.1} MB, {} days old]",
            i + 1,
            r.result.title,
            size_mb,
            age_days
        );
        println!("   indexer: {}", r.sources.join(", "));
        println!(
            "   category: {} ({})",
            r.result.category_name, r.result.category
        );
        println!("   url: {}", r.result.nzb_url);
        if r.result.password != PasswordStatus::None && r.result.password != PasswordStatus::Unknown
        {
            println!("   password: {:?}", r.result.password);
        }
        if let Some(tv) = &r.result.tv {
            if let Some(s) = tv.season {
                println!("   S{:02}E{:02}", s, tv.episode.unwrap_or(0));
            }
        }
        println!();
    }

    Ok(())
}
