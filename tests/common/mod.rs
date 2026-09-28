//! Integration-test helpers (PLAN.md §15).
//!
//! Spawns a regtest `bitcoind` with the P2P port enabled and drives a `cli_ent`
//! session in-process against it, asserting via RPC. Requires a `bitcoind` on
//! PATH (the Nix dev shell provides one) or `BITCOIND_EXE` set; the `download`
//! feature fetches one in CI.

#![allow(dead_code)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command as ProcCommand};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use corepc_client::client_sync::v31::Client;
use corepc_client::client_sync::Auth;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use cli_ent::cli::Transport as TransportPref;
use cli_ent::messages::samples::SampleData;
use cli_ent::messages::summary::render_event;
use cli_ent::session::automations::Automations;
use cli_ent::session::events::{Command, Event};
use cli_ent::session::handshake::{Handshaker, Profile, VersionConfig};
use cli_ent::session::view::SessionView;
use cli_ent::session::{Session, SessionConfig};

/// A regtest `bitcoind` spawned directly (corepc-node's own spawn fails in this
/// sandbox); we keep `corepc-client` only for the RPC.
pub struct TestNode {
    child: Child,
    datadir: PathBuf,
    /// `127.0.0.1:<port>` to point cli-ent at.
    pub p2p_addr: String,
    pub client: Client,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.datadir);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn a regtest node with a P2P port. `v2` enables `-v2transport=1`.
pub fn spawn_node(v2: bool) -> TestNode {
    let exe = std::env::var("BITCOIND_EXE").unwrap_or_else(|_| "bitcoind".to_string());
    let p2p_port = free_port();
    let rpc_port = free_port();
    let datadir = std::env::temp_dir().join(format!("cli-ent-it-{}", std::process::id() as u64 * 1000 + rpc_port as u64));
    std::fs::create_dir_all(&datadir).expect("create datadir");

    let v2arg = if v2 { "-v2transport=1" } else { "-v2transport=0" };
    let child = ProcCommand::new(&exe)
        .args([
            "-regtest",
            &format!("-datadir={}", datadir.display()),
            "-listen=1",
            "-bind=127.0.0.1",
            &format!("-port={p2p_port}"),
            &format!("-rpcport={rpc_port}"),
            "-rpcuser=u",
            "-rpcpassword=p",
            "-fallbackfee=0.0001",
            v2arg,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn bitcoind");

    let client = Client::new_with_auth(
        &format!("http://127.0.0.1:{rpc_port}"),
        Auth::UserPass("u".to_string(), "p".to_string()),
    )
    .expect("rpc client");

    // Wait for RPC to come up.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if client.call::<Value>("getblockchaininfo", &[]).is_ok() {
            break;
        }
        if Instant::now() > deadline {
            panic!("bitcoind RPC did not come up");
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    TestNode {
        child,
        datadir,
        p2p_addr: format!("127.0.0.1:{p2p_port}"),
        client,
    }
}

/// A running in-process cli_ent session driven against a node.
pub struct Harness {
    pub view: Arc<Mutex<SessionView>>,
    pub commands: UnboundedSender<Command>,
    pub samples: Arc<SampleData>,
    /// Every event, rendered exactly as the REPL would print it.
    pub lines: Arc<Mutex<Vec<String>>>,
    _events: tokio::task::JoinHandle<()>,
    _session: tokio::task::JoinHandle<()>,
}

impl Harness {
    /// Start a session (not yet connected).
    pub fn start(default_transport: TransportPref) -> Self {
        let magic = bitcoin::p2p::Magic::from(bitcoin::Network::Regtest);
        let handshaker = Handshaker {
            profile: Profile::Core,
            verack_manual: false,
            version: VersionConfig::default(),
        };
        let config = SessionConfig {
            default_port: 18444,
            magic,
            default_transport,
            timeout: Duration::from_secs(10),
            verack_delay: None,
            handshaker,
        };
        let samples = Arc::new(SampleData::new(bitcoin::Network::Regtest));
        let automations = Automations::new(samples.clone());
        let view = Arc::new(Mutex::new(SessionView::new()));
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let session = Session::new(config, automations, view.clone(), ev_tx, cmd_rx);
        let session_handle = tokio::spawn(session.run());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let events_handle = tokio::spawn(drain(ev_rx, lines.clone()));

        Harness {
            view,
            commands: cmd_tx,
            samples,
            lines,
            _events: events_handle,
            _session: session_handle,
        }
    }

    pub fn connect(&self, addr: String, transport: TransportPref) {
        let _ = self.commands.send(Command::Connect {
            addr,
            transport: Some(transport),
        });
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    /// Wait until the handshake is ready, or panic on timeout.
    pub async fn wait_ready(&self, timeout: Duration) {
        poll(timeout, || self.view.lock().unwrap().peer.handshake.is_ready())
            .await
            .expect("handshake did not reach Ready in time");
    }

    /// Whether our session currently believes it is connected.
    pub fn connected(&self) -> bool {
        self.view.lock().unwrap().is_connected()
    }
}

/// Render every event the way the REPL printer does, so tests can assert on the
/// output a user would actually see.
async fn drain(mut rx: UnboundedReceiver<Event>, lines: Arc<Mutex<Vec<String>>>) {
    let t0 = Instant::now();
    while let Some(ev) = rx.recv().await {
        if let Some(line) = render_event(&ev, t0, false) {
            lines.lock().unwrap().push(line);
        }
    }
}

/// Poll `cond` until true or the timeout elapses.
pub async fn poll<F: Fn() -> bool>(timeout: Duration, cond: F) -> Result<(), ()> {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Call an RPC method returning raw JSON.
pub fn rpc(node: &TestNode, method: &str) -> Value {
    node.client.call(method, &[]).expect("rpc call")
}

/// The single connected peer's getpeerinfo entry, if any.
pub fn peer_info(node: &TestNode) -> Option<Value> {
    let peers = rpc(node, "getpeerinfo");
    peers.as_array().and_then(|a| a.first().cloned())
}
