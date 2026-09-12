//! The session task (PLAN.md §3, §6).
//!
//! A long-lived actor: it starts disconnected, and processes commands. While
//! disconnected it waits for `Connect`; while connected it runs a `select!` loop
//! over incoming messages and commands, driving the handshake, keeping
//! [`SessionView`] current, and emitting [`Event`]s. `Disconnect` returns it to
//! the idle state (the task stays alive); `Quit` ends it. Sequence numbers
//! continue across reconnects (PLAN.md §3).

pub mod automations;
pub mod events;
pub mod handshake;
pub mod view;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::Magic;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::cli::Transport as TransportPref;
use crate::net::connect::connect;
use crate::net::resolve::resolve;
use crate::net::transport::{FrameError, Reader, TransportKind, Wire, Writer};

use self::events::{Command, DisconnectReason, Direction, Event, HandshakeState};
use self::handshake::Handshaker;
use self::view::SessionView;

/// Immutable connection parameters shared across (re)connects.
pub struct SessionConfig {
    pub default_port: u16,
    pub magic: Magic,
    pub default_transport: TransportPref,
    pub timeout: Duration,
    /// Delay applied only to the verack we send (PLAN §6). Other post-version
    /// sends go out immediately.
    pub verack_delay: Option<Duration>,
    pub handshaker: Handshaker,
}

/// Per-connection state, held locally while connected so it does not alias the
/// actor's other fields during `select!`.
struct ConnState {
    reader: Reader,
    writer: Writer,
    hs: HandshakeState,
    addr: SocketAddr,
    ready_emitted: bool,
    /// A pending delayed verack (armed by `--verack-delay`).
    pending_verack: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

/// How a connected phase ended.
enum ConnOutcome {
    /// Back to idle; the task keeps running.
    Disconnected(DisconnectReason),
    /// End the whole task.
    Quit(DisconnectReason),
}

/// The session actor.
pub struct Session {
    config: SessionConfig,
    view: Arc<Mutex<SessionView>>,
    events: UnboundedSender<Event>,
    commands: UnboundedReceiver<Command>,
    seq: u64,
}

impl Session {
    pub fn new(
        config: SessionConfig,
        view: Arc<Mutex<SessionView>>,
        events: UnboundedSender<Event>,
        commands: UnboundedReceiver<Command>,
    ) -> Self {
        Self {
            config,
            view,
            events,
            commands,
            seq: 1,
        }
    }

    /// Run until `Quit` (or all command senders drop). Consumes `self`.
    pub async fn run(mut self) {
        loop {
            // Idle phase: only a command can move us forward.
            match self.commands.recv().await {
                None | Some(Command::Quit) => break,
                Some(Command::Connect { addr, transport }) => {
                    match self.establish(&addr, transport).await {
                        Ok(mut conn) => {
                            self.after_connect(&mut conn).await;
                            match self.run_connected(&mut conn).await {
                                ConnOutcome::Disconnected(reason) => {
                                    self.teardown(&mut conn, reason).await;
                                }
                                ConnOutcome::Quit(reason) => {
                                    self.teardown(&mut conn, reason).await;
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = self.events.send(Event::Error(e.to_string()));
                        }
                    }
                }
                Some(Command::Disconnect) => {
                    let _ = self
                        .events
                        .send(Event::Error("not connected".to_string()));
                }
                Some(Command::Send(_)) | Some(Command::SendRaw(_)) => {
                    let _ = self
                        .events
                        .send(Event::Error("not connected".to_string()));
                }
            }
        }
    }

    /// Resolve, connect, and set up view/events. Returns the fresh connection.
    async fn establish(
        &mut self,
        spec: &str,
        transport: Option<TransportPref>,
    ) -> anyhow::Result<ConnState> {
        let pref = transport.unwrap_or(self.config.default_transport);
        let addr = resolve(spec, self.config.default_port)?;

        let attempt = match pref {
            TransportPref::V1 => TransportKind::V1,
            TransportPref::V2 | TransportPref::Auto => TransportKind::V2,
        };
        let _ = self.events.send(Event::Connecting {
            addr,
            transport: attempt,
        });

        let conn = connect(addr, pref, self.config.magic, self.config.timeout).await?;

        if let Some(reason) = &conn.fell_back {
            let _ = self.events.send(Event::FellBackToV1 {
                reason: reason.clone(),
            });
        }

        {
            let mut v = self.view.lock().unwrap();
            v.reset_connection();
            v.peer.addr = Some(addr);
            v.peer.transport = Some(conn.kind);
            v.peer.v2_session_id = conn.session_id;
            v.peer.fell_back = conn.fell_back.clone();
            v.peer.resolved_from = (spec != addr.to_string()).then(|| spec.to_string());
            v.stats.connected_at = Some(Instant::now());
        }

        let _ = self.events.send(Event::Connected {
            addr,
            transport: conn.kind,
            v2_session_id: conn.session_id,
        });

        Ok(ConnState {
            reader: conn.reader,
            writer: conn.writer,
            hs: HandshakeState::default(),
            addr,
            ready_emitted: false,
            pending_verack: None,
        })
    }

    /// Send the profile's on-connect messages.
    async fn after_connect(&mut self, conn: &mut ConnState) {
        for msg in self.config.handshaker.on_connect(conn.addr) {
            self.send(conn, msg, None).await;
        }
    }

    /// The connected select loop.
    async fn run_connected(&mut self, conn: &mut ConnState) -> ConnOutcome {
        loop {
            tokio::select! {
                _ = tick(&mut conn.pending_verack) => {
                    conn.pending_verack = None;
                    self.send(conn, NetworkMessage::Verack, Some("verack: delayed".into())).await;
                }
                incoming = conn.reader.read_message() => {
                    match incoming {
                        Ok(wire) => self.handle_recv(conn, wire).await,
                        Err(FrameError::Eof) => {
                            return ConnOutcome::Disconnected(DisconnectReason::PeerClosed)
                        }
                        Err(FrameError::Reset) => {
                            return ConnOutcome::Disconnected(DisconnectReason::PeerReset)
                        }
                        Err(e) => {
                            return ConnOutcome::Disconnected(DisconnectReason::FrameError(
                                e.to_string(),
                            ))
                        }
                    }
                }
                cmd = self.commands.recv() => {
                    match cmd {
                        Some(Command::Send(msg)) => self.send(conn, msg, None).await,
                        Some(Command::SendRaw(bytes)) => self.send_raw(conn, bytes, None).await,
                        Some(Command::Connect { .. }) => {
                            let _ = self.events.send(Event::Error(
                                "already connected; disconnect first".to_string(),
                            ));
                        }
                        Some(Command::Disconnect) => {
                            return ConnOutcome::Disconnected(DisconnectReason::UserRequested)
                        }
                        Some(Command::Quit) | None => {
                            return ConnOutcome::Quit(DisconnectReason::UserRequested)
                        }
                    }
                }
            }
        }
    }

    /// Close the socket, emit final stats, and mark the view disconnected.
    async fn teardown(&mut self, conn: &mut ConnState, reason: DisconnectReason) {
        let _ = conn.writer.close().await;
        let stats = {
            let mut v = self.view.lock().unwrap();
            let stats = v.stats.clone();
            v.reset_connection();
            stats
        };
        let _ = self.events.send(Event::Disconnected { reason, stats });
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    /// Encode, write, record, and emit a message.
    async fn send(&mut self, conn: &mut ConnState, msg: NetworkMessage, tag: Option<String>) {
        let (bytes, wire) = conn.writer.encode(&msg);
        if let Err(e) = conn.writer.write_bytes(&bytes).await {
            let _ = self.events.send(Event::Error(format!("write failed: {e}")));
            return;
        }

        match &msg {
            NetworkMessage::Version(_) => conn.hs.version_sent = true,
            NetworkMessage::Verack => conn.hs.verack_sent = true,
            _ => {}
        }

        let seq = self.next_seq();
        let at = Instant::now();
        self.view.lock().unwrap().record(seq, Direction::Sent, &wire, at);
        let _ = self.events.send(Event::Sent { seq, wire, at, tag });
        self.sync_handshake(conn);
    }

    /// Send pre-framed bytes verbatim (raw / misbehaviour path).
    async fn send_raw(&mut self, conn: &mut ConnState, bytes: Vec<u8>, tag: Option<String>) {
        if let Err(e) = conn.writer.write_bytes(&bytes).await {
            let _ = self.events.send(Event::Error(format!("write failed: {e}")));
            return;
        }
        let wire = Wire {
            msg: None,
            frame: crate::net::v1::frame_kind(&bytes),
            raw: bytes,
            decode_error: None,
        };
        let seq = self.next_seq();
        let at = Instant::now();
        self.view.lock().unwrap().record(seq, Direction::Sent, &wire, at);
        let _ = self.events.send(Event::Sent { seq, wire, at, tag });
    }

    /// Handle one received message: record, react per the profile, warn on decode
    /// failure, and update handshake state.
    async fn handle_recv(&mut self, conn: &mut ConnState, wire: Wire) {
        let seq = self.next_seq();
        let at = Instant::now();
        self.view.lock().unwrap().record(seq, Direction::Recv, &wire, at);

        let mut responses = Vec::new();
        if let Some(msg) = &wire.msg {
            match msg {
                NetworkMessage::Version(v) => {
                    conn.hs.version_received = true;
                    self.view.lock().unwrap().peer.peer_version = Some(v.clone());
                    responses = self.config.handshaker.on_peer_version(v);
                }
                NetworkMessage::Verack => {
                    conn.hs.verack_received = true;
                    responses = self.config.handshaker.on_peer_verack();
                }
                _ => {}
            }
        }

        let decode_error = wire.decode_error.clone();
        let command = wire.command();
        let _ = self.events.send(Event::Recv { seq, wire, at });
        if let Some(error) = decode_error {
            let _ = self.events.send(Event::DecodeWarning {
                seq,
                command,
                error,
            });
        }

        for msg in responses {
            // --verack-delay: hold back only the verack; send everything else now.
            if matches!(msg, NetworkMessage::Verack) {
                if let Some(delay) = self.config.verack_delay {
                    conn.pending_verack = Some(Box::pin(tokio::time::sleep(delay)));
                    continue;
                }
            }
            self.send(conn, msg, None).await;
        }
        self.sync_handshake(conn);
    }

    // (helper `tick` lives at module scope, below.)

    /// Mirror handshake flags into the view and emit `Ready` once per connection.
    fn sync_handshake(&mut self, conn: &mut ConnState) {
        self.view.lock().unwrap().peer.handshake = conn.hs;
        if conn.hs.is_ready() && !conn.ready_emitted {
            conn.ready_emitted = true;
            let _ = self.events.send(Event::Ready);
        }
    }
}

/// Await a pending delayed-verack timer, or never resolve when none is armed.
async fn tick(pending: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>) {
    match pending {
        Some(sleep) => sleep.as_mut().await,
        None => std::future::pending().await,
    }
}
