use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;

mod class;
mod harness;
mod hazard;
mod mtr_parser;
mod oracle;
mod outcome;
mod record;
mod result_diff;
mod result_file;
mod runner;
mod side;
mod skip_list;

#[derive(Parser, Debug)]
#[command(version, about = "MTR-dialect MySQL-compat test runner for Zeta")]
struct Args {
    /// Path to a built `zeta` server binary. Not needed with `--record-oracle`.
    #[arg(long)]
    zeta_bin: Option<PathBuf>,

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

    /// Record mode (needs `--oracle-url`, no zeta): run the tests against the oracle and store what MySQL did for
    /// every statement under DIR as `<suite>/<name>.oracle.jsonl` (default `tests`, i.e. next to the `.test`
    /// files). The corpus is run `--record-passes` times to find nondeterministic answers.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "tests")]
    record_oracle: Option<PathBuf>,

    /// Record mode: how many times to run the corpus against the oracle (the second pass in reverse file order).
    #[arg(long, default_value_t = 2)]
    record_passes: u32,

    /// Record mode: a shell command that puts the oracle back to a pristine state and starts it; run before each
    /// pass and once at the end.
    #[arg(long)]
    oracle_restore_cmd: Option<String>,

    /// Only tests of this class: `mysql` (the default targets of MySQL compatibility) or `zeta-only` (the PostgreSQL
    /// grammar and sentinel tests listed in tests/zeta_only.list). Default: all.
    #[arg(long)]
    class: Option<String>,

    /// Offline: (re)write the `.result` file of every test that has an oracle record, from the record. No server
    /// is started.
    #[arg(long)]
    write_results: bool,

    /// Replay mode (needs `--zeta-bin`, no MySQL): compare zeta with the stored oracle outcomes under DIR
    /// (default `tests`). Prints the same rows and summary as the live differential mode.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "tests")]
    replay_oracle: Option<PathBuf>,
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
        class: args.class.as_deref(),
        suites: &suite_list,
        filter: args.filter.as_deref(),
        exclude: &args.exclude,
        report: args.report.as_deref(),
    };

    if args.record_oracle.is_some() && args.replay_oracle.is_some() {
        bail!("--record-oracle and --replay-oracle are different modes");
    }

    if args.write_results {
        return oracle::write_results(common());
    }

    // Record mode talks to the oracle only.
    if let Some(root) = &args.record_oracle {
        let Some(url) = &args.oracle_url else {
            bail!("--record-oracle needs --oracle-url");
        };
        return oracle::run_record(
            common(),
            url,
            args.allow_remote_oracle,
            root,
            args.record_passes,
            args.oracle_restore_cmd.as_deref(),
        )
        .await;
    }

    let Some(zeta_bin) = args.zeta_bin.clone() else {
        bail!("--zeta-bin is required (only --record-oracle runs without zeta)");
    };
    let mut zeta = harness::ZetaServer::start(&zeta_bin).await?;
    let url_base = format!("mysql://root@127.0.0.1:{}", zeta.port());

    if let Some(root) = &args.replay_oracle {
        if args.oracle_url.is_some() {
            let _ = zeta.shutdown().await;
            bail!("--replay-oracle compares with stored outcomes; drop --oracle-url (or use the live mode without --replay-oracle)");
        }
        let res = oracle::run_replay(&mut zeta, &zeta_bin, common(), root).await;
        let _ = zeta.shutdown().await;
        return res;
    }

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
