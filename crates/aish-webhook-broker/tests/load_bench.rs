//! TASK-371 — non-gating throughput / delivery-latency benchmark.
//!
//! `#[ignore]`d so it never runs in CI or the default `cargo test` gate. Run it
//! by hand (release build for representative numbers):
//!
//! ```text
//! cargo test --release --manifest-path crates/aish-webhook-broker/Cargo.toml \
//!     --test load_bench -- --ignored --nocapture
//! # knobs: BENCH_N (default 10000 webhooks), BENCH_CONC (default 16 senders),
//! #        BENCH_RATE (default 1000/s, open-loop paced; 0 = unpaced/saturation)
//! ```
//!
//! Setup: the real router on a loopback TCP socket over a temp-file SQLite DB
//! (WAL), one registered (tenant, plugin) with an HMAC secret, and ONE
//! authenticated WebSocket subscriber that acks every webhook. `BENCH_CONC`
//! keep-alive HTTP/1.1 connections POST `BENCH_N` signed webhooks, paced on an
//! open-loop schedule (webhook `seq` is due at `start + seq / BENCH_RATE`) so the
//! default run measures latency AT the TASK-371 target rate (1000/s) rather than
//! queueing delay at saturation; `BENCH_RATE=0` sends as fast as the broker answers. Each payload carries a sequence number; delivery latency is
//! measured per webhook from "request write started" to "envelope read on the
//! WS". Throughput is N / (first send → last WS receipt).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aish_webhook_broker::config::BrokerConfig;
use aish_webhook_broker::dispatcher::Hub;
use aish_webhook_broker::{db, http, signature};

use aish_webhook_client::transport::TungsteniteTransport;
use aish_webhook_client::{Transport, WsMessage};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const TENANT: &str = "t-bench";
const PLUGIN: &str = "github";
const SECRET: &str = "bench-secret";

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Minimal HTTP/1.1 keep-alive POST; returns the status code.
async fn http_post(
    conn: &mut BufReader<TcpStream>,
    path: &str,
    headers: &[(&str, String)],
    body: &str,
) -> u16 {
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nhost: bench\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    conn.get_mut().write_all(req.as_bytes()).await.unwrap();

    // Read the head, then exactly content-length bytes of body.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        conn.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    conn.read_exact(&mut body).await.unwrap();
    status
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load benchmark — run manually with --ignored --nocapture"]
async fn throughput_and_delivery_latency() {
    let n = env_usize("BENCH_N", 10_000);
    let conc = env_usize("BENCH_CONC", 16).max(1);
    let rate = env_usize("BENCH_RATE", 1000);

    // Broker on loopback.
    let dir = tempfile::tempdir().unwrap();
    let pool = db::init(dir.path().join("broker.db").to_str().unwrap()).unwrap();
    let config = BrokerConfig {
        db: pool.clone(),
        hub: Arc::new(Hub::new()),
        start_time: Instant::now(),
        max_queue_size: n + 1, // never drop during the run
        ws_heartbeat_secs: 30,
        poll_timeout_secs: 30,
        msg_ttl_secs: 604_800,
    };
    let router = http::router(config.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    // Register + authenticate one WS subscriber.
    let session = db::register_client(&pool, TENANT, PLUGIN, "bench", "websocket", Some(SECRET))
        .unwrap()
        .session_token;
    let mut ws = TungsteniteTransport::connect(&format!("ws://{addr}/ws"))
        .await
        .unwrap();
    ws.send(WsMessage::Text(
        json!({"type":"auth","session_token":session}).to_string(),
    ))
    .await
    .unwrap();
    match ws.recv().await.unwrap() {
        Some(WsMessage::Text(t)) => assert!(t.contains("auth_ok"), "{t}"),
        other => panic!("expected auth_ok, got {other:?}"),
    }
    while config.hub.connected_count() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let sent_at: Arc<Mutex<Vec<Option<Instant>>>> = Arc::new(Mutex::new(vec![None; n]));
    let start = Instant::now();

    // Subscriber: record receipt time per seq, ack each.
    let reader = tokio::spawn(async move {
        let mut recv_at: Vec<Option<Instant>> = vec![None; n];
        let mut got = 0usize;
        while got < n {
            match ws.recv().await.unwrap() {
                Some(WsMessage::Text(t)) => {
                    let now = Instant::now();
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["type"] != "webhook" {
                        continue;
                    }
                    let seq = v["payload"]["seq"].as_u64().unwrap() as usize;
                    if recv_at[seq].is_none() {
                        recv_at[seq] = Some(now);
                        got += 1;
                    }
                    let ack = json!({"type":"ack","webhook_id":v["id"]}).to_string();
                    ws.send(WsMessage::Text(ack)).await.unwrap();
                }
                Some(_) => continue,
                None => panic!("ws closed after {got}/{n}"),
            }
        }
        recv_at
    });

    // Senders: `conc` keep-alive connections, seqs striped across them.
    let mut senders = Vec::new();
    for w in 0..conc {
        let sent_at = sent_at.clone();
        senders.push(tokio::spawn(async move {
            let mut conn = BufReader::new(TcpStream::connect(addr).await.unwrap());
            conn.get_ref().set_nodelay(true).unwrap();
            let path = format!("/webhooks/{TENANT}/{PLUGIN}");
            for seq in (w..n).step_by(conc) {
                if rate > 0 {
                    let due = start + Duration::from_secs_f64(seq as f64 / rate as f64);
                    tokio::time::sleep_until(due.into()).await;
                }
                let body = json!({"seq": seq, "ref": "refs/heads/main"}).to_string();
                let sig = format!("sha256={}", signature::sign(body.as_bytes(), SECRET));
                sent_at.lock().unwrap()[seq] = Some(Instant::now());
                let status = http_post(
                    &mut conn,
                    &path,
                    &[("x-event-type", "push".into()), ("x-signature", sig)],
                    &body,
                )
                .await;
                assert_eq!(status, 202, "seq {seq}");
            }
        }));
    }
    for s in senders {
        s.await.unwrap();
    }
    let ingest_elapsed = start.elapsed();
    let recv_at = tokio::time::timeout(Duration::from_secs(120), reader)
        .await
        .expect("all webhooks delivered")
        .unwrap();
    let total_elapsed = recv_at
        .iter()
        .flatten()
        .max()
        .unwrap()
        .duration_since(start);

    let sent_at = sent_at.lock().unwrap();
    let mut lat: Vec<Duration> = (0..n)
        .map(|i| recv_at[i].unwrap().duration_since(sent_at[i].unwrap()))
        .collect();
    lat.sort();
    let mean = lat.iter().sum::<Duration>() / n as u32;

    let ingest_rps = n as f64 / ingest_elapsed.as_secs_f64();
    let e2e_rps = n as f64 / total_elapsed.as_secs_f64();
    println!(
        "\nBENCH broker: n={n} conc={conc} rate={}\n  \
         ingest (HTTP 202): {ingest_rps:.0} webhooks/s over {ingest_elapsed:.2?}\n  \
         end-to-end (WS):   {e2e_rps:.0} webhooks/s over {total_elapsed:.2?}\n  \
         delivery latency:  p50={:.2?} p90={:.2?} p99={:.2?} max={:.2?} mean={mean:.2?}",
        if rate > 0 {
            format!("{rate}/s paced")
        } else {
            "unpaced".to_string()
        },
        pct(&lat, 0.50),
        pct(&lat, 0.90),
        pct(&lat, 0.99),
        lat[n - 1],
    );

    // Sanity only (non-gating): everything arrived; acks drain the queue.
    let deadline = Instant::now() + Duration::from_secs(30);
    while db::count_pending(&pool, TENANT, PLUGIN).unwrap() > 0 {
        assert!(Instant::now() < deadline, "acks did not drain the queue");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
