//! The session task (PLAN.md §3, §6).
//!
//! Owns the socket halves and runs a `select!` loop over incoming messages and
//! REPL/script commands. It drives the handshake, keeps [`SessionView`] current,
//! and emits [`Event`]s. Milestone 1 covers connect, the profile-driven
//! handshake, sending, and clean teardown with stats.

pub mod automations;
pub mod events;
pub mod handshake;
pub mod view;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::Magic;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::cli::Transport as TransportPref;
use crate::net::connect::connect;
use crate::net::transport::{FrameError, Reader, TransportKind, Wire, Writer};

use self::events::{Command, DisconnectReason, Direction, Event, HandshakeState};
use self::handshake::Handshaker;
use self::view::SessionView;

/// The running session: transport halves, shared view, channels, and handshake state.
pub struct Session {
    reader: Reader,
    writer: Writer,
    view: Arc<Mutex<SessionView>>,
    events: UnboundedSender<Event>,
    commands: UnboundedReceiver<Command>,
    handshaker: Handshaker,
    hs: HandshakeState,
    addr: SocketAddr,
    seq: u64,
    ready_emitted: bool,
}

impl Session {
    /// Connect to the peer and prepare the session (no messages sent yet).
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        peer_spec: String,
        addr: SocketAddr,
        pref: TransportPref,
        magic: Magic,
        connect_timeout: Duration,
        handshaker: Handshaker,
        view: Arc<Mutex<SessionView>>,
        events: UnboundedSender<Event>,
        commands: UnboundedReceiver<Command>,
    ) -> Result<Self> {
        // Milestone 1 uses v1; emit the kind we will actually connect with.
        let _ = events.send(Event::Connecting {
            addr,
            transport: TransportKind::V1,
        });

        let conn = connect(addr, pref, magic, connect_timeout).await?;

        {
            let mut v = view.lock().unwrap();
            v.peer.addr = Some(addr);
            v.peer.transport = Some(conn.kind);
            v.peer.resolved_from = (peer_spec != addr.to_string()).then_some(peer_spec);
            v.stats.connected_at = Some(Instant::now());
        }

        let _ = events.send(Event::Connected {
            addr,
            transport: conn.kind,
        });

        Ok(Self {
            reader: conn.reader,
            writer: conn.writer,
            view,
            events,
            commands,
            handshaker,
            hs: HandshakeState::default(),
            addr,
            seq: 1,
            ready_emitted: false,
        })
    }

    /// Run the session until disconnect. Consumes `self`.
    pub async fn run(mut self) {
        // Profile's on-connect sends (core/minimal: version; manual: nothing).
        for msg in self.handshaker.on_connect(self.addr) {
            self.send(msg, None).await;
        }

        let reason = loop {
            tokio::select! {
                incoming = self.reader.read_message() => {
                    match incoming {
                        Ok(wire) => self.handle_recv(wire).await,
                        Err(FrameError::Eof) => break DisconnectReason::PeerClosed,
                        Err(FrameError::Reset) => break DisconnectReason::PeerReset,
                        Err(e) => break DisconnectReason::FrameError(e.to_string()),
                    }
                }
                cmd = self.commands.recv() => {
                    match cmd {
                        Some(Command::Send(msg)) => self.send(msg, None).await,
                        Some(Command::SendRaw(bytes)) => self.send_raw(bytes, None).await,
                        Some(Command::Disconnect) | Some(Command::Quit) | None => {
                            break DisconnectReason::UserRequested
                        }
                    }
                }
            }
        };

        let _ = self.writer.close().await;
        let stats = self.view.lock().unwrap().stats.clone();
        let _ = self.events.send(Event::Disconnected { reason, stats });
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    /// Encode, write, record, and emit a message. `tag` labels automatic or
    /// misbehaving sends (e.g. `auto: pong`).
    async fn send(&mut self, msg: NetworkMessage, tag: Option<String>) {
        let (bytes, wire) = self.writer.encode(&msg);
        if let Err(e) = self.writer.write_bytes(&bytes).await {
            let _ = self.events.send(Event::Error(format!("write failed: {e}")));
            return;
        }

        // Handshake flags follow the message actually sent.
        match &msg {
            NetworkMessage::Version(_) => self.hs.version_sent = true,
            NetworkMessage::Verack => self.hs.verack_sent = true,
            _ => {}
        }

        let seq = self.next_seq();
        let at = Instant::now();
        self.view.lock().unwrap().record(seq, Direction::Sent, &wire, at);
        let _ = self.events.send(Event::Sent { seq, wire, at, tag });
        self.sync_handshake();
    }

    /// Send pre-framed bytes verbatim (misbehaviour / raw path). The parsed
    /// message is unknown, so it is recorded with `msg = None`.
    async fn send_raw(&mut self, bytes: Vec<u8>, tag: Option<String>) {
        if let Err(e) = self.writer.write_bytes(&bytes).await {
            let _ = self.events.send(Event::Error(format!("write failed: {e}")));
            return;
        }
        let wire = crate::net::transport::Wire {
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

    /// Handle one received message: record it, react per the profile, warn on
    /// decode failure, and update handshake state.
    async fn handle_recv(&mut self, wire: Wire) {
        let seq = self.next_seq();
        let at = Instant::now();
        self.view.lock().unwrap().record(seq, Direction::Recv, &wire, at);

        // Decide our reaction and update state before moving `wire` into the event.
        let mut responses = Vec::new();
        if let Some(msg) = &wire.msg {
            match msg {
                NetworkMessage::Version(v) => {
                    self.hs.version_received = true;
                    self.view.lock().unwrap().peer.peer_version = Some(v.clone());
                    responses = self.handshaker.on_peer_version(v);
                }
                NetworkMessage::Verack => {
                    self.hs.verack_received = true;
                    responses = self.handshaker.on_peer_verack();
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
            self.send(msg, None).await;
        }
        self.sync_handshake();
    }

    /// Mirror handshake flags into the view and emit `Ready` once.
    fn sync_handshake(&mut self) {
        self.view.lock().unwrap().peer.handshake = self.hs;
        if self.hs.is_ready() && !self.ready_emitted {
            self.ready_emitted = true;
            let _ = self.events.send(Event::Ready);
        }
    }
}
