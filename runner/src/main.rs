use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

mod harness;
mod mtr_parser;
mod result_diff;
mod runner;
mod skip_list;

#[derive(Parser, Debug)]
#[command(version, about = "MTR-dialect MySQL-compat test runner for Zeta")]
struct Args {
    /// Path to a built `zeta` server binary.
    #[arg(long)]
    zeta_bin: PathBuf,

    /// Comma-separated list of suites under tests/, e.g. `main,binlog`.
    /// Use `all` for every suite.
    #[arg(long, default_value = "all")]
    suite: String,

    /// Optional substring filter applied to the .test path.
    #[arg(long)]
    filter: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let mut zeta = harness::ZetaServer::start(&args.zeta_bin).await?;
    let url_base = format!("mysql://root@127.0.0.1:{}", zeta.port());

    // Load the skip list once for all suites. A missing file is fine (empty
    // skip list); a malformed one is a hard error rather than silently
    // skipping nothing and reporting every previously-skipped test as new
    // failures.
    let skip_list = skip_list::SkipList::load(std::path::Path::new("."))?;

    let mut had_failure = false;
    for suite in args
        .suite
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Err(e) =
            runner::run_suite(suite, &url_base, args.filter.as_deref(), &skip_list).await
        {
            eprintln!("suite {suite} failed: {e}");
            had_failure = true;
        }
    }

    let _ = zeta.shutdown().await;
    if had_failure {
        std::process::exit(1);
    }
    Ok(())
}
