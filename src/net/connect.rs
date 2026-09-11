//! Connection setup (PLAN.md §5).
//!
//! Milestone 1 handles the v1 path: TCP connect under `--timeout`, then split the
//! stream into the reader/writer halves. The v2 probe and auto-fallback land in
//! milestone 3; until then `--transport v2` is rejected and `auto` uses v1.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bitcoin::p2p::Magic;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::cli::Transport as TransportPref;

use super::transport::{Reader, TransportKind, Writer};
use super::v1::{V1Reader, V1Writer};

/// Outcome of a successful connect: the two socket halves and which transport
/// ended up in use.
pub struct Connection {
    pub reader: Reader,
    pub writer: Writer,
    pub kind: TransportKind,
}

/// Open a connection to `addr`, honouring the transport preference.
pub async fn connect(
    addr: SocketAddr,
    pref: TransportPref,
    magic: Magic,
    connect_timeout: Duration,
) -> Result<Connection> {
    match pref {
        TransportPref::V2 => Err(anyhow!(
            "v2 (BIP324) transport is not implemented yet (milestone 3); use --transport v1"
        )),
        // `auto` will probe v2 first once milestone 3 lands; for now it is v1.
        TransportPref::V1 | TransportPref::Auto => connect_v1(addr, magic, connect_timeout).await,
    }
}

async fn connect_v1(addr: SocketAddr, magic: Magic, connect_timeout: Duration) -> Result<Connection> {
    let stream = timeout(connect_timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connect to {addr} timed out after {connect_timeout:?}"))?
        .with_context(|| format!("connecting to {addr}"))?;

    stream.set_nodelay(true).ok();
    let (rh, wh) = stream.into_split();

    Ok(Connection {
        reader: Reader::V1(V1Reader::new(rh, magic)),
        writer: Writer::V1(V1Writer::new(wh, magic)),
        kind: TransportKind::V1,
    })
}
