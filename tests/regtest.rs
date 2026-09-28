//! Integration tests against a real regtest `bitcoind`, asserting via RPC
//! (PLAN.md §15). `#[ignore]` by default; run with:
//!
//! ```text
//! nix develop --command cargo test --test regtest -- --ignored
//! ```

mod common;

use std::time::Duration;

use common::{peer_info, poll, rpc, spawn_node, Harness};

use cli_ent::cli::Transport;
use cli_ent::misbehave::MangleKind;
use cli_ent::session::events::Command;

const READY: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn v1_handshake() {
    let node = spawn_node(false);
    let addr = node.p2p_addr.clone();
    let h = Harness::start(Transport::V1);
    h.connect(addr, Transport::V1);
    h.wait_ready(READY).await;

    // The node should now see our peer.
    poll(READY, || peer_info(&node).is_some()).await.unwrap();
    let peer = peer_info(&node).unwrap();
    assert_eq!(peer["transport_protocol_type"], "v1");
    assert_eq!(peer["version"], 70016);
    assert!(peer["subver"].as_str().unwrap().contains("cli-ent"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn v2_handshake() {
    let node = spawn_node(true);
    let addr = node.p2p_addr.clone();
    let h = Harness::start(Transport::V2);
    h.connect(addr, Transport::V2);
    h.wait_ready(READY).await;

    poll(READY, || peer_info(&node).is_some()).await.unwrap();
    let peer = peer_info(&node).unwrap();
    assert_eq!(peer["transport_protocol_type"], "v2");
    // A v2 session id is present on both sides.
    assert!(h.view.lock().unwrap().peer.v2_session_id.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn auto_falls_back_to_v1() {
    let node = spawn_node(false); // v2transport=0
    let addr = node.p2p_addr.clone();
    let h = Harness::start(Transport::Auto);
    h.connect(addr, Transport::Auto);
    h.wait_ready(READY).await;

    let fell_back = h.view.lock().unwrap().peer.fell_back.is_some();
    assert!(fell_back, "auto should have fallen back to v1");
    let peer = peer_info(&node).unwrap();
    assert_eq!(peer["transport_protocol_type"], "v1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn bad_magic_disconnects() {
    let node = spawn_node(false);
    let addr = node.p2p_addr.clone();
    let h = Harness::start(Transport::V1);
    h.connect(addr, Transport::V1);
    h.wait_ready(READY).await;
    assert!(peer_info(&node).is_some());

    // A bad-magic frame makes Core drop us.
    h.send(Command::Mangle {
        msg: bitcoin::p2p::message::NetworkMessage::Ping(1),
        kind: MangleKind::BadMagic,
    });
    poll(READY, || peer_info(&node).is_none())
        .await
        .expect("peer should have been disconnected after bad magic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn spam_ping_stays_connected() {
    let node = spawn_node(true);
    let addr = node.p2p_addr.clone();
    let h = Harness::start(Transport::V2);
    h.connect(addr, Transport::V2);
    h.wait_ready(READY).await;

    // The Core handshake profile sends a ping of its own, so count the delta.
    let pings_before = pings_out(&h);
    h.send(Command::Spam {
        msg: bitcoin::p2p::message::NetworkMessage::Ping(0),
        rate: None,
        count: Some(200),
    });
    // Give the burst time to land.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let peer = peer_info(&node).expect("peer still connected after a bounded ping burst");
    let pings = peer["bytesrecv_per_msg"]["ping"].as_u64().unwrap_or(0);
    assert!(pings > 0, "node should have received pings");
    // Sanity: RPC still responsive.
    let _ = rpc(&node, "getblockcount");

    // The burst is visible without flooding: a sample of the sends is printed,
    // every one of them is counted, and the run reports its total.
    let lines = h.lines.lock().unwrap().clone();
    let shown: Vec<&String> = lines.iter().filter(|l| l.contains("spam #")).collect();
    assert!(!shown.is_empty(), "spam printed nothing: {lines:#?}");
    assert!(
        shown.len() < 20,
        "an unpaced 200-message burst should be throttled, printed {}",
        shown.len()
    );
    assert!(
        lines.iter().any(|l| l.contains("spam ended: 200 sent")),
        "missing the spam summary: {lines:#?}"
    );
    assert_eq!(
        pings_out(&h) - pings_before,
        200,
        "every spam send should reach the traffic counters"
    );
}

/// Pings our session has sent, per its own traffic counters.
fn pings_out(h: &Harness) -> u64 {
    h.view.lock().unwrap().stats.per_msg_out.get("ping").copied().unwrap_or(0)
}
