//! Spawns the `zeta` server binary as a child process exposing the MySQL
//! wire endpoint, and tears it down on shutdown.
//!
//! Independently re-derived. Does not copy from MTR's
//! `mysql-test-run.pl` (GPL Perl) or from the Apache-licensed companion
//! repo's harness — both implementations stand on their own.

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time;

/// Maximum time to wait for zeta's MySQL listener to come up.
const STARTUP_BUDGET: Duration = Duration::from_secs(30);

/// Maximum time to wait for graceful shutdown after SIGTERM.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

pub struct ZetaServer {
    child: Option<Child>,
    listener_port: u16,
    _data_dir: TempDir,
}

impl ZetaServer {
    pub async fn start(zeta_bin: &Path) -> Result<Self> {
        if !zeta_bin.is_file() {
            return Err(anyhow!(
                "no `zeta` binary at {} — pass --zeta-bin pointing at a built server binary",
                zeta_bin.display()
            ));
        }

        let listener_port = reserve_local_port()?;
        let data_dir = tempfile::Builder::new()
            .prefix("zeta-mtr-")
            .tempdir()
            .context("creating ephemeral data dir for zeta")?;

        let mut cmd = Command::new(zeta_bin);
        cmd.args([
            "--no-pg",
            "--bind",
            "127.0.0.1",
            "--storage-backend",
            "memory",
        ]);
        cmd.arg("--mysql-port").arg(listener_port.to_string());
        cmd.arg("--data-dir").arg(data_dir.path());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Default to a quiet log unless the operator explicitly raised the level.
        let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string());
        cmd.env("RUST_LOG", log_level);

        let mut child = cmd.spawn().context("spawning zeta child process")?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("captured stderr handle missing"))?;

        match time::timeout(STARTUP_BUDGET, await_ready(stderr, listener_port)).await {
            Ok(Ok(())) => {}
            Ok(Err(banner_err)) => {
                let _ = child.kill().await;
                return Err(banner_err);
            }
            Err(_) => {
                let _ = child.kill().await;
                return Err(anyhow!(
                    "zeta failed to advertise its MySQL listener on port {listener_port} within {:?}",
                    STARTUP_BUDGET
                ));
            }
        }

        Ok(ZetaServer {
            child: Some(child),
            listener_port,
            _data_dir: data_dir,
        })
    }

    #[allow(dead_code)]
    pub fn mysql_url(&self, database: &str) -> String {
        format!("mysql://root@127.0.0.1:{}/{}", self.listener_port, database)
    }

    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.listener_port
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        send_sigterm(&child);
        match time::timeout(SHUTDOWN_BUDGET, child.wait()).await {
            Ok(_) => Ok(()),
            Err(_) => {
                let _ = child.kill().await;
                Ok(())
            }
        }
    }
}

impl Drop for ZetaServer {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            send_sigterm(child);
            // tokio's Child::kill_on_drop ensures the OS reaps the process.
            // We sent SIGTERM first to give zeta a chance to flush state.
        }
    }
}

fn reserve_local_port() -> Result<u16> {
    // Bind to port 0, ask the kernel for the chosen port, then drop the socket
    // and immediately reuse the number when starting zeta. There is a small
    // race window — acceptable for a developer-machine test harness.
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("kernel refused to assign an ephemeral port")?;
    Ok(probe.local_addr()?.port())
}

fn send_sigterm(child: &Child) {
    let Some(raw_pid) = child.id() else {
        return;
    };
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(raw_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
}

/// Streams zeta's stderr until it prints the MySQL listener-ready banner.
/// All lines are forwarded to our stderr (prefixed `[zeta]`) so test
/// failures aren't blind.
async fn await_ready(stderr: tokio::process::ChildStderr, expected_port: u16) -> Result<()> {
    // Banner shape from zeta-server-bin/src/main.rs:
    //   "Zeta MySQL wire listener ready on <bind>:<port> (trust mode)"
    let banner =
        Regex::new(r"Zeta MySQL wire listener ready on \S+:(\d+)").expect("static regex compiles");

    let mut buf = BufReader::new(stderr).lines();
    while let Some(line) = buf.next_line().await? {
        eprintln!("[zeta] {}", line);
        let Some(caps) = banner.captures(&line) else {
            continue;
        };
        let actual: u16 = caps[1].parse().unwrap_or(0);
        if actual != expected_port {
            return Err(anyhow!(
                "zeta bound MySQL listener on {actual}, but harness expected {expected_port}"
            ));
        }
        return Ok(());
    }
    Err(anyhow!(
        "zeta exited before its MySQL listener became ready"
    ))
}
