//! Connection setup and auto-fallback (PLAN.md §5).
//!
//! `v1` connects plaintext. `v2` runs the BIP324 handshake and never falls back.
//! `auto` opens a TCP connection, sends the v2 key + garbage, and waits up to a
//! fixed 3s probe window for the peer's key; if the peer closes, resets, times
//! out, or turns out to be v1, it opens a **fresh** TCP connection and runs v1.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bitcoin::p2p::Magic;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::cli::Transport as TransportPref;

use super::transport::{Reader, TransportKind, Writer};
use super::v1::{V1Reader, V1Writer};
use super::v2::{self, V2Connection};

/// Fixed v2 probe window for `--transport auto` (PLAN.md §5).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Outcome of a successful connect.
pub struct Connection {
    pub reader: Reader,
    pub writer: Writer,
    pub kind: TransportKind,
    /// The v2 session id, if the connection is v2.
    pub session_id: Option<[u8; 32]>,
    /// Set when `auto` probed v2 and fell back to v1; carries the reason.
    pub fell_back: Option<String>,
}

/// Open a connection to `addr`, honouring the transport preference.
pub async fn connect(
    addr: SocketAddr,
    pref: TransportPref,
    magic: Magic,
    connect_timeout: Duration,
) -> Result<Connection> {
    match pref {
        TransportPref::V1 => connect_v1(addr, magic, connect_timeout, None).await,
        TransportPref::V2 => connect_v2(addr, magic, connect_timeout).await,
        TransportPref::Auto => connect_auto(addr, magic, connect_timeout).await,
    }
}

async fn tcp_connect(addr: SocketAddr, connect_timeout: Duration) -> Result<TcpStream> {
    let stream = timeout(connect_timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connect to {addr} timed out after {connect_timeout:?}"))?
        .with_context(|| format!("connecting to {addr}"))?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

async fn connect_v1(
    addr: SocketAddr,
    magic: Magic,
    connect_timeout: Duration,
    fell_back: Option<String>,
) -> Result<Connection> {
    let stream = tcp_connect(addr, connect_timeout).await?;
    let (rh, wh) = stream.into_split();
    Ok(Connection {
        reader: Reader::V1(V1Reader::new(rh, magic)),
        writer: Writer::V1(V1Writer::new(wh, magic)),
        kind: TransportKind::V1,
        session_id: None,
        fell_back,
    })
}

async fn connect_v2(addr: SocketAddr, magic: Magic, connect_timeout: Duration) -> Result<Connection> {
    let stream = tcp_connect(addr, connect_timeout).await?;
    let conn = v2::client_handshake(stream, magic, PROBE_TIMEOUT, connect_timeout)
        .await
        .map_err(|e| anyhow!("v2 handshake failed: {}", e.message()))?;
    Ok(from_v2(conn, None))
}

async fn connect_auto(addr: SocketAddr, magic: Magic, connect_timeout: Duration) -> Result<Connection> {
    let stream = tcp_connect(addr, connect_timeout).await?;
    match v2::client_handshake(stream, magic, PROBE_TIMEOUT, connect_timeout).await {
        Ok(conn) => Ok(from_v2(conn, None)),
        Err(e) if e.is_fallback() => {
            // Fresh TCP connection for the v1 attempt (the v2 probe bytes poisoned
            // the first one from the peer's point of view).
            connect_v1(addr, magic, connect_timeout, Some(e.message())).await
        }
        Err(e) => Err(anyhow!("v2 handshake failed: {}", e.message())),
    }
}

fn from_v2(conn: V2Connection, fell_back: Option<String>) -> Connection {
    Connection {
        reader: Reader::V2(conn.reader),
        writer: Writer::V2(conn.writer),
        kind: TransportKind::V2,
        session_id: Some(conn.session_id),
        fell_back,
    }
}
