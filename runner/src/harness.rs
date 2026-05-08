#![allow(dead_code)] // skeleton — fields/methods land as the runner is fleshed out

use anyhow::Result;
use std::path::Path;

/// Spawns the `zeta` server binary on an ephemeral TCP port and exposes the
/// MySQL wire endpoint to the MTR runner. Independently re-derived from the
/// equivalent in the sibling `zeta-mysql-compat` repo — no shared code.
pub struct ZetaServer {
    // TODO: child process handle, port, tempdir
}

impl ZetaServer {
    pub async fn start(_zeta_bin: &Path) -> Result<Self> {
        // TODO: spawn `zeta --bind 127.0.0.1 --port 0 --data-dir <tmp>` with
        // MySQL listener flags; scrape the chosen port; wait for readiness.
        anyhow::bail!("ZetaServer::start: not yet implemented")
    }

    pub fn mysql_port(&self) -> u16 {
        unimplemented!()
    }
}
