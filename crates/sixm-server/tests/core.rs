//! Server core behaviour over real sockets, and the re-entrancy guarantees the
//! scripting layer relies on.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sixm_protocol::{
    decode_snapshot, encode_state, ClientResource, ClientScript, EntityState,
    MAX_EVENT_NAME_LENGTH, PROTOCOL_VERSION,
};
use sixm_server::{
    ConnectDecision, ConnectInfo, Logger, PlayerInfo, Server, ServerHandler, ServerOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::tungstenite::{self, Message};

// --------------------------------------------------------------------- harness

/// Run a test body on a single-threaded runtime inside a `LocalSet`, like the real server.
fn run(test: impl Future<Output = ()>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async {
        timeout(Duration::from_secs(30), test)
            .await
            .expect("test timed out");
    });
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Poll until `probe` yields a value; panics after two seconds.
async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(ms(2)).await;
    }
}

fn options() -> ServerOptions {
    ServerOptions {
        port: 0,
        host: "127.0.0.1".to_string(),
        logger: Logger::silent(),
        ..ServerOptions::default()
    }
}

/// Start a server on an ephemeral loopback port.
async fn start(options: ServerOptions) -> (Server, u16) {
    let server = Server::new(options).unwrap();
    let port = server.start().await.unwrap();
    (server, port)
}

type Hook<F> = RefCell<Option<Box<F>>>;
type ConnectingHook = dyn Fn(&ConnectInfo) -> ConnectDecision;
type JoinedHook = dyn Fn(&PlayerInfo);
type DroppedHook = dyn Fn(&PlayerInfo, &str);
type EventHook = dyn Fn(u16, &str, &[Value]);
type TickHook = dyn Fn(u64);

/// A handler that records every callback and optionally runs a closure for it.
#[derive(Default)]
struct Recorder {
    connecting: RefCell<Vec<ConnectInfo>>,
    joined: RefCell<Vec<PlayerInfo>>,
    dropped: RefCell<Vec<(u16, String, String)>>,
    events: RefCell<Vec<(u16, String, Vec<Value>)>>,
    ticks: RefCell<Vec<(u64, f64)>>,
    on_connecting: Hook<ConnectingHook>,
    on_joined: Hook<JoinedHook>,
    on_dropped: Hook<DroppedHook>,
    on_event: Hook<EventHook>,
    on_tick: Hook<TickHook>,
}

impl Recorder {
    fn install(server: &Server) -> Rc<Recorder> {
        let recorder = Rc::new(Recorder::default());
        server.set_handler(recorder.clone());
        recorder
    }

    fn event_names(&self) -> Vec<String> {
        self.events
            .borrow()
            .iter()
            .map(|(_, name, _)| name.clone())
            .collect()
    }

    fn drop_reasons(&self) -> Vec<String> {
        self.dropped
            .borrow()
            .iter()
            .map(|(_, _, reason)| reason.clone())
            .collect()
    }
}

impl ServerHandler for Recorder {
    fn player_connecting(&self, info: &ConnectInfo) -> ConnectDecision {
        self.connecting.borrow_mut().push(info.clone());
        match &*self.on_connecting.borrow() {
            Some(hook) => hook(info),
            None => ConnectDecision::Accept,
        }
    }

    fn player_joined(&self, player: &PlayerInfo) {
        self.joined.borrow_mut().push(player.clone());
        if let Some(hook) = &*self.on_joined.borrow() {
            hook(player);
        }
    }

    fn player_dropped(&self, player: &PlayerInfo, reason: &str) {
        self.dropped
            .borrow_mut()
            .push((player.id, player.name.clone(), reason.to_string()));
        if let Some(hook) = &*self.on_dropped.borrow() {
            hook(player, reason);
        }
    }

    fn net_event(&self, player: u16, name: &str, args: Vec<Value>) {
        self.events
            .borrow_mut()
            .push((player, name.to_string(), args.clone()));
        if let Some(hook) = &*self.on_event.borrow() {
            hook(player, name, &args);
        }
    }

    fn tick(&self, tick: u64, dt_ms: f64) {
        self.ticks.borrow_mut().push((tick, dt_ms));
        if let Some(hook) = &*self.on_tick.borrow() {
            hook(tick);
        }
    }
}

enum Command {
    Send(Message),
    /// Stop reading (and therefore stop answering pings).
    Pause,
    Resume,
    /// Drop the socket without a close handshake.
    Terminate,
}

#[derive(Default)]
struct Received {
    msgs: RefCell<Vec<Value>>,
    bins: RefCell<Vec<Vec<u8>>>,
    closed: Cell<bool>,
    close_code: Cell<Option<u16>>,
}

/// A raw protocol client that records everything it receives.
struct Client {
    commands: mpsc::UnboundedSender<Command>,
    received: Rc<Received>,
    id: u16,
    welcome: Value,
}

type Snapshot = (u32, Vec<(u16, EntityState)>);

impl Client {
    async fn connect(port: u16) -> Client {
        Client::connect_to(port, "/").await
    }

    async fn connect_to(port: u16, path: &str) -> Client {
        Client::connect_with(port, path, &[]).await
    }

    /// A client whose upgrade request carries extra headers, the way a proxy forwards one.
    async fn connect_with(port: u16, path: &str, headers: &[(&str, &str)]) -> Client {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut request = format!("ws://127.0.0.1:{port}{path}").into_client_request().unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let (mut ws, _) = tokio_tungstenite::client_async(request, stream).await.unwrap();
        let (commands, mut rx) = mpsc::unbounded_channel();
        let received = Rc::new(Received::default());
        let seen = received.clone();
        tokio::task::spawn_local(async move {
            let mut paused = false;
            loop {
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(Command::Send(message)) => {
                            if ws.send(message).await.is_err() {
                                break;
                            }
                        }
                        Some(Command::Pause) => paused = true,
                        Some(Command::Resume) => paused = false,
                        Some(Command::Terminate) | None => break,
                    },
                    frame = ws.next(), if !paused => match frame {
                        Some(Ok(Message::Text(text))) => seen.msgs.borrow_mut().push(serde_json::from_str(&text).unwrap()),
                        Some(Ok(Message::Binary(data))) => seen.bins.borrow_mut().push(data.to_vec()),
                        Some(Ok(Message::Close(frame))) => seen.close_code.set(frame.map(|f| u16::from(f.code))),
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    },
                }
            }
            seen.closed.set(true);
        });
        Client {
            commands,
            received,
            id: 0,
            welcome: Value::Null,
        }
    }

    /// Connect, say hello and wait for the WELCOME.
    async fn join(port: u16, name: &str) -> Client {
        Client::join_with(port, name, &[]).await
    }

    async fn join_with(port: u16, name: &str, headers: &[(&str, &str)]) -> Client {
        let mut client = Client::connect_with(port, "/", headers).await;
        client.hello(name);
        client.welcome = client.next("welcome").await;
        client.id = client.welcome["id"].as_u64().unwrap() as u16;
        client
    }

    fn command(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    fn send_text(&self, text: impl Into<String>) {
        self.command(Command::Send(Message::text(text.into())));
    }

    fn send(&self, message: Value) {
        self.send_text(message.to_string());
    }

    fn send_binary(&self, data: Vec<u8>) {
        self.command(Command::Send(Message::binary(data)));
    }

    fn hello(&self, name: &str) {
        self.send(
            json!({ "t": "hello", "proto": PROTOCOL_VERSION, "name": name, "build": "test" }),
        );
    }

    fn event(&self, name: &str, args: Value) {
        self.send(json!({ "t": "event", "name": name, "args": args }));
    }

    fn state(&self, seq: u32, state: EntityState) {
        self.send_binary(encode_state(seq, &state));
    }

    /// Start a clean WebSocket close.
    fn close(&self) {
        self.command(Command::Send(Message::Close(None)));
    }

    /// Wait for (and consume) the next control message of a type.
    async fn next(&self, kind: &str) -> Value {
        until(&format!("\"{kind}\" message"), || {
            let mut msgs = self.received.msgs.borrow_mut();
            let at = msgs.iter().position(|m| m["t"] == kind)?;
            Some(msgs.remove(at))
        })
        .await
    }

    fn has(&self, kind: &str) -> bool {
        self.received.msgs.borrow().iter().any(|m| m["t"] == kind)
    }

    fn bin_count(&self) -> usize {
        self.received.bins.borrow().len()
    }

    /// Every binary frame received so far, which must all be valid snapshots.
    fn snapshots(&self) -> Vec<Snapshot> {
        self.received
            .bins
            .borrow()
            .iter()
            .map(|b| decode_snapshot(b).expect("every binary frame decodes"))
            .collect()
    }

    /// Wait for a snapshot holding exactly `count` entities.
    async fn snapshot_with(&self, count: usize) -> Snapshot {
        until(&format!("a snapshot of {count}"), || {
            self.snapshots().into_iter().find(|(_, e)| e.len() == count)
        })
        .await
    }

    async fn until_closed(&self) {
        until("socket close", || self.received.closed.get().then_some(())).await;
    }
}

/// Connect, optionally send a first message, expect a REJECT followed by the socket closing.
async fn expect_reject(port: u16, first: Option<Value>) -> String {
    let client = Client::connect(port).await;
    if let Some(message) = first {
        client.send(message);
    }
    let reject = client.next("reject").await;
    client.until_closed().await;
    assert_eq!(client.received.close_code.get(), Some(4001));
    reject["reason"].as_str().unwrap().to_string()
}

fn at(x: f32, y: f32, z: f32) -> EntityState {
    EntityState {
        x,
        y,
        z,
        ..EntityState::default()
    }
}

struct HttpReply {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl HttpReply {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", String::as_str)
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// A one-shot HTTP/1.1 request; header names in the reply are lower-cased.
async fn http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> std::io::Result<HttpReply> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(body) = body {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    request.push_str(body.unwrap_or(""));
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let raw = String::from_utf8(raw).unwrap();
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .expect("a complete HTTP response");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.to_lowercase(), value.trim().to_string())
        })
        .collect();
    Ok(HttpReply {
        status,
        headers,
        body: body.to_string(),
    })
}

async fn get(port: u16, path: &str) -> HttpReply {
    http(port, "GET", path, None).await.unwrap()
}

// ------------------------------------------------------------------- handshake

#[test]
fn handshake_welcome_carries_id_server_info_existing_players_and_resources() {
    run(async {
        let (server, port) = start(ServerOptions {
            name: "Test Box".to_string(),
            max_players: 8,
            tick_rate: 30,
            ..options()
        })
        .await;
        let hud = ClientResource {
            name: "hud".to_string(),
            scripts: vec![ClientScript {
                file: "client.lua".to_string(),
                code: "print('hi')".to_string(),
            }],
        };
        server.set_client_resources(vec![hud]);
        let recorder = Recorder::install(&server);
        assert_eq!(
            (
                server.name().as_str(),
                server.max_players(),
                server.tick_rate()
            ),
            ("Test Box", 8, 30)
        );

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let a = Client::join(port, "  Alice\u{7}  ").await;
        assert_eq!(
            a.welcome,
            json!({
                "t": "welcome", "id": 1, "name": "Alice", "serverName": "Test Box", "tickRate": 30, "maxPlayers": 8,
                "players": [],
                "resources": [{ "name": "hud", "scripts": [{ "file": "client.lua", "code": "print('hi')" }] }],
            })
        );

        let joined = recorder.joined.borrow()[0].clone();
        assert_eq!((joined.id, joined.name.as_str()), (1, "Alice"));
        let (address, client_port) = joined.endpoint.rsplit_once(':').unwrap();
        assert_eq!(address, "127.0.0.1");
        assert!(client_port.parse::<u16>().is_ok());
        assert_eq!(joined.identifiers, ["ip:127.0.0.1"]);
        assert_eq!(joined.state, EntityState::default());
        assert!(!joined.has_state);
        assert!(joined.joined_at_ms >= before && joined.joined_at_ms <= before + 5000);
        assert_eq!(server.player_count(), 1);
        assert_eq!(server.player(1).unwrap().endpoint, joined.endpoint);

        let b = Client::join(port, "Bob").await;
        assert_eq!(b.id, 2);
        assert_eq!(b.welcome["players"], json!([{ "id": 1, "name": "Alice" }]));
        assert_eq!(server.player_ids(), [1, 2]);

        // The ws-level ping sent at join yields a measured round trip.
        sleep(ms(50)).await;
        let ping = server.player(1).unwrap().ping;
        assert!((0..1000).contains(&ping));
    });
}

#[test]
fn handshake_ws_path_works_and_other_upgrade_paths_are_refused() {
    run(async {
        let (_server, port) = start(options()).await;
        let via_ws = Client::connect_to(port, "/ws").await;
        via_ws.send(json!({ "t": "hello", "proto": PROTOCOL_VERSION, "name": "P" }));
        let welcome = via_ws.next("welcome").await;
        assert_eq!(welcome["resources"], json!([]));

        // Any other upgrade path is refused before a WebSocket exists.
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let refused =
            tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/nope"), stream).await;
        match refused {
            Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 404),
            other => panic!("expected a 404, got {:?}", other.map(|_| ())),
        }
    });
}

#[test]
fn handshake_rejects_wrong_protocol_empty_name_and_non_hello_first_frame() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);

        let proto = expect_reject(
            port,
            Some(json!({ "t": "hello", "proto": PROTOCOL_VERSION + 1, "name": "X" })),
        )
        .await;
        assert_eq!(proto, "protocol version mismatch (server 1, client 2)");
        let proto = expect_reject(
            port,
            Some(json!({ "t": "hello", "proto": "1", "name": "X" })),
        )
        .await;
        assert_eq!(
            proto,
            "protocol version mismatch (server 1, client unknown)"
        );

        let empty = expect_reject(
            port,
            Some(json!({ "t": "hello", "proto": PROTOCOL_VERSION, "name": " \u{0}\t " })),
        )
        .await;
        assert_eq!(empty, "invalid player name");
        let missing = expect_reject(
            port,
            Some(json!({ "t": "hello", "proto": PROTOCOL_VERSION })),
        )
        .await;
        assert_eq!(missing, "invalid player name");

        let not_hello =
            expect_reject(port, Some(json!({ "t": "event", "name": "x", "args": [] }))).await;
        assert_eq!(not_hello, "expected a hello message");

        let binary_first = Client::connect(port).await;
        binary_first.state(1, EntityState::default());
        assert_eq!(
            binary_first.next("reject").await["reason"],
            "expected a hello message"
        );

        assert!(
            recorder.connecting.borrow().is_empty(),
            "player_connecting must not fire for invalid handshakes"
        );
        assert_eq!(server.player_count(), 0);
    });
}

#[test]
fn handshake_rejects_when_the_server_is_full() {
    run(async {
        let (server, port) = start(ServerOptions {
            max_players: 1,
            ..options()
        })
        .await;
        let _only = Client::join(port, "Only").await;
        let reason = expect_reject(
            port,
            Some(json!({ "t": "hello", "proto": PROTOCOL_VERSION, "name": "Late" })),
        )
        .await;
        assert_eq!(reason, "server is full");
        assert_eq!(server.player_count(), 1);
    });
}

#[test]
fn handshake_no_hello_within_the_timeout_is_rejected() {
    run(async {
        let (_server, port) = start(ServerOptions {
            hello_timeout_ms: 60,
            ..options()
        })
        .await;
        let started = Instant::now();
        let reason = expect_reject(port, None).await;
        assert_eq!(reason, "handshake timed out");
        assert!(started.elapsed() >= ms(50));
    });
}

#[test]
fn pending_connections_are_capped() {
    run(async {
        let (server, port) = start(ServerOptions {
            max_pending_connections: Some(2),
            ..options()
        })
        .await;
        // Joined players do not count against the cap.
        let joined = Client::join(port, "A").await;
        assert_eq!(joined.id, 1);
        let first = Client::connect(port).await;
        let _second = Client::connect(port).await;
        let refused = || async {
            let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            match tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/"), stream).await {
                Err(tungstenite::Error::Http(response)) => Some(response.status().as_u16()),
                _ => None,
            }
        };
        assert_eq!(refused().await, Some(503));
        assert_eq!(
            get(port, "/dynamic.json").await.status,
            200,
            "plain HTTP is still served"
        );

        // A pending connection that goes away frees its place.
        first.command(Command::Terminate);
        let mut retry = None;
        for _ in 0..200 {
            let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            if let Ok((ws, _)) =
                tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/"), stream).await
            {
                retry = Some(ws);
                break;
            }
            sleep(ms(5)).await;
        }
        assert!(retry.is_some());
        assert_eq!(server.player_count(), 1);
    });
}

// --------------------------------------------------------------------- players

#[test]
fn player_ids_lowest_free_id_is_reused() {
    run(async {
        let (server, port) = start(options()).await;
        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;
        let c = Client::join(port, "C").await;
        assert_eq!([a.id, b.id, c.id], [1, 2, 3]);

        b.close();
        until("player 2 to drop", || {
            server.player(2).is_none().then_some(())
        })
        .await;
        let d = Client::join(port, "D").await;
        assert_eq!(d.id, 2);
        let e = Client::join(port, "E").await;
        assert_eq!(e.id, 4);
    });
}

#[test]
fn join_and_leave_are_broadcast_to_the_other_players() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);

        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;
        assert_eq!(
            a.next("join").await,
            json!({ "t": "join", "id": 2, "name": "B" })
        );
        assert!(!b.has("join"), "no JOIN for yourself");

        b.close();
        assert_eq!(
            a.next("leave").await,
            json!({ "t": "leave", "id": 2, "reason": "disconnected" })
        );
        assert_eq!(
            *recorder.dropped.borrow(),
            [(2, "B".to_string(), "disconnected".to_string())]
        );
        assert!(server.player(2).is_none());
        b.until_closed().await;

        // A socket that just vanishes is noticed too.
        let c = Client::join(port, "C").await;
        assert_eq!(c.id, 2);
        c.command(Command::Terminate);
        assert_eq!(
            a.next("leave").await,
            json!({ "t": "leave", "id": 2, "reason": "disconnected" })
        );
    });
}

#[test]
fn net_event_is_delivered_for_client_events_and_args_default_to_empty() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);

        let a = Client::join(port, "A").await;
        a.event("chat:say", json!(["hi", 2, { "k": true }]));
        a.send(json!({ "t": "event", "name": "noargs" }));
        until("two net events", || {
            (recorder.events.borrow().len() == 2).then_some(())
        })
        .await;
        assert_eq!(
            *recorder.events.borrow(),
            [
                (
                    1,
                    "chat:say".to_string(),
                    vec![json!("hi"), json!(2), json!({ "k": true })]
                ),
                (1, "noargs".to_string(), vec![]),
            ]
        );
    });
}

#[test]
fn emit_net_delivers_to_one_player_or_to_everyone() {
    run(async {
        let (server, port) = start(options()).await;
        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;

        assert!(server.emit_net(i32::from(a.id), "only:a", vec![json!(1)]));
        assert!(server.emit_net(-1, "all", vec![json!("x")]));
        assert!(!server.emit_net(99, "nobody", vec![]));
        assert!(!server.emit_net(-2, "nobody", vec![]));
        assert!(!server.emit_net(70_000, "nobody", vec![]));
        assert!(!server.emit_net(-1, "", vec![]));
        assert!(!server.emit_net(-1, &"x".repeat(MAX_EVENT_NAME_LENGTH + 1), vec![]));

        assert_eq!(
            a.next("event").await,
            json!({ "t": "event", "name": "only:a", "args": [1], "src": 0 })
        );
        assert_eq!(
            a.next("event").await,
            json!({ "t": "event", "name": "all", "args": ["x"], "src": 0 })
        );
        assert_eq!(
            b.next("event").await,
            json!({ "t": "event", "name": "all", "args": ["x"], "src": 0 })
        );
        assert!(!b.has("event"));
    });
}

#[test]
fn client_ping_is_answered_with_pong() {
    run(async {
        let (_server, port) = start(options()).await;
        let a = Client::join(port, "A").await;
        a.send(json!({ "t": "ping", "ts": 12345 }));
        assert_eq!(a.next("pong").await, json!({ "t": "pong", "ts": 12345 }));
        a.send_text(r#"{"t":"ping","ts":1795597672181.25}"#);
        assert_eq!(a.next("pong").await["ts"], json!(1795597672181.25));
    });
}

// ------------------------------------------------------------------ state sync

#[test]
fn state_sync_each_client_receives_the_other_in_snapshots_never_itself() {
    run(async {
        let (server, port) = start(ServerOptions {
            tick_rate: 50,
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);

        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;
        let state_a = EntityState {
            heading: 90.0,
            model: 0xdead_beef,
            flags: 5,
            ..at(1.0, 2.0, 3.0)
        };
        a.state(1, state_a);
        b.state(1, at(10.0, 20.0, 30.0));

        let (tick_a, for_a) = a.snapshot_with(1).await;
        let (_, for_b) = b.snapshot_with(1).await;
        assert_eq!(for_a, [(b.id, at(10.0, 20.0, 30.0))]);
        assert_eq!(for_b, [(a.id, state_a)]);
        assert!(tick_a >= 1);
        assert!(!a
            .snapshots()
            .iter()
            .any(|(_, entities)| entities.iter().any(|(id, _)| *id == a.id)));
        assert_eq!(server.player(a.id).unwrap().state, state_a);
        assert!(server.player(a.id).unwrap().has_state);

        let ticks = recorder.ticks.borrow();
        assert_eq!(ticks[0].0, 1);
        assert!(ticks[0].1 > 0.0);
        assert_eq!(server.tick(), ticks.last().unwrap().0);
    });
}

#[test]
fn state_sync_players_that_never_sent_a_state_are_not_included() {
    run(async {
        let (_server, port) = start(ServerOptions {
            tick_rate: 100,
            stream_distance: 0.0,
            ..options()
        })
        .await;
        let a = Client::join(port, "A").await;
        let _silent = Client::join(port, "Silent").await;
        a.state(1, at(1.0, 0.0, 0.0));
        sleep(ms(80)).await;
        assert_eq!(a.bin_count(), 0, "nothing to report: no snapshot at all");
    });
}

#[test]
fn tick_loop_keeps_pace_with_the_configured_rate() {
    run(async {
        let (server, _port) = start(ServerOptions {
            tick_rate: 100,
            ..options()
        })
        .await;
        let before = server.tick();
        sleep(ms(300)).await;
        let ran = server.tick() - before;
        assert!(
            (20..=36).contains(&ran),
            "expected about 30 ticks in 300 ms, got {ran}"
        );
    });
}

#[test]
fn stream_distance_culls_far_players_and_sends_exactly_one_empty_snapshot_on_leaving_range() {
    run(async {
        let (_server, port) = start(ServerOptions {
            tick_rate: 100,
            stream_distance: 100.0,
            ..options()
        })
        .await;
        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;
        a.state(1, at(0.0, 0.0, 0.0));
        b.state(1, at(60.0, 0.0, 60.0)); // ~85 away: in range
        a.snapshot_with(1).await;
        b.snapshot_with(1).await;

        b.state(2, at(60.0, 60.0, 60.0)); // ~104 away in 3D: out of range
        a.snapshot_with(0).await;
        b.snapshot_with(0).await;
        sleep(ms(120)).await; // a dozen more ticks

        for client in [&a, &b] {
            let counts: Vec<usize> = client.snapshots().iter().map(|(_, e)| e.len()).collect();
            assert_eq!(
                counts.iter().filter(|n| **n == 0).count(),
                1,
                "exactly one empty snapshot"
            );
            assert_eq!(counts.last(), Some(&0), "and nothing after it");
        }

        let seen = a.bin_count();
        b.state(3, at(10.0, 0.0, 0.0)); // back in range
        until("snapshots resume", || (a.bin_count() > seen).then_some(())).await;
        assert_eq!(a.snapshots().last().unwrap().1[0].0, b.id);
    });
}

#[test]
fn a_lone_player_gets_one_empty_snapshot_when_the_last_neighbour_leaves() {
    run(async {
        let (server, port) = start(ServerOptions {
            tick_rate: 100,
            ..options()
        })
        .await;
        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;
        a.state(1, at(0.0, 0.0, 0.0));
        b.state(1, at(1.0, 0.0, 0.0));
        a.snapshot_with(1).await;

        b.close();
        until("B to drop", || (server.player_count() == 1).then_some(())).await;
        a.snapshot_with(0).await;
        sleep(ms(60)).await;
        let counts: Vec<usize> = a.snapshots().iter().map(|(_, e)| e.len()).collect();
        assert_eq!(counts.iter().filter(|n| **n == 0).count(), 1);
        assert_eq!(counts.last(), Some(&0));
    });
}

#[test]
fn stream_distance_zero_or_less_means_unlimited() {
    run(async {
        let (_server, port) = start(ServerOptions {
            tick_rate: 100,
            stream_distance: 0.0,
            ..options()
        })
        .await;
        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;
        a.state(1, at(0.0, 0.0, 0.0));
        b.state(1, at(1e6, -1e6, 1e6));
        let (_, entities) = a.snapshot_with(1).await;
        assert_eq!(entities[0].0, b.id);
    });
}

#[test]
fn stale_and_duplicate_seq_are_ignored_and_uint32_wraparound_is_accepted() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        let a = Client::join(port, "A").await;
        // Frames arrive in order, so once the marker event is seen the states before it were handled.
        let sync = |label: &'static str| {
            a.event(label, json!([]));
            let recorder = recorder.clone();
            async move {
                until(label, || {
                    recorder
                        .event_names()
                        .iter()
                        .any(|n| n == label)
                        .then_some(())
                })
                .await
            }
        };
        let x = || server.player(a.id).unwrap().state.x;

        a.state(5, at(1.0, 0.0, 0.0));
        a.state(3, at(99.0, 0.0, 0.0)); // older
        a.state(5, at(77.0, 0.0, 0.0)); // duplicate
        sync("m1").await;
        assert_eq!(x(), 1.0);

        a.state(0xffff_fff0, at(2.0, 0.0, 0.0)); // more than half the range "ahead" == behind
        sync("m2").await;
        assert_eq!(x(), 1.0);

        a.state(0x7fff_ffff, at(3.0, 0.0, 0.0));
        a.state(0xffff_fffe, at(4.0, 0.0, 0.0));
        sync("m3").await;
        assert_eq!(x(), 4.0);

        a.state(2, at(5.0, 0.0, 0.0)); // wrapped past 0xFFFFFFFF
        sync("m4").await;
        assert_eq!(x(), 5.0);
    });
}

// ------------------------------------------------------------- hostile clients

#[test]
fn malformed_frames_do_not_crash_the_server_and_eventually_kick() {
    run(async {
        let (server, port) = start(ServerOptions {
            max_strikes: 4,
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);

        let a = Client::join(port, "A").await;
        let witness = Client::join(port, "W").await;

        a.send_text("this is not json");
        a.send_binary(vec![1, 2, 3]); // wrong-size binary
        a.state(1, at(f32::NAN, 0.0, 0.0)); // non-finite state
        a.event("still-here", json!([]));
        until("event after three strikes", || {
            recorder
                .event_names()
                .contains(&"still-here".to_string())
                .then_some(())
        })
        .await;
        let player = server.player(a.id).expect("three strikes are tolerated");
        assert!(!player.has_state, "the NaN state was not applied");

        a.event(&"x".repeat(200), json!([])); // event name too long: 4th strike
        assert_eq!(
            a.next("kick").await["reason"],
            "too many malformed messages"
        );
        a.until_closed().await;
        assert_eq!(a.received.close_code.get(), Some(4002));
        assert_eq!(recorder.drop_reasons(), ["too many malformed messages"]);
        assert!(server.player(a.id).is_none());

        // Other strike sources, on a fresh connection.
        let b = Client::join(port, "B").await;
        b.send(json!({ "t": "event", "name": "e", "args": "not-an-array" }));
        b.send(json!({ "t": "event", "name": 42 }));
        b.send(json!({ "t": "made-up-type" }));
        b.send(json!({ "t": "hello", "proto": PROTOCOL_VERSION, "name": "again" }));
        assert_eq!(
            b.next("kick").await["reason"],
            "too many malformed messages"
        );

        // More shapes that must be shrugged off: not an object, no `t`, absurd nesting, a bad ping.
        let c = Client::join(port, "C").await;
        c.send_text("[1,2,3]");
        c.send_text(r#"{"name":"x"}"#);
        c.send_text("[".repeat(100_000));
        c.send(json!({ "t": "ping", "ts": "soon" }));
        assert_eq!(
            c.next("kick").await["reason"],
            "too many malformed messages"
        );

        // The server is still healthy.
        assert!(server.player(witness.id).is_some());
        assert_eq!(get(port, "/dynamic.json").await.status, 200);
    });
}

#[test]
fn an_oversized_frame_only_costs_the_sender_its_connection() {
    run(async {
        let (server, port) = start(options()).await;
        let a = Client::join(port, "A").await;
        let w = Client::join(port, "W").await;
        a.event("big", json!(["x".repeat(300 * 1024)]));
        a.until_closed().await;
        until("oversized sender dropped", || {
            server.player(a.id).is_none().then_some(())
        })
        .await;
        assert!(server.player(w.id).is_some());
    });
}

#[test]
fn a_panicking_handler_does_not_break_message_handling() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        *recorder.on_event.borrow_mut() = Some(Box::new(|_, name, _| {
            if name == "one" {
                panic!("handler bug (this panic is part of the test)");
            }
        }));
        let a = Client::join(port, "A").await;
        a.event("one", json!([]));
        a.event("two", json!([]));
        until("the event after the panic", || {
            (recorder.events.borrow().len() == 2).then_some(())
        })
        .await;
        assert!(server.player(a.id).is_some());
        assert!(server.emit_net(-1, "still:alive", vec![]));
        assert_eq!(a.next("event").await["name"], "still:alive");
    });
}

#[test]
fn event_flood_is_kicked_with_event_rate_limit() {
    run(async {
        let (server, port) = start(ServerOptions {
            event_rate: 1.0,
            event_burst: 10.0,
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);

        let a = Client::join(port, "A").await;
        for i in 0..40 {
            a.event("spam", json!([i]));
        }
        assert_eq!(
            a.next("kick").await,
            json!({ "t": "kick", "reason": "event rate limit" })
        );
        a.until_closed().await;
        assert_eq!(recorder.drop_reasons(), ["event rate limit"]);
        assert_eq!(
            recorder.events.borrow().len(),
            10,
            "only the burst allowance got through"
        );
    });
}

#[test]
fn state_flood_is_dropped_silently_not_kicked() {
    run(async {
        let (server, port) = start(ServerOptions {
            tick_rate: 10,
            state_rate: Some(5.0),
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);
        let a = Client::join(port, "A").await;
        for seq in 1..=50u32 {
            a.state(seq, at(seq as f32, 0.0, 0.0));
        }
        a.event("done", json!([]));
        until("marker", || {
            (!recorder.events.borrow().is_empty()).then_some(())
        })
        .await;
        let player = server.player(a.id).expect("still connected");
        assert_eq!(player.state.x, 5.0, "only the allowance was applied");
    });
}

// ----------------------------------------------------------- player_connecting

#[test]
fn a_trusted_proxy_hands_over_the_client_address_from_x_forwarded_for() {
    run(async {
        let (server, port) = start(ServerOptions {
            trusted_proxies: vec!["127.0.0.1".parse().unwrap()],
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);
        let _client = Client::join_with(port, "Via Proxy", &[("x-forwarded-for", "203.0.113.9")]).await;

        let seen = recorder.connecting.borrow();
        assert!(seen[0].endpoint.starts_with("203.0.113.9:"), "endpoint was {}", seen[0].endpoint);
        assert_eq!(seen[0].identifiers, ["ip:203.0.113.9"]);
    });
}

#[test]
fn an_untrusted_peer_cannot_forge_x_forwarded_for() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        let _client = Client::join_with(port, "Forger", &[("x-forwarded-for", "203.0.113.9")]).await;

        let seen = recorder.connecting.borrow();
        assert!(seen[0].endpoint.starts_with("127.0.0.1:"));
        assert_eq!(seen[0].identifiers, ["ip:127.0.0.1"]);
    });
}

#[test]
fn a_chain_of_two_proxies_yields_the_address_before_the_trusted_ones() {
    run(async {
        let (server, port) = start(ServerOptions {
            trusted_proxies: vec!["127.0.0.1".parse().unwrap(), "10.0.0.5".parse().unwrap()],
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);
        let _client = Client::join_with(port, "Behind Two", &[("x-forwarded-for", "198.51.100.7, 10.0.0.5")]).await;

        let seen = recorder.connecting.borrow();
        assert!(seen[0].endpoint.starts_with("198.51.100.7:"), "endpoint was {}", seen[0].endpoint);
        assert_eq!(seen[0].identifiers, ["ip:198.51.100.7"]);
    });
}

#[test]
fn player_connecting_synchronous_reject() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        *recorder.on_connecting.borrow_mut() = Some(Box::new(|info| match info.name.as_str() {
            "Banned" => ConnectDecision::Reject("you are banned".to_string()),
            "Unexplained" => ConnectDecision::Reject(String::new()),
            _ => ConnectDecision::Accept,
        }));

        let hello = |name: &str| json!({ "t": "hello", "proto": PROTOCOL_VERSION, "name": name, "build": "b1", "token": "secret" });
        assert_eq!(
            expect_reject(port, Some(hello("Banned"))).await,
            "you are banned"
        );
        assert_eq!(
            expect_reject(port, Some(hello("Unexplained"))).await,
            "connection rejected"
        );
        assert!(recorder.joined.borrow().is_empty());
        assert_eq!(server.player_count(), 0);
        {
            let seen = recorder.connecting.borrow();
            assert_eq!(seen[0].name, "Banned");
            assert!(seen[0].endpoint.starts_with("127.0.0.1:"));
            assert_eq!(seen[0].identifiers, ["ip:127.0.0.1"]);
            assert_eq!(seen[0].build.as_deref(), Some("b1"));
            assert_eq!(seen[0].token.as_deref(), Some("secret"));
        }

        let ok = Client::join(port, "Fine").await;
        assert_eq!(ok.id, 1, "the rejected connections did not consume an id");
    });
}

#[test]
fn player_connecting_deferral_can_reject_accept_time_out_or_be_abandoned() {
    run(async {
        let (server, port) = start(ServerOptions {
            defer_timeout_ms: 400,
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);
        let never: Rc<RefCell<Vec<oneshot::Sender<Option<String>>>>> = Rc::default();
        let parked = never.clone();
        *recorder.on_connecting.borrow_mut() = Some(Box::new(move |info| {
            let (answer, decision) = oneshot::channel();
            let verdict = match info.name.as_str() {
                "Slow-No" => Some("whitelist only".to_string()),
                "Slow-Yes" => None,
                "Never" => {
                    parked.borrow_mut().push(answer);
                    return ConnectDecision::Defer(decision);
                }
                _ => return ConnectDecision::Defer(decision), // the sender is dropped right here
            };
            tokio::task::spawn_local(async move {
                // Long enough that the frame sent while pending is read before the verdict, even on a slow machine.
                sleep(ms(150)).await;
                let _ = answer.send(verdict);
            });
            ConnectDecision::Defer(decision)
        }));

        let no = Client::connect(port).await;
        no.hello("Slow-No");
        sleep(ms(10)).await;
        assert_eq!(server.player_count(), 0, "held while deferred");
        assert_eq!(no.next("reject").await["reason"], "whitelist only");
        no.until_closed().await;

        let yes = Client::connect(port).await;
        yes.hello("Slow-Yes");
        yes.event("ignored while pending", json!([]));
        sleep(ms(10)).await;
        assert_eq!(
            server.player_count(),
            0,
            "no slot assigned before the deferral completes"
        );
        assert_eq!(yes.next("welcome").await["id"], 1);

        let never_answered = Client::connect(port).await;
        never_answered.hello("Never");
        assert_eq!(
            never_answered.next("reject").await["reason"],
            "connection deferral timed out"
        );
        never_answered.until_closed().await;

        let abandoned = Client::connect(port).await;
        abandoned.hello("Abandoned");
        assert_eq!(
            abandoned.next("reject").await["reason"],
            "connection rejected"
        );

        assert_eq!(server.player_count(), 1);
        assert!(recorder.events.borrow().is_empty());
    });
}

#[test]
fn a_client_that_disconnects_during_a_deferral_never_joins() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        let pending: Rc<RefCell<Option<oneshot::Sender<Option<String>>>>> = Rc::default();
        let slot = pending.clone();
        *recorder.on_connecting.borrow_mut() = Some(Box::new(move |_| {
            let (answer, decision) = oneshot::channel();
            *slot.borrow_mut() = Some(answer);
            ConnectDecision::Defer(decision)
        }));

        let ghost = Client::connect(port).await;
        ghost.hello("Ghost");
        let answer = until("deferral", || pending.borrow_mut().take()).await;
        ghost.command(Command::Terminate);
        sleep(ms(50)).await; // the server notices the dead socket
        let _ = answer.send(None);
        sleep(ms(30)).await;
        assert!(recorder.joined.borrow().is_empty());
        assert_eq!(server.player_count(), 0);
    });
}

// ------------------------------------------------------- kicks and the handler

#[test]
fn kick_client_gets_kick_others_get_leave_player_dropped_fires_id_is_freed_at_once() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        let a = Client::join(port, "A").await;
        let b = Client::join(port, "B").await;

        assert!(server.kick(a.id, "bye"));
        assert!(server.player(a.id).is_none(), "removed synchronously");
        assert!(!server.kick(a.id, "twice"), "harmless");
        assert!(!server.emit_net(i32::from(a.id), "x", vec![]));

        assert_eq!(
            a.next("kick").await,
            json!({ "t": "kick", "reason": "bye" })
        );
        a.until_closed().await;
        assert_eq!(a.received.close_code.get(), Some(4002));
        assert_eq!(
            b.next("leave").await,
            json!({ "t": "leave", "id": a.id, "reason": "bye" })
        );
        assert_eq!(
            *recorder.dropped.borrow(),
            [(a.id, "A".to_string(), "bye".to_string())]
        );
    });
}

#[test]
fn handlers_may_call_back_into_the_server() {
    run(async {
        let (server, port) = start(ServerOptions {
            tick_rate: 100,
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);

        // emit_net(-1) and set_client_resources inside player_joined.
        let s = server.clone();
        *recorder.on_joined.borrow_mut() = Some(Box::new(move |player| {
            assert!(s.emit_net(-1, "hello:all", vec![json!(player.id)]));
            assert_eq!(s.player(player.id).unwrap().name, player.name);
            s.set_client_resources(vec![ClientResource {
                name: format!("after-{}", player.id),
                scripts: vec![],
            }]);
        }));
        // Kicking the very player whose event is being handled.
        let s = server.clone();
        *recorder.on_event.borrow_mut() = Some(Box::new(move |player, name, _| {
            if name == "kick:me" {
                assert!(s.kick(player, "asked for it"));
                assert!(s.player(player).is_none());
                assert!(!s.emit_net(i32::from(player), "too:late", vec![]));
            }
        }));
        // The slot is already free inside player_dropped, and the server is usable.
        let s = server.clone();
        let seen_in_dropped: Rc<RefCell<Vec<(bool, usize)>>> = Rc::default();
        let seen = seen_in_dropped.clone();
        *recorder.on_dropped.borrow_mut() = Some(Box::new(move |player, reason| {
            seen.borrow_mut()
                .push((s.player(player.id).is_none(), s.player_count()));
            s.emit_net(-1, "bye", vec![json!(player.id), json!(reason)]);
        }));

        let a = Client::join(port, "A").await;
        assert_eq!(
            a.next("event").await,
            json!({ "t": "event", "name": "hello:all", "args": [1], "src": 0 })
        );
        let b = Client::join(port, "B").await;
        assert_eq!(
            b.welcome["resources"],
            json!([{ "name": "after-1", "scripts": [] }])
        );
        assert_eq!(a.next("event").await["args"], json!([2]));
        assert_eq!(b.next("event").await["args"], json!([2]));

        b.event("kick:me", json!([]));
        assert_eq!(b.next("kick").await["reason"], "asked for it");
        b.until_closed().await;
        assert_eq!(
            a.next("leave").await,
            json!({ "t": "leave", "id": 2, "reason": "asked for it" })
        );
        assert_eq!(
            a.next("event").await,
            json!({ "t": "event", "name": "bye", "args": [2, "asked for it"], "src": 0 })
        );
        assert_eq!(*seen_in_dropped.borrow(), [(true, 1)]);

        // Kicking another player from inside tick.
        let c = Client::join(port, "C").await;
        let s = server.clone();
        let target = c.id;
        *recorder.on_tick.borrow_mut() = Some(Box::new(move |_| {
            s.kick(target, "tick says no");
        }));
        assert_eq!(c.next("kick").await["reason"], "tick says no");
        assert_eq!(a.next("leave").await["reason"], "tick says no");
        assert_eq!(recorder.drop_reasons(), ["asked for it", "tick says no"]);
        assert_eq!(server.player_ids(), [a.id]);
    });
}

#[test]
fn a_handler_may_stop_the_server() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        let s = server.clone();
        *recorder.on_event.borrow_mut() = Some(Box::new(move |_, name, _| {
            if name == "shutdown" {
                let s = s.clone();
                tokio::task::spawn_local(async move { s.stop("stopped by a script").await });
            }
        }));
        let a = Client::join(port, "A").await;
        a.event("shutdown", json!([]));
        assert_eq!(a.next("kick").await["reason"], "stopped by a script");
        a.until_closed().await;
        server.stop("ignored").await; // waits for the stop already in progress
        assert_eq!(recorder.drop_reasons(), ["stopped by a script"]);
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    });
}

#[test]
fn a_client_that_stops_reading_is_dropped_once_its_backlog_is_too_large() {
    run(async {
        let (server, port) = start(options()).await;
        let recorder = Recorder::install(&server);
        let reader = Client::join(port, "Reader").await;
        let stalled = Client::join(port, "Stalled").await;
        stalled.command(Command::Pause);

        // 16 MiB may pile up for one client; the megabyte after that is one too many.
        let megabyte = json!("x".repeat(1024 * 1024));
        let mut accepted = 0;
        while server.emit_net(i32::from(stalled.id), "bulk", vec![megabyte.clone()]) {
            accepted += 1;
            assert!(accepted < 64, "the backlog limit never triggered");
        }
        assert!(accepted >= 16);
        assert!(server.player(stalled.id).is_none());
        assert_eq!(recorder.drop_reasons(), ["send buffer overflow"]);
        assert_eq!(reader.next("leave").await["reason"], "send buffer overflow");
        assert!(server.emit_net(i32::from(reader.id), "fine", vec![]));
        assert_eq!(reader.next("event").await["name"], "fine");
    });
}

#[test]
fn heartbeat_drops_a_client_that_stops_answering_pings() {
    run(async {
        let (server, port) = start(ServerOptions {
            heartbeat_interval_ms: 25,
            heartbeat_timeout_ms: 90,
            ..options()
        })
        .await;
        let recorder = Recorder::install(&server);

        let alive = Client::join(port, "Alive").await;
        let dead = Client::join(port, "Dead").await;
        dead.command(Command::Pause);
        until("timeout drop", || {
            (!recorder.dropped.borrow().is_empty()).then_some(())
        })
        .await;
        assert_eq!(
            *recorder.dropped.borrow(),
            [(dead.id, "Dead".to_string(), "timed out".to_string())]
        );
        dead.command(Command::Resume);
        dead.until_closed().await;
        assert_eq!(alive.next("leave").await["reason"], "timed out");

        sleep(ms(120)).await;
        assert_eq!(server.player_count(), 1, "the responsive client stays");
        assert_eq!(recorder.dropped.borrow().len(), 1);
    });
}

// ------------------------------------------------------------------- lifecycle

#[test]
fn stop_kicks_everyone_is_idempotent_and_frees_the_port() {
    run(async {
        let server = Server::new(options()).unwrap();
        let port = server.start().await.unwrap();
        assert_eq!(server.port(), port);
        assert_eq!(
            server.start().await.unwrap(),
            port,
            "start() twice returns the same port"
        );

        let recorder = Recorder::install(&server);
        let a = Client::join(port, "A").await;
        let idle = Client::connect(port).await; // connected but never said hello

        let started = Instant::now();
        tokio::join!(server.stop("maintenance"), server.stop("ignored"));
        assert!(
            started.elapsed() < ms(900),
            "clients that close cleanly are not waited for"
        );

        assert_eq!(
            a.next("kick").await,
            json!({ "t": "kick", "reason": "maintenance" })
        );
        a.until_closed().await;
        assert_eq!(idle.next("reject").await["reason"], "maintenance");
        idle.until_closed().await;
        assert_eq!(recorder.drop_reasons(), ["maintenance"]);
        assert_eq!(server.player_count(), 0);
        assert!(http(port, "GET", "/info.json", None).await.is_err());
        assert!(server
            .start()
            .await
            .unwrap_err()
            .to_string()
            .contains("stopped"));

        // The port can be bound again straight away.
        let again = Server::new(ServerOptions { port, ..options() }).unwrap();
        assert_eq!(again.start().await.unwrap(), port);
        let ticks_at_stop = server.tick();
        sleep(ms(60)).await;
        assert_eq!(
            server.tick(),
            ticks_at_stop,
            "the stopped server no longer ticks"
        );
        again.stop("done").await;
    });
}

#[test]
fn stop_cuts_off_a_client_that_ignores_the_close_handshake() {
    run(async {
        let (server, port) = start(ServerOptions {
            close_grace_ms: 100,
            ..options()
        })
        .await;
        let stuck = Client::join(port, "Stuck").await;
        stuck.command(Command::Pause); // never reads the close frame, never answers it
        let started = Instant::now();
        server.stop("bye").await;
        assert!(started.elapsed() >= ms(90));
        assert!(started.elapsed() < ms(1500));
        stuck.command(Command::Resume);
        stuck.until_closed().await;
    });
}

#[test]
fn start_fails_when_the_port_is_taken_and_stop_before_start_is_fine() {
    run(async {
        let (_server, port) = start(options()).await;
        let clash = Server::new(ServerOptions { port, ..options() }).unwrap();
        assert_eq!(
            clash.start().await.unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        clash.stop("unused").await;

        let unused = Server::new(options()).unwrap();
        unused.stop("unused").await;
        assert!(unused.start().await.is_err());

        assert!(Server::new(ServerOptions {
            max_players: 70_000,
            ..options()
        })
        .is_err());
        assert!(Server::new(ServerOptions {
            max_players: 0,
            ..options()
        })
        .is_err());
        assert!(Server::new(ServerOptions {
            tick_rate: 0,
            ..options()
        })
        .is_err());
        assert!(Server::new(ServerOptions {
            tick_rate: 1001,
            ..options()
        })
        .is_err());
        assert!(Server::new(ServerOptions {
            stream_distance: f32::NAN,
            ..options()
        })
        .is_err());
    });
}

// ------------------------------------------------------------------------ http

#[test]
fn http_info_json() {
    run(async {
        let (server, port) = start(ServerOptions {
            name: "Info <b>Box</b>".to_string(),
            max_players: 12,
            tick_rate: 25,
            build: "9.9.9".to_string(),
            tags: vec!["rp".to_string(), "eu".to_string()],
            vars: [("locale".to_string(), "en".to_string())].into(),
            ..options()
        })
        .await;
        server.set_info_var("gametype", Some("freeroam"));
        server.set_info_var("locale", None);
        server.set_resource_names(vec!["chat".to_string(), "spawn".to_string()]);

        let res = get(port, "/info.json").await;
        assert_eq!(res.status, 200);
        assert_eq!(
            res.header("content-type"),
            "application/json; charset=utf-8"
        );
        assert_eq!(res.header("access-control-allow-origin"), "*");
        assert_eq!(res.header("x-content-type-options"), "nosniff");
        assert_eq!(res.header("cache-control"), "no-store");
        assert_eq!(res.header("content-length"), res.body.len().to_string());
        // Byte for byte what the Node server sends, key order included.
        assert_eq!(
            res.body,
            r#"{"server":"SixM","name":"Info <b>Box</b>","build":"9.9.9","protocol":1,"maxPlayers":12,"tickRate":25,"tags":["rp","eu"],"vars":{"gametype":"freeroam"},"resources":["chat","spawn"]}"#
        );
    });
}

#[test]
fn http_players_json_and_dynamic_json_expose_no_addresses_or_identifiers() {
    run(async {
        let (_server, port) = start(ServerOptions {
            name: "Dyn".to_string(),
            max_players: 5,
            ..options()
        })
        .await;
        let _alice = Client::join(port, "Alice").await;
        let _bob = Client::join(port, "Bob").await;

        let res = get(port, "/players.json?cachebust=1").await;
        assert_eq!(res.status, 200);
        assert!(res.header("content-type").starts_with("application/json"));
        assert_eq!(res.header("access-control-allow-origin"), "*");
        let players = res.json();
        let players = players.as_array().unwrap();
        assert_eq!(players.len(), 2);
        for p in players {
            let mut keys: Vec<&String> = p.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["id", "name", "ping"]);
            assert!(p["ping"].is_i64());
        }
        assert_eq!(
            (&players[0]["id"], &players[0]["name"]),
            (&json!(1), &json!("Alice"))
        );
        assert_eq!(
            (&players[1]["id"], &players[1]["name"]),
            (&json!(2), &json!("Bob"))
        );
        for leak in ["127.0.0.1", "identifier", "endpoint", "ip:"] {
            assert!(!res.body.contains(leak));
        }

        let dynamic = get(port, "/dynamic.json").await;
        assert_eq!(dynamic.status, 200);
        assert!(dynamic
            .header("content-type")
            .starts_with("application/json"));
        let body = dynamic.json();
        let uptime = body["uptime"]
            .as_u64()
            .expect("uptime is a whole number of seconds");
        assert_eq!(
            body,
            json!({ "name": "Dyn", "clients": 2, "maxClients": 5, "uptime": uptime })
        );
    });
}

#[test]
fn http_status_page_is_self_contained_and_escapes_user_controlled_text() {
    run(async {
        let (_server, port) = start(ServerOptions {
            name: "<script>alert(1)</script> & \"Co\"".to_string(),
            ..options()
        })
        .await;
        let _player = Client::join(port, "<img src=x onerror=1>").await;

        let res = get(port, "/").await;
        assert_eq!(res.status, 200);
        assert_eq!(res.header("content-type"), "text/html; charset=utf-8");
        assert_eq!(res.header("x-content-type-options"), "nosniff");
        assert_eq!(res.header("cache-control"), "no-store");
        assert_eq!(res.header("referrer-policy"), "no-referrer");
        assert_eq!(
            res.header("content-security-policy"),
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'"
        );
        let html = &res.body;
        assert!(html.starts_with("<!doctype html>\n<html lang=\"en\">"));
        assert!(html.ends_with("</script>\n</body>\n</html>\n"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;Co&quot;"));
        assert!(html.contains("<tr><td>1</td><td>&lt;img src=x onerror=1&gt;</td><td>"));
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains("<img"));
        assert!(html.contains("1 / 32"));
        assert!(html.contains("var max = 32;"));
        assert!(html.contains("players.json"));
        assert!(html.contains(&format!(
            "<footer>SixM {} &middot; protocol 1</footer>",
            env!("CARGO_PKG_VERSION")
        )));
        for external in [
            "src=\"http",
            "href=\"http",
            "src=\"//",
            "href=\"//",
            "src='http",
            "href='http",
        ] {
            assert!(!html.contains(external), "no external requests");
        }
        assert!(!html.contains("127.0.0.1"));

        assert_eq!(get(port, "/index.html").await.body, *html);
    });
}

#[test]
fn http_404_for_unknown_paths_405_for_other_methods_head_has_no_body() {
    run(async {
        let (_server, port) = start(options()).await;

        let missing = get(port, "/nope").await;
        assert_eq!(missing.status, 404);
        assert!(missing
            .header("content-type")
            .starts_with("application/json"));
        assert_eq!(missing.header("x-content-type-options"), "nosniff");
        assert_eq!(missing.body, r#"{"error":"not found"}"#);

        for method in ["POST", "PUT", "DELETE"] {
            let body = (method != "DELETE").then_some("{}");
            let res = http(port, method, "/info.json", body).await.unwrap();
            assert_eq!(res.status, 405, "{method}");
            assert_eq!(res.header("allow"), "GET, HEAD");
            assert!(res.header("content-type").starts_with("application/json"));
            assert_eq!(res.body, r#"{"error":"method not allowed"}"#);
        }

        let head = http(port, "HEAD", "/info.json", None).await.unwrap();
        assert_eq!(head.status, 200);
        assert!(head.header("content-length").parse::<usize>().unwrap() > 0);
        assert_eq!(head.body, "");

        // Garbage instead of HTTP is answered with a 400 and does not disturb the server.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"\x00\x01garbage\r\n\r\n").await.unwrap();
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw).await;
        assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 400"));
        assert_eq!(get(port, "/dynamic.json").await.status, 200);
    });
}
