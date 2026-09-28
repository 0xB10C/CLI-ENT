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

use self::automations::{AutoKind, Automations};
use self::events::{Command, DisconnectReason, Direction, Event, HandshakeState};
use self::handshake::Handshaker;
use self::view::SessionView;

/// Print the opening sends of a `spam` run in full, so short bursts are shown
/// whole whatever their rate.
const SPAM_PRINT_HEAD: u64 = 10;

/// Past the head, print at most one `spam` send per this interval; the rest are
/// counted only.
const SPAM_PRINT_INTERVAL: Duration = Duration::from_millis(500);

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
    /// Whether the peer sent `wtxidrelay` (used by the getdata automation).
    wtxidrelay: bool,
    /// A pending delayed verack (armed by `--verack-delay`).
    pending_verack: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    /// Bytes held back by `hold` (a partial message).
    held: Option<Vec<u8>>,
    /// Slowloris drip: `(chunk_size, interval)` releasing the held bytes.
    drip: Option<(usize, Duration)>,
    /// A running spam.
    spam: Option<SpamState>,
    /// When true, the select loop stops reading from the peer.
    reads_paused: bool,
}

/// A running `spam`.
struct SpamState {
    msg: NetworkMessage,
    /// Pause between sends; `None` sends as fast as `write` accepts.
    interval: Option<Duration>,
    /// Remaining sends for a bounded burst.
    remaining: Option<u64>,
    sent: u64,
    started: Instant,
    /// When the last send was printed; `None` until the first one is.
    last_print: Option<Instant>,
    /// Sends counted but not printed since `last_print`.
    since_print: u64,
}

impl SpamState {
    /// Count one send and decide whether to show it. Returns `Some((total,
    /// suppressed))` for a send that should be printed — the first
    /// [`SPAM_PRINT_HEAD`], then one per [`SPAM_PRINT_INTERVAL`] — where
    /// `suppressed` is how many sends went unprinted since the previous printed
    /// one.
    fn note_send(&mut self, at: Instant) -> Option<(u64, u64)> {
        self.sent += 1;
        let due = self.sent <= SPAM_PRINT_HEAD
            || match self.last_print {
                Some(prev) => at.duration_since(prev) >= SPAM_PRINT_INTERVAL,
                None => true,
            };
        if !due {
            self.since_print += 1;
            return None;
        }
        let shown = (self.sent, self.since_print);
        self.last_print = Some(at);
        self.since_print = 0;
        Some(shown)
    }
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
    automations: Automations,
    seq: u64,
}

impl Session {
    pub fn new(
        config: SessionConfig,
        automations: Automations,
        view: Arc<Mutex<SessionView>>,
        events: UnboundedSender<Event>,
        commands: UnboundedReceiver<Command>,
    ) -> Self {
        view.lock().unwrap().automations = automations.state;
        Self {
            config,
            view,
            events,
            commands,
            automations,
            seq: 1,
        }
    }

    /// Apply an automation toggle and mirror it to the view.
    fn set_auto(&mut self, kind: AutoKind, on: bool) {
        self.automations.state.set(kind, on);
        self.view.lock().unwrap().automations = self.automations.state;
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
                Some(Command::SetAuto { kind, on }) => self.set_auto(kind, on),
                // Everything else needs a connection.
                Some(_) => {
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
            wtxidrelay: false,
            pending_verack: None,
            held: None,
            drip: None,
            spam: None,
            reads_paused: false,
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
                _ = spam_wait(&conn.spam) => {
                    self.spam_step(conn).await;
                }
                _ = drip_wait(&conn.drip, conn.held.is_some()) => {
                    self.drip_step(conn).await;
                }
                incoming = conn.reader.read_message(), if !conn.reads_paused => {
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
                        Some(Command::Disconnect) => {
                            return ConnOutcome::Disconnected(DisconnectReason::UserRequested)
                        }
                        Some(Command::Quit) | None => {
                            return ConnOutcome::Quit(DisconnectReason::UserRequested)
                        }
                        Some(other) => self.dispatch_command(conn, other).await,
                    }
                }
            }
        }
    }

    /// Handle a non-terminal command while connected.
    async fn dispatch_command(&mut self, conn: &mut ConnState, cmd: Command) {
        // Guard: while holding a partial message (and not dripping), an
        // interleaved send would corrupt the v1 frame or desync the v2 cipher.
        let interleaving = matches!(
            cmd,
            Command::Send(_)
                | Command::SendRaw(_)
                | Command::Craft { .. }
                | Command::Mangle { .. }
                | Command::Spam { .. }
        );
        if interleaving && conn.held.is_some() && conn.drip.is_none() {
            let _ = self
                .events
                .send(Event::Error("holding a partial message; release or drop first".into()));
            return;
        }

        match cmd {
            Command::Send(msg) => self.send(conn, msg, None).await,
            Command::SendRaw(bytes) => self.send_raw(conn, bytes, None).await,
            Command::SetAuto { kind, on } => self.set_auto(kind, on),
            Command::Craft { command, payload, tag } => {
                let (bytes, wire) = conn.writer.encode_raw(&command, &payload);
                self.write_and_record(conn, &bytes, wire, Some(tag)).await;
            }
            Command::Mangle { msg, kind } => self.mangle_send(conn, msg, kind).await,
            Command::Oversize { msg, bytes } => {
                let (command, payload) =
                    crate::net::v1::command_and_payload(self.config.magic, &msg);
                let padded = crate::misbehave::oversize_payload(&payload, bytes);
                let (wire_bytes, wire) = conn.writer.encode_raw(&command, &padded);
                let tag = format!("misbehave: oversize {bytes} bytes");
                self.write_and_record(conn, &wire_bytes, wire, Some(tag)).await;
            }
            Command::Spam { msg, rate, count } => {
                conn.spam = Some(SpamState {
                    msg,
                    interval: rate.filter(|r| *r > 0).map(|r| Duration::from_secs_f64(1.0 / r as f64)),
                    remaining: count,
                    sent: 0,
                    started: Instant::now(),
                    last_print: None,
                    since_print: 0,
                });
            }
            Command::StopSpam => self.stop_spam(conn),
            Command::Hold { msg, keep, drip } => self.hold(conn, msg, keep, drip).await,
            Command::Release => self.release(conn).await,
            Command::Drop => {
                if conn.held.take().is_some() {
                    conn.drip = None;
                    let _ = self.events.send(Event::Error("held bytes dropped".into()));
                }
            }
            Command::PauseReads(on) => {
                conn.reads_paused = on;
            }
            Command::Connect { .. } => {
                let _ = self
                    .events
                    .send(Event::Error("already connected; disconnect first".into()));
            }
            // Disconnect/Quit handled by the caller.
            Command::Disconnect | Command::Quit => {}
        }
    }

    /// Encode a message and mangle the encoded bytes, if the mangle applies to
    /// this transport.
    async fn mangle_send(&mut self, conn: &mut ConnState, msg: NetworkMessage, kind: crate::misbehave::MangleKind) {
        let is_v2 = matches!(conn.writer.kind(), TransportKind::V2);
        if !kind.applies_to(is_v2) {
            let where_ = if is_v2 { "v2" } else { "v1" };
            let _ = self
                .events
                .send(Event::Error(format!("{} — not applicable on {where_}", kind.label())));
            return;
        }
        let (command, payload) = crate::net::v1::command_and_payload(self.config.magic, &msg);
        let (bytes, _wire) = conn.writer.encode_raw(&command, &payload);
        let mangled = crate::misbehave::mangle(&bytes, kind);
        // Record the mangled bytes we actually put on the wire.
        let wire = Wire {
            msg: None,
            frame: crate::net::v1::frame_kind(&mangled),
            raw: mangled.clone(),
            decode_error: None,
        };
        self.write_and_record(conn, &mangled, wire, Some(kind.label())).await;
    }

    /// Begin holding a partial message: send all but the last `keep` bytes.
    async fn hold(
        &mut self,
        conn: &mut ConnState,
        msg: NetworkMessage,
        keep: usize,
        drip: Option<(usize, Duration)>,
    ) {
        if conn.held.is_some() {
            let _ = self.events.send(Event::Error("already holding; release or drop first".into()));
            return;
        }
        let (bytes, _wire) = conn.writer.encode(&msg);
        let keep = keep.min(bytes.len());
        let split = bytes.len() - keep;
        let (prefix, suffix) = bytes.split_at(split);
        let tag = format!("misbehave: hold {}/{} bytes", keep, bytes.len());
        let wire = Wire {
            msg: None,
            frame: crate::net::v1::frame_kind(prefix),
            raw: prefix.to_vec(),
            decode_error: None,
        };
        self.write_and_record(conn, prefix, wire, Some(tag)).await;
        conn.held = Some(suffix.to_vec());
        conn.drip = drip;
    }

    /// Release all held bytes at once.
    async fn release(&mut self, conn: &mut ConnState) {
        match conn.held.take() {
            Some(bytes) => {
                conn.drip = None;
                let wire = Wire {
                    msg: None,
                    frame: crate::net::v1::frame_kind(&bytes),
                    raw: bytes.clone(),
                    decode_error: None,
                };
                self.write_and_record(conn, &bytes, wire, Some("misbehave: release".into()))
                    .await;
            }
            None => {
                let _ = self.events.send(Event::Error("nothing held".into()));
            }
        }
    }

    /// Release one drip chunk of the held bytes.
    async fn drip_step(&mut self, conn: &mut ConnState) {
        let Some((chunk, _)) = conn.drip else { return };
        let Some(held) = conn.held.as_mut() else { return };
        let n = chunk.min(held.len());
        let piece: Vec<u8> = held.drain(..n).collect();
        let done = held.is_empty();
        if let Err(e) = conn.writer.write_bytes(&piece).await {
            let _ = self.events.send(Event::Error(format!("drip write failed: {e}")));
        }
        if done {
            conn.held = None;
            conn.drip = None;
            let _ = self.events.send(Event::Error("drip complete".into()));
        }
    }

    /// Send one spam packet (re-encoding each time so the v2 cipher advances).
    ///
    /// Every send is counted, but past the opening [`SPAM_PRINT_HEAD`] only one
    /// per [`SPAM_PRINT_INTERVAL`] is ringed and printed: at `--rate 5000/s` a
    /// line each would drown the terminal and flush the ring.
    async fn spam_step(&mut self, conn: &mut ConnState) {
        let msg = match &conn.spam {
            Some(s) => s.msg.clone(),
            None => return,
        };
        let (bytes, wire) = conn.writer.encode(&msg);
        if conn.writer.write_bytes(&bytes).await.is_err() {
            self.stop_spam(conn);
            return;
        }

        let at = Instant::now();
        let mut show = None;
        let mut finished = false;
        if let Some(s) = conn.spam.as_mut() {
            show = s.note_send(at);
            if let Some(r) = s.remaining.as_mut() {
                *r -= 1;
                finished = *r == 0;
            }
        }

        match show {
            Some((sent, skipped)) => {
                let seq = self.next_seq();
                self.view.lock().unwrap().record(seq, Direction::Sent, &wire, at);
                let tag = match skipped {
                    0 => format!("spam #{sent}"),
                    n => format!("spam #{sent}, {n} more not shown"),
                };
                let _ = self.events.send(Event::Sent { seq, wire, at, tag: Some(tag) });
            }
            None => self.view.lock().unwrap().count_sent(&wire),
        }

        if finished {
            self.stop_spam(conn);
        }
    }

    fn stop_spam(&mut self, conn: &mut ConnState) {
        if let Some(s) = conn.spam.take() {
            let _ = self.events.send(Event::SpamEnded {
                sent: s.sent,
                elapsed: s.started.elapsed(),
            });
        }
    }

    /// Write raw bytes to the wire and record them in the ring/event stream.
    async fn write_and_record(
        &mut self,
        conn: &mut ConnState,
        bytes: &[u8],
        wire: Wire,
        tag: Option<String>,
    ) {
        if let Err(e) = conn.writer.write_bytes(bytes).await {
            let _ = self.events.send(Event::Error(format!("write failed: {e}")));
            return;
        }
        let seq = self.next_seq();
        let at = Instant::now();
        self.view.lock().unwrap().record(seq, Direction::Sent, &wire, at);
        let _ = self.events.send(Event::Sent { seq, wire, at, tag });
    }

    /// Close the socket, emit final stats, and mark the view disconnected.
    async fn teardown(&mut self, conn: &mut ConnState, reason: DisconnectReason) {
        self.stop_spam(conn);
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

        // Handshake reactions (from the profile) and automation reactions are
        // collected separately: profile sends are untagged, automation sends carry
        // an `auto: …` tag.
        let mut responses: Vec<NetworkMessage> = Vec::new();
        let mut auto_responses: Vec<(NetworkMessage, String)> = Vec::new();
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
                other => self.note_negotiation(conn, other),
            }
            // Automations react to any message (they no-op on handshake messages).
            auto_responses = self.automations.on_message(msg, conn.wtxidrelay);
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
        for (msg, tag) in auto_responses {
            self.send(conn, msg, Some(tag)).await;
        }
        self.sync_handshake(conn);
    }

    /// Record feature negotiation from the peer (for `status` and the getdata
    /// automation's witness upgrade).
    fn note_negotiation(&mut self, conn: &mut ConnState, msg: &NetworkMessage) {
        let mut v = self.view.lock().unwrap();
        match msg {
            NetworkMessage::WtxidRelay => {
                conn.wtxidrelay = true;
                v.peer.negotiated.wtxidrelay = true;
            }
            NetworkMessage::SendAddrV2 => v.peer.negotiated.addrv2 = true,
            NetworkMessage::SendHeaders => v.peer.negotiated.sendheaders = true,
            NetworkMessage::SendCmpct(s) => {
                v.peer.negotiated.sendcmpct = Some((s.send_compact, s.version))
            }
            NetworkMessage::FeeFilter(f) => v.peer.negotiated.feefilter = Some(*f),
            _ => {}
        }
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

/// Pace the next spam send: sleep for the interval, yield when unpaced, or never
/// resolve when not spamming.
async fn spam_wait(spam: &Option<SpamState>) {
    match spam {
        Some(s) => match s.interval {
            Some(d) => tokio::time::sleep(d).await,
            None => tokio::task::yield_now().await,
        },
        None => std::future::pending().await,
    }
}

/// Pace the next drip chunk, or never resolve when not dripping.
async fn drip_wait(drip: &Option<(usize, Duration)>, holding: bool) {
    match (drip, holding) {
        (Some((_, interval)), true) => tokio::time::sleep(*interval).await,
        _ => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spam(interval: Option<Duration>) -> SpamState {
        SpamState {
            msg: NetworkMessage::Ping(0),
            interval,
            remaining: None,
            sent: 0,
            started: Instant::now(),
            last_print: None,
            since_print: 0,
        }
    }

    #[test]
    fn unpaced_spam_prints_the_head_then_throttles() {
        let mut s = spam(None);
        let t0 = Instant::now();
        // The head prints in full, however fast the sends arrive.
        for i in 1..=SPAM_PRINT_HEAD {
            assert_eq!(s.note_send(t0), Some((i, 0)));
        }
        assert_eq!(s.note_send(t0 + Duration::from_millis(1)), None);
        assert_eq!(s.note_send(t0 + Duration::from_millis(400)), None);
        // Due again: the two sends in between went unprinted.
        let n = SPAM_PRINT_HEAD;
        assert_eq!(s.note_send(t0 + SPAM_PRINT_INTERVAL), Some((n + 3, 2)));
        assert_eq!(s.note_send(t0 + SPAM_PRINT_INTERVAL), None);
        assert_eq!(s.sent, n + 4, "every send is counted, printed or not");
    }

    #[test]
    fn short_burst_prints_every_send() {
        // `spam ping --rate 4/s --count 6`: faster than the throttle, but the
        // whole run fits in the head.
        let interval = Duration::from_millis(250);
        let mut s = spam(Some(interval));
        let t0 = Instant::now();
        for i in 1..=6u64 {
            assert_eq!(s.note_send(t0 + interval * i as u32), Some((i, 0)));
        }
    }

    #[test]
    fn slow_spam_prints_every_send() {
        let interval = Duration::from_secs(1);
        let mut s = spam(Some(interval));
        let t0 = Instant::now();
        for i in 1..=SPAM_PRINT_HEAD + 5 {
            assert_eq!(s.note_send(t0 + interval * i as u32), Some((i, 0)));
        }
    }
}
