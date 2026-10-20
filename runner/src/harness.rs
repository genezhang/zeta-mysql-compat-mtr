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
        // Nothing reads stdout, and a pipe nobody drains stalls zeta for good once its 64 KiB
        // buffer fills (every thread blocks in write), so send it nowhere.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
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

    /// `false` once the server process has exited (crash, kill, or a clean exit nobody asked for).
    /// The exit status is logged the first time it is seen.
    pub fn is_alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                eprintln!("[zeta] server process exited: {status}");
                false
            }
            Err(e) => {
                eprintln!("[zeta] cannot query the server process: {e}");
                false
            }
        }
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

/// Reads one line as raw bytes and decodes it lossily. `Lines::next_line` fails with `InvalidData` on a
/// non-UTF-8 byte (a binary value in a log line is enough), and a drain that bails out drops the read end of
/// the pipe: zeta's next log write then fails or blocks, which is the failure the drain exists to prevent.
async fn next_lossy_line<R: AsyncBufReadExt + Unpin>(r: &mut R) -> std::io::Result<Option<String>> {
    let mut raw = Vec::new();
    if r.read_until(b'\n', &mut raw).await? == 0 {
        return Ok(None);
    }
    while matches!(raw.last(), Some(b'\n' | b'\r')) {
        raw.pop();
    }
    Ok(Some(String::from_utf8_lossy(&raw).into_owned()))
}

/// Streams zeta's stderr until it prints the MySQL listener-ready banner.
/// All lines are forwarded to our stderr (prefixed `[zeta]`) so test
/// failures aren't blind.
async fn await_ready(stderr: tokio::process::ChildStderr, expected_port: u16) -> Result<()> {
    // Banner shape from zeta-server-bin/src/main.rs:
    //   "Zeta MySQL wire listener ready on <bind>:<port> (trust mode)"
    let banner =
        Regex::new(r"Zeta MySQL wire listener ready on \S+:(\d+)").expect("static regex compiles");

    let mut buf = BufReader::new(stderr);
    while let Some(line) = next_lossy_line(&mut buf).await? {
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
        // Keep draining: a closed read end makes zeta's later log writes fail, a full one
        // blocks them. Forward the lines so a crash or panic is still visible, and say when the
        // stream ends (the server exited or closed stderr) so a crash is not silent.
        tokio::spawn(async move {
            loop {
                match next_lossy_line(&mut buf).await {
                    Ok(Some(line)) => eprintln!("[zeta] {}", line),
                    Ok(None) => {
                        eprintln!("[zeta] stderr closed: the server exited or closed its log");
                        break;
                    }
                    Err(e) => {
                        eprintln!("[zeta] stderr read failed: {e}");
                        break;
                    }
                }
            }
        });
        return Ok(());
    }
    Err(anyhow!(
        "zeta exited before its MySQL listener became ready"
    ))
}
