use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

mod harness;
mod mtr_parser;
mod oracle;
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

    /// Differential mode: base URL of a real MySQL server (for example
    /// `mysql://root@127.0.0.1:33306`) to compare zeta against, statement by
    /// statement. The sibling `.result` files and the skip list are not used.
    /// The server's databases other than the system ones are DROPPED.
    #[arg(long)]
    oracle_url: Option<String>,

    /// Differential mode: skip test paths containing this substring (repeatable).
    /// Used for files that change the oracle's accounts, which need a restored
    /// server of their own.
    #[arg(long = "exclude")]
    exclude: Vec<String>,

    /// Differential mode: write one line per diverging statement (with the
    /// statement text and both outputs) to this file. Local reading only: it
    /// quotes test content, so do not commit it.
    #[arg(long)]
    report: Option<PathBuf>,
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
    if let Some(oracle_url) = &args.oracle_url {
        let suites: Vec<String> = args
            .suite
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let res = oracle::run_diff(
            &suites,
            args.filter.as_deref(),
            &args.exclude,
            &url_base,
            oracle_url,
            args.report.as_deref(),
        )
        .await;
        let _ = zeta.shutdown().await;
        return res;
    }

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
