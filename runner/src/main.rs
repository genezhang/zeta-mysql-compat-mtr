use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

mod harness;
mod mtr_parser;
mod result_diff;

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

    /// Optional: limit to .test files matching this glob pattern.
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
    let _zeta = harness::ZetaServer::start(&args.zeta_bin).await?;

    for suite in args.suite.split(',') {
        run_suite(suite, args.filter.as_deref()).await?;
    }

    Ok(())
}

async fn run_suite(_suite: &str, _filter: Option<&str>) -> Result<()> {
    // TODO: glob tests/<suite>/**/*.test, parse with mtr_parser, execute
    // each statement against the running zeta-mysqlwire endpoint, capture
    // output, diff with the matching .result file via result_diff, honor
    // skip_list.toml.
    anyhow::bail!("run_suite: not yet implemented")
}
