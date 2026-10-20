use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

mod harness;
mod hazard;
mod mtr_parser;
mod oracle;
mod outcome;
mod result_diff;
mod runner;
mod side;
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
    #[arg(long = "exclude")]
    exclude: Vec<String>,

    /// Differential mode: allow an `--oracle-url` whose host is not this machine. The run DROPS every
    /// non-system database on the oracle, so a remote server is refused unless this is given.
    #[arg(long)]
    allow_remote_oracle: bool,

    /// Differential mode: write one line per diverging statement (with the
    /// statement text and both outputs) to this file. Local reading only: it
    /// quotes test content, so do not commit it.
    #[arg(long)]
    report: Option<PathBuf>,
}

fn suites(args: &Args) -> Vec<String> {
    args.suite
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
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
    let suite_list = suites(&args);
    let common = || oracle::Common {
        suites: &suite_list,
        filter: args.filter.as_deref(),
        exclude: &args.exclude,
        report: args.report.as_deref(),
    };

    let zeta_bin = args.zeta_bin.clone();
    let mut zeta = harness::ZetaServer::start(&zeta_bin).await?;
    let url_base = format!("mysql://root@127.0.0.1:{}", zeta.port());

    if let Some(oracle_url) = &args.oracle_url {
        let res = oracle::run_diff(
            &mut zeta,
            &zeta_bin,
            common(),
            oracle_url,
            args.allow_remote_oracle,
        )
        .await;
        let _ = zeta.shutdown().await;
        return res;
    }

    // Load the skip list once for all suites. A missing file is fine (empty
    // skip list); a malformed one is a hard error rather than silently
    // skipping nothing and reporting every previously-skipped test as new
    // failures.
    let skip_list = skip_list::SkipList::load(std::path::Path::new("."))?;

    let mut had_failure = false;
    for suite in &suite_list {
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
