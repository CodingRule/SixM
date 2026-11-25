//! SixM server core: networking, players, state sync and the HTTP status
//! endpoints. This module knows nothing about scripting; the Lua layer sits on
//! top and talks to it through [`ServerHandler`] and the methods of [`Server`].
//!
//! Everything runs on one thread (a tokio `current_thread` runtime inside a
//! `LocalSet`). Server state lives behind `RefCell`s that are never borrowed
//! across a handler call or an `.await`, so handlers may freely call back into
//! the [`Server`].

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Serialize;
use serde_json::Value;
use sixm_protocol::{
    decode_state, encode_snapshot, sanitize_name, ClientResource, EntityState, Msg, DEFAULT_PORT,
    MAX_EVENT_NAME_LENGTH, MAX_JSON_FRAME, PROTOCOL_VERSION,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout, Instant};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::WebSocketStream;

use crate::logger::Logger;

const CLOSE_REJECTED: u16 = 4001;
const CLOSE_KICKED: u16 = 4002;
/// Snapshots are skipped for a client whose send queue is this far behind.
const SNAPSHOT_BACKLOG_LIMIT: usize = 1024 * 1024;
/// A client that lets this much pile up is not reading at all: drop it.
const SEND_BACKLOG_LIMIT: usize = 16 * 1024 * 1024;
/// How long a connection the server cuts off may take to flush what was already queued for it.
const FLUSH_GRACE: Duration = Duration::from_millis(250);

type WsStream = WebSocketStream<TokioIo<Upgraded>>;
type HttpResponse = Response<Full<Bytes>>;
type Deferral = (
    String,
    Pin<Box<tokio::time::Timeout<oneshot::Receiver<Option<String>>>>>,
);

// ------------------------------------------------------------------ public API

/// Construction options. `Default` mirrors the documented server defaults; the
/// timing and limit fields beyond the documented ones exist mainly so tests can run fast.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// TCP port for both HTTP and WebSocket; 0 picks a free one.
    pub port: u16,
    pub host: String,
    pub name: String,
    /// Player slots, 1-65535.
    pub max_players: u32,
    /// Ticks (and snapshots) per second, 1-1000.
    pub tick_rate: u32,
    /// Players further apart than this do not see each other; `<= 0` means unlimited.
    pub stream_distance: f32,
    /// Version string shown in `/info.json` and on the status page.
    pub build: String,
    pub tags: Vec<String>,
    /// Public variables shown in `/info.json`.
    pub vars: BTreeMap<String, String>,
    pub logger: Logger,
    /// How long a new connection may take to send its `hello`.
    pub hello_timeout_ms: u64,
    /// How long a deferred `player_connecting` decision may take.
    pub defer_timeout_ms: u64,
    pub heartbeat_interval_ms: u64,
    /// A player silent for this long is dropped with "timed out".
    pub heartbeat_timeout_ms: u64,
    /// Malformed frames tolerated before a kick.
    pub max_strikes: u32,
    /// One strike is forgiven per this many milliseconds without a new one.
    pub strike_decay_ms: u64,
    /// Sustained client events per second.
    pub event_rate: f64,
    /// Client event burst allowance.
    pub event_burst: f64,
    /// STATE frames accepted per second; `None` means four per tick.
    pub state_rate: Option<f64>,
    /// How long a closing connection may take to finish the close handshake.
    pub close_grace_ms: u64,
    /// Connections that may be open without having joined; `None` means `2 * max_players + 16`.
    pub max_pending_connections: Option<usize>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            port: DEFAULT_PORT,
            host: "0.0.0.0".to_string(),
            name: "SixM Server".to_string(),
            max_players: 32,
            tick_rate: 20,
            stream_distance: 500.0,
            build: env!("CARGO_PKG_VERSION").to_string(),
            tags: Vec::new(),
            vars: BTreeMap::new(),
            logger: Logger::new("server"),
            hello_timeout_ms: 5000,
            defer_timeout_ms: 10_000,
            heartbeat_interval_ms: 5000,
            heartbeat_timeout_ms: 15_000,
            max_strikes: 5,
            strike_decay_ms: 10_000,
            event_rate: 50.0,
            event_burst: 100.0,
            state_rate: None,
            close_grace_ms: 1000,
            max_pending_connections: None,
        }
    }
}

/// An owned snapshot of a joined player, taken when it was requested.
#[derive(Clone, Debug)]
pub struct PlayerInfo {
    pub id: u16,
    pub name: String,
    /// `address:port` of the client socket.
    pub endpoint: String,
    pub identifiers: Vec<String>,
    /// Last measured WebSocket round trip in milliseconds; 0 until the first measurement.
    pub ping: i32,
    pub state: EntityState,
    /// False until the player's first accepted STATE frame.
    pub has_state: bool,
    /// Unix time of the join, in milliseconds.
    pub joined_at_ms: u64,
}

/// What is known about a client while it is still knocking.
#[derive(Clone, Debug)]
pub struct ConnectInfo {
    /// The sanitised player name.
    pub name: String,
    pub endpoint: String,
    pub identifiers: Vec<String>,
    pub build: Option<String>,
    pub token: Option<String>,
}

/// The answer to [`ServerHandler::player_connecting`].
pub enum ConnectDecision {
    Accept,
    /// Refuse with this reason (shown to the client).
    Reject(String),
    /// Decide later: send `None` to accept or `Some(reason)` to reject. Dropping
    /// the sender, or not answering within the deferral timeout, rejects.
    Defer(oneshot::Receiver<Option<String>>),
}

/// The scripting layer implements this. Every method has a default no-op.
///
/// Handlers run on the server thread and may call any [`Server`] method,
/// including kicking the player the callback is about.
pub trait ServerHandler {
    /// A client sent a valid `hello` and a slot is free.
    fn player_connecting(&self, _info: &ConnectInfo) -> ConnectDecision {
        ConnectDecision::Accept
    }
    /// The player has been welcomed and announced to the others.
    fn player_joined(&self, _player: &PlayerInfo) {}
    /// The player is gone; its slot is already free when this runs.
    fn player_dropped(&self, _player: &PlayerInfo, _reason: &str) {}
    /// A client triggered a net event.
    fn net_event(&self, _player: u16, _name: &str, _args: Vec<Value>) {}
    /// Runs once per tick, before the snapshots of that tick are sent.
    fn tick(&self, _tick: u64, _dt_ms: f64) {}
}

/// A cheap handle to the server; clones share one server.
///
/// Call [`Server::stop`] to shut down: dropping the handles of a started
/// server does not stop it.
#[derive(Clone)]
pub struct Server {
    inner: Rc<Inner>,
}

impl Server {
    /// Validates the options; nothing is bound until [`Server::start`].
    pub fn new(options: ServerOptions) -> Result<Server, String> {
        if !(1..=65535).contains(&options.max_players) {
            return Err("maxPlayers must be an integer 1-65535".to_string());
        }
        if !(1..=1000).contains(&options.tick_rate) {
            return Err("tickRate must be a number in (0, 1000]".to_string());
        }
        if options.stream_distance.is_nan() {
            return Err("streamDistance must be a number".to_string());
        }
        let limits = Limits::from_options(&options);
        let (shutdown, _) = watch::channel(false);
        let mut http = http1::Builder::new();
        http.title_case_headers(true).timer(TokioTimer::new());
        Ok(Server {
            inner: Rc::new(Inner {
                name: options.name,
                host: options.host,
                requested_port: options.port,
                max_players: options.max_players,
                tick_rate: options.tick_rate,
                stream_distance: options.stream_distance,
                build: options.build,
                tags: options.tags,
                limits,
                log: options.logger,
                http,
                state: RefCell::new(State {
                    vars: options.vars,
                    ..State::default()
                }),
                handler: RefCell::new(None),
                tick: Cell::new(0),
                running: Cell::new(false),
                lifecycle: Cell::new(Lifecycle::Idle),
                stopped: Notify::new(),
                shutdown,
                open_sockets: Cell::new(0),
            }),
        })
    }

    /// Binds the port and starts ticking. Must be called inside a `LocalSet`.
    /// Returns the bound port; calling it again returns the same port.
    pub async fn start(&self) -> std::io::Result<u16> {
        let inner = &self.inner;
        match inner.lifecycle.get() {
            Lifecycle::Idle => {}
            Lifecycle::Running => return Ok(self.port()),
            Lifecycle::Stopping | Lifecycle::Stopped => {
                return Err(std::io::Error::other(
                    "server was stopped; create a new instance",
                ));
            }
        }
        let listener = std::net::TcpListener::bind((inner.host.as_str(), inner.requested_port))?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let port = listener.local_addr()?.port();

        inner.lifecycle.set(Lifecycle::Running);
        inner.running.set(true);
        let tasks = vec![
            tokio::task::spawn_local(inner.clone().accept_loop(listener)),
            tokio::task::spawn_local(inner.clone().tick_loop()),
            tokio::task::spawn_local(inner.clone().heartbeat_loop()),
        ];
        {
            let mut st = inner.state.borrow_mut();
            st.bound_port = Some(port);
            st.started_at = Some(Instant::now());
            st.tasks = tasks;
        }
        inner.log.info(format!(
            "\"{}\" listening on {}:{port} ({} Hz, {} slots)",
            inner.name, inner.host, inner.tick_rate, inner.max_players
        ));
        Ok(port)
    }

    /// Kicks everyone, closes every socket and the listener. Idempotent: a second
    /// call waits for the first to finish (its reason is ignored).
    pub async fn stop(&self, reason: &str) {
        let inner = &self.inner;
        loop {
            // Registered before the check, so a completion in between is not missed.
            let stopped = inner.stopped.notified();
            match inner.lifecycle.get() {
                Lifecycle::Stopped => return,
                Lifecycle::Stopping => stopped.await,
                Lifecycle::Idle | Lifecycle::Running => break,
            }
        }
        inner.lifecycle.set(Lifecycle::Stopping);
        inner.running.set(false);
        let tasks = std::mem::take(&mut inner.state.borrow_mut().tasks);
        for task in &tasks {
            task.abort();
        }

        for id in self.player_ids() {
            inner.kick(id, reason);
        }
        for conn in inner.connections() {
            inner.reject(&conn, reason);
        }
        // Waiting for the aborted accept loop guarantees the port is free again.
        for task in tasks {
            let _ = task.await;
        }
        let _ = inner.shutdown.send(true); // ends idle HTTP connections

        // Give clients a moment to complete the close handshake, then cut them off.
        let deadline = Instant::now() + inner.limits.close_grace.min(Duration::from_millis(500));
        while inner.open_sockets.get() > 0 && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        for conn in inner.connections() {
            conn.terminate();
        }
        let deadline = Instant::now() + FLUSH_GRACE * 2;
        while inner.open_sockets.get() > 0 && Instant::now() < deadline {
            sleep(Duration::from_millis(1)).await;
        }

        inner.log.info(format!("server stopped ({reason})"));
        inner.lifecycle.set(Lifecycle::Stopped);
        inner.stopped.notify_waiters();
    }

    /// The bound port once started, otherwise the requested one.
    pub fn port(&self) -> u16 {
        self.inner
            .state
            .borrow()
            .bound_port
            .unwrap_or(self.inner.requested_port)
    }

    pub fn name(&self) -> String {
        self.inner.name.clone()
    }

    /// Current tick counter; 0 until the first tick has run.
    pub fn tick(&self) -> u64 {
        self.inner.tick.get()
    }

    pub fn max_players(&self) -> u32 {
        self.inner.max_players
    }

    pub fn tick_rate(&self) -> u32 {
        self.inner.tick_rate
    }

    /// Ids of the joined players, ascending.
    pub fn player_ids(&self) -> Vec<u16> {
        self.inner.state.borrow().players.keys().copied().collect()
    }

    pub fn player_count(&self) -> usize {
        self.inner.state.borrow().players.len()
    }

    /// A snapshot of one joined player; `None` once it has been dropped.
    pub fn player(&self, id: u16) -> Option<PlayerInfo> {
        self.inner.state.borrow().players.get(&id).map(Player::info)
    }

    /// Trigger a client event. `target` is a player id, or -1 for everyone.
    /// Returns false when the event name is invalid or the target is unknown.
    pub fn emit_net(&self, target: i32, name: &str, args: Vec<Value>) -> bool {
        let inner = &self.inner;
        if !valid_event_name(name) {
            inner.log.warn("emit_net: invalid event name");
            return false;
        }
        let event = Msg::Event {
            name: name.to_string(),
            args,
            src: Some(0),
        };
        if target == -1 {
            inner.broadcast(&event, None);
            return true;
        }
        match u16::try_from(target) {
            Ok(id) => inner.send_player(id, text_frame(&event)),
            Err(_) => false,
        }
    }

    /// Tell the client why, close its socket and free its slot immediately.
    /// Returns false when there is no such player.
    pub fn kick(&self, id: u16, reason: &str) -> bool {
        self.inner.kick(id, reason)
    }

    /// The client scripts sent as `resources` in every later WELCOME.
    pub fn set_client_resources(&self, resources: Vec<ClientResource>) {
        self.inner.state.borrow_mut().client_resources = resources;
    }

    /// Names listed under `resources` in `/info.json`.
    pub fn set_resource_names(&self, names: Vec<String>) {
        self.inner.state.borrow_mut().resource_names = names;
    }

    /// Set (or with `None`, remove) a public variable shown in `/info.json`.
    pub fn set_info_var(&self, key: &str, value: Option<&str>) {
        let mut st = self.inner.state.borrow_mut();
        match value {
            Some(value) => st.vars.insert(key.to_string(), value.to_string()),
            None => st.vars.remove(key),
        };
    }

    /// Install the callbacks; replaces any previous handler.
    pub fn set_handler(&self, handler: Rc<dyn ServerHandler>) {
        *self.inner.handler.borrow_mut() = Some(handler);
    }
}

// -------------------------------------------------------------------- internals

/// Options after applying fallbacks for unusable values.
struct Limits {
    hello_timeout: Duration,
    defer_timeout: Duration,
    heartbeat_interval: Duration,
    heartbeat_timeout: Duration,
    max_strikes: u32,
    strike_decay: Duration,
    event_rate: f64,
    event_burst: f64,
    state_rate: f64,
    close_grace: Duration,
    max_pending: usize,
}

impl Limits {
    fn from_options(o: &ServerOptions) -> Limits {
        let ms = |value: u64, fallback: u64| {
            Duration::from_millis(if value > 0 { value } else { fallback })
        };
        let positive = |value: f64, fallback: f64| {
            if value.is_finite() && value > 0.0 {
                value
            } else {
                fallback
            }
        };
        let default_state_rate = f64::from(o.tick_rate) * 4.0;
        Limits {
            hello_timeout: ms(o.hello_timeout_ms, 5000),
            defer_timeout: ms(o.defer_timeout_ms, 10_000),
            heartbeat_interval: ms(o.heartbeat_interval_ms, 5000),
            heartbeat_timeout: ms(o.heartbeat_timeout_ms, 15_000),
            max_strikes: if o.max_strikes > 0 { o.max_strikes } else { 5 },
            strike_decay: ms(o.strike_decay_ms, 10_000),
            event_rate: positive(o.event_rate, 50.0),
            event_burst: positive(o.event_burst, 100.0).max(1.0),
            state_rate: positive(
                o.state_rate.unwrap_or(default_state_rate),
                default_state_rate,
            )
            .max(1.0),
            close_grace: ms(o.close_grace_ms, 1000),
            max_pending: o
                .max_pending_connections
                .filter(|n| *n > 0)
                .unwrap_or(o.max_players as usize * 2 + 16),
        }
    }
}

struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate_per_second: f64, capacity: f64) -> TokenBucket {
        TokenBucket {
            rate: rate_per_second,
            capacity,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f64() * self.rate;
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.last = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Waiting for the `hello`.
    Hello,
    /// Waiting for a deferred `player_connecting` decision.
    Pending,
    Joined,
    Closing,
}

/// An item of a connection's send queue.
enum Outgoing {
    Frame(Message),
    /// Flush what was queued before this and stop writing.
    Stop,
}

enum SendError {
    /// The connection is closing or gone.
    Closed,
    /// The client is not reading; see [`SEND_BACKLOG_LIMIT`].
    Overflow,
}

/// One open WebSocket, joined or not. Shared between the server state and the
/// task that owns the socket; all of its mutable parts are `Cell`s.
struct Conn {
    id: u64,
    endpoint: String,
    identifiers: Vec<String>,
    stage: Cell<Stage>,
    /// The joined player this socket belongs to, until that player is dropped.
    player: Cell<Option<u16>>,
    /// Still counted against the pending-connection cap (i.e. never joined).
    pending: Cell<bool>,
    tx: mpsc::UnboundedSender<Outgoing>,
    /// Payload bytes queued in `tx` and not yet handed to the socket.
    queued: Cell<usize>,
    /// When the hello (stage `Hello`) or the close handshake (stage `Closing`) times out.
    deadline: Cell<Option<Instant>>,
    terminated: Cell<bool>,
    /// Wakes the owning task after `deadline` or `terminated` changed.
    wake: Notify,
}

impl Conn {
    fn push(&self, frame: Message) {
        self.queued.set(self.queued.get() + frame.len());
        let _ = self.tx.send(Outgoing::Frame(frame));
    }

    fn enqueue(&self, frame: Message) -> Result<(), SendError> {
        if self.stage.get() == Stage::Closing {
            return Err(SendError::Closed);
        }
        if self.queued.get() > SEND_BACKLOG_LIMIT {
            return Err(SendError::Overflow);
        }
        self.push(frame);
        Ok(())
    }

    /// Start a graceful close and make sure the socket dies even if the peer stalls.
    fn close(&self, code: u16, why: &'static str, grace: Duration) {
        if self.stage.replace(Stage::Closing) == Stage::Closing {
            return;
        }
        self.push(Message::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: Utf8Bytes::from_static(why),
        })));
        self.deadline.set(Some(Instant::now() + grace));
        self.wake.notify_one();
    }

    /// Cut the connection without a close handshake.
    fn terminate(&self) {
        self.stage.set(Stage::Closing);
        self.terminated.set(true);
        self.wake.notify_one();
    }
}

/// A connected, fully joined player.
struct Player {
    id: u16,
    name: String,
    conn: Rc<Conn>,
    ping: i32,
    joined_at_ms: u64,
    state: EntityState,
    has_state: bool,
    /// Sequence number of the last accepted STATE frame; meaningless before the first.
    last_seq: u32,
    strikes: u32,
    last_strike_at: Instant,
    last_seen: Instant,
    ping_sent_at: Option<Instant>,
    last_snapshot_count: usize,
    event_bucket: TokenBucket,
    state_bucket: TokenBucket,
}

impl Player {
    fn info(&self) -> PlayerInfo {
        PlayerInfo {
            id: self.id,
            name: self.name.clone(),
            endpoint: self.conn.endpoint.clone(),
            identifiers: self.conn.identifiers.clone(),
            ping: self.ping,
            state: self.state,
            has_state: self.has_state,
            joined_at_ms: self.joined_at_ms,
        }
    }

    /// Send a WebSocket-level ping; the pong measures the round trip.
    fn send_ping(&mut self) {
        if self.conn.enqueue(Message::Ping(Bytes::new())).is_ok() && self.ping_sent_at.is_none() {
            self.ping_sent_at = Some(Instant::now());
        }
    }
}

#[derive(Default)]
struct State {
    players: BTreeMap<u16, Player>,
    /// Every open socket, joined or not.
    conns: HashMap<u64, Rc<Conn>>,
    pending_count: usize,
    next_conn_id: u64,
    vars: BTreeMap<String, String>,
    client_resources: Vec<ClientResource>,
    resource_names: Vec<String>,
    bound_port: Option<u16>,
    started_at: Option<Instant>,
    /// The accept, tick and heartbeat loops.
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Idle,
    Running,
    Stopping,
    Stopped,
}

struct Inner {
    name: String,
    host: String,
    requested_port: u16,
    max_players: u32,
    tick_rate: u32,
    stream_distance: f32,
    build: String,
    tags: Vec<String>,
    limits: Limits,
    log: Logger,
    http: http1::Builder,
    /// Never borrowed across a handler call or an `.await`.
    state: RefCell<State>,
    handler: RefCell<Option<Rc<dyn ServerHandler>>>,
    tick: Cell<u64>,
    running: Cell<bool>,
    lifecycle: Cell<Lifecycle>,
    /// Notified when `stop()` has finished.
    stopped: Notify,
    /// Flipped to true by `stop()`; ends plain HTTP connections.
    shutdown: watch::Sender<bool>,
    /// Tasks currently holding a client socket (HTTP or WebSocket).
    open_sockets: Cell<usize>,
}

/// Counts a task that holds a client socket for as long as it lives.
struct SocketGuard(Rc<Inner>);

impl SocketGuard {
    fn new(inner: &Rc<Inner>) -> SocketGuard {
        inner.open_sockets.set(inner.open_sockets.get() + 1);
        SocketGuard(inner.clone())
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.0.open_sockets.set(self.0.open_sockets.get() - 1);
    }
}

fn text_frame(msg: &Msg) -> Message {
    Message::Text(Utf8Bytes::from(msg.encode()))
}

/// Event names are bounded in UTF-16 units, as protocol v1 clients count them.
fn valid_event_name(name: &str) -> bool {
    !name.is_empty() && name.encode_utf16().count() <= MAX_EVENT_NAME_LENGTH
}

/// A control message is a JSON object with a string `t`. Parsed loosely so a
/// wrong field type can be answered precisely instead of as "not JSON".
fn decode_control(text: &str) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_str(text).ok()? {
        Value::Object(map) if map.get("t").is_some_and(Value::is_string) => Some(map),
        _ => None,
    }
}

fn truncated(value: Option<&Value>, max_chars: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|s| s.chars().take(max_chars).collect())
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Inner {
    // ---------------------------------------------------------------- helpers

    /// Run one handler callback with no state borrowed. A panicking handler is
    /// logged instead of unwinding into the networking code.
    fn call_handler<R>(&self, what: &str, f: impl FnOnce(&dyn ServerHandler) -> R) -> Option<R> {
        let handler = self.handler.borrow().clone()?;
        match catch_unwind(AssertUnwindSafe(|| f(&*handler))) {
            Ok(result) => Some(result),
            Err(payload) => {
                self.log.error(format!(
                    "\"{what}\" handler panicked: {}",
                    panic_message(&*payload)
                ));
                None
            }
        }
    }

    fn connections(&self) -> Vec<Rc<Conn>> {
        self.state.borrow().conns.values().cloned().collect()
    }

    fn conn_of(&self, id: u16) -> Option<Rc<Conn>> {
        self.state.borrow().players.get(&id).map(|p| p.conn.clone())
    }

    /// Queue a frame for player `id`, who must still own `conn`.
    fn send_to(&self, id: u16, conn: &Conn, frame: Message) -> bool {
        if conn.player.get() != Some(id) {
            return false;
        }
        match conn.enqueue(frame) {
            Ok(()) => true,
            Err(SendError::Closed) => false,
            Err(SendError::Overflow) => {
                self.drop_player(id, "send buffer overflow", true);
                false
            }
        }
    }

    fn send_player(&self, id: u16, frame: Message) -> bool {
        self.conn_of(id)
            .is_some_and(|conn| self.send_to(id, &conn, frame))
    }

    /// Send one control message to every player, optionally skipping one id.
    fn broadcast(&self, msg: &Msg, except: Option<u16>) {
        let text = Utf8Bytes::from(msg.encode());
        let targets: Vec<(u16, Rc<Conn>)> = self
            .state
            .borrow()
            .players
            .values()
            .map(|p| (p.id, p.conn.clone()))
            .collect();
        for (id, conn) in targets {
            if Some(id) != except {
                self.send_to(id, &conn, Message::Text(text.clone()));
            }
        }
    }

    fn reject(&self, conn: &Conn, reason: &str) {
        if conn.stage.get() == Stage::Closing {
            return;
        }
        self.log
            .debug(format!("rejected {}: {reason}", conn.endpoint));
        let _ = conn.enqueue(text_frame(&Msg::Reject {
            reason: reason.to_string(),
        }));
        conn.close(CLOSE_REJECTED, "rejected", self.limits.close_grace);
    }

    fn kick(&self, id: u16, reason: &str) -> bool {
        let Some(conn) = self.conn_of(id) else {
            return false;
        };
        self.send_to(
            id,
            &conn,
            text_frame(&Msg::Kick {
                reason: reason.to_string(),
            }),
        );
        self.drop_player(id, reason, false);
        true
    }

    /// Remove a player from the world. `hard` skips the close handshake.
    fn drop_player(&self, id: u16, reason: &str, hard: bool) {
        let Some(player) = self.state.borrow_mut().players.remove(&id) else {
            return;
        };
        let conn = &player.conn;
        conn.player.set(None);
        if hard {
            conn.terminate();
        } else {
            conn.close(CLOSE_KICKED, "kicked", self.limits.close_grace);
        }
        self.log.info(format!(
            "player {id} \"{}\" dropped ({reason})",
            player.name
        ));
        self.broadcast(
            &Msg::Leave {
                id,
                reason: reason.to_string(),
            },
            None,
        );
        let info = player.info();
        self.call_handler("player_dropped", |h| h.player_dropped(&info, reason));
    }

    // ------------------------------------------------------------- connection

    /// Runs for the whole life of one WebSocket: owns the socket, reads frames
    /// and drives the timers of the connection.
    async fn run_connection(
        self: Rc<Self>,
        conn: Rc<Conn>,
        rx: mpsc::UnboundedReceiver<Outgoing>,
        upgrade: OnUpgrade,
    ) {
        let _guard = SocketGuard::new(&self);
        if let Ok(Ok(upgraded)) = timeout(self.limits.hello_timeout, upgrade).await {
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_JSON_FRAME))
                .max_frame_size(Some(MAX_JSON_FRAME));
            let ws = WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                Some(config),
            )
            .await;
            self.pump(&conn, ws, rx).await;
        }

        {
            let mut st = self.state.borrow_mut();
            st.conns.remove(&conn.id);
            if conn.pending.replace(false) {
                st.pending_count -= 1;
            }
        }
        conn.stage.set(Stage::Closing);
        if let Some(id) = conn.player.get() {
            self.drop_player(id, "disconnected", true);
        }
    }

    async fn pump(&self, conn: &Rc<Conn>, ws: WsStream, rx: mpsc::UnboundedReceiver<Outgoing>) {
        let (sink, mut stream) = ws.split();
        let writer = write_loop(sink, rx, conn.clone());
        tokio::pin!(writer);
        let mut writer_done = false;
        let mut deferral: Option<Deferral> = None;

        while !conn.terminated.get() {
            if conn.stage.get() != Stage::Pending {
                deferral = None; // closed while a decision was outstanding
            }
            let deadline = conn.deadline.get();
            tokio::select! {
                biased;
                () = conn.wake.notified() => {}
                keep_reading = &mut writer, if !writer_done => {
                    writer_done = true;
                    // While a close handshake is in flight the reader finishes it (or the
                    // close deadline does); otherwise a dead writer means a dead socket.
                    if !keep_reading && conn.stage.get() != Stage::Closing {
                        break;
                    }
                }
                () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.on_deadline(conn);
                }
                Some(answer) = wait_for(&mut deferral) => {
                    if let Some((name, _)) = deferral.take() {
                        self.on_deferral(conn, &name, answer);
                    }
                }
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => deferral = self.on_frame(conn, Some(text.as_str()), &[]).or(deferral),
                    Some(Ok(Message::Binary(data))) => deferral = self.on_frame(conn, None, &data).or(deferral),
                    Some(Ok(Message::Pong(_))) => self.on_pong(conn),
                    Some(Ok(Message::Close(_))) => self.on_peer_close(conn),
                    Some(Ok(_)) => {} // pings are answered by tungstenite
                    Some(Err(_)) | None => break,
                },
            }
        }

        if conn.terminated.get() && !writer_done {
            // Cut off by the server: let what was queued before that (a KICK) out first.
            let _ = conn.tx.send(Outgoing::Stop);
            let _ = timeout(FLUSH_GRACE, &mut writer).await;
        }
    }

    fn on_deadline(&self, conn: &Conn) {
        conn.deadline.set(None);
        match conn.stage.get() {
            Stage::Hello => self.reject(conn, "handshake timed out"),
            Stage::Closing => conn.terminate(),
            Stage::Pending | Stage::Joined => {}
        }
    }

    /// The peer started (or answered) the close handshake. The owning task keeps
    /// reading until tungstenite has flushed the reply and reports the end.
    fn on_peer_close(&self, conn: &Conn) {
        if conn.stage.replace(Stage::Closing) != Stage::Closing {
            conn.deadline
                .set(Some(Instant::now() + self.limits.close_grace));
        }
        if let Some(id) = conn.player.get() {
            self.drop_player(id, "disconnected", false);
        }
    }

    fn on_pong(&self, conn: &Conn) {
        let Some(id) = conn.player.get() else {
            return;
        };
        if let Some(p) = self.state.borrow_mut().players.get_mut(&id) {
            let now = Instant::now();
            p.last_seen = now;
            if let Some(sent) = p.ping_sent_at.take() {
                p.ping = (now.duration_since(sent).as_secs_f64() * 1000.0).round() as i32;
            }
        }
    }

    /// One data frame: `text` for a text frame, otherwise `data` holds a binary one.
    fn on_frame(&self, conn: &Rc<Conn>, text: Option<&str>, data: &[u8]) -> Option<Deferral> {
        match conn.stage.get() {
            Stage::Hello => return self.on_hello(conn, text),
            Stage::Joined => {
                if let Some(id) = conn.player.get() {
                    if let Some(p) = self.state.borrow_mut().players.get_mut(&id) {
                        p.last_seen = Instant::now();
                    }
                    match text {
                        Some(text) => self.on_control(id, text),
                        None => self.on_state(id, data),
                    }
                }
            }
            Stage::Pending | Stage::Closing => {} // awaiting a deferral, or closing: ignore
        }
        None
    }

    fn on_hello(&self, conn: &Rc<Conn>, text: Option<&str>) -> Option<Deferral> {
        let msg = text.and_then(decode_control).filter(|m| m["t"] == "hello");
        let Some(msg) = msg else {
            self.reject(conn, "expected a hello message");
            return None;
        };
        let proto = msg.get("proto");
        if proto.and_then(Value::as_f64) != Some(f64::from(PROTOCOL_VERSION)) {
            let client = match proto {
                Some(Value::Number(n)) => n.to_string(),
                _ => "unknown".to_string(),
            };
            self.reject(
                conn,
                &format!("protocol version mismatch (server {PROTOCOL_VERSION}, client {client})"),
            );
            return None;
        }
        let name = msg
            .get("name")
            .and_then(Value::as_str)
            .map(sanitize_name)
            .unwrap_or_default();
        if name.is_empty() {
            self.reject(conn, "invalid player name");
            return None;
        }
        if self.state.borrow().players.len() >= self.max_players as usize {
            self.reject(conn, "server is full");
            return None;
        }

        conn.deadline.set(None);
        conn.stage.set(Stage::Pending);
        let info = ConnectInfo {
            name: name.clone(),
            endpoint: conn.endpoint.clone(),
            identifiers: conn.identifiers.clone(),
            build: truncated(msg.get("build"), 64),
            token: truncated(msg.get("token"), 4096),
        };
        let decision = if self.handler.borrow().is_some() {
            // A handler that panics cannot vouch for the client, so the client stays out.
            self.call_handler("player_connecting", |h| h.player_connecting(&info))
                .unwrap_or_else(|| ConnectDecision::Reject("internal server error".to_string()))
        } else {
            ConnectDecision::Accept
        };
        if conn.stage.get() != Stage::Pending {
            return None; // socket closed (or server stopped) while the handler ran
        }
        match decision {
            ConnectDecision::Accept => self.join(conn, &name),
            ConnectDecision::Reject(reason) => self.reject(conn, rejection(&reason)),
            ConnectDecision::Defer(answer) => {
                return Some((name, Box::pin(timeout(self.limits.defer_timeout, answer))));
            }
        }
        None
    }

    fn on_deferral(
        &self,
        conn: &Rc<Conn>,
        name: &str,
        answer: Result<
            Result<Option<String>, oneshot::error::RecvError>,
            tokio::time::error::Elapsed,
        >,
    ) {
        if conn.stage.get() != Stage::Pending {
            return;
        }
        match answer {
            Ok(Ok(None)) => self.join(conn, name),
            Ok(Ok(Some(reason))) => self.reject(conn, rejection(&reason)),
            Ok(Err(_)) => self.reject(conn, rejection("")), // the sender was dropped unanswered
            Err(_) => self.reject(conn, "connection deferral timed out"),
        }
    }

    fn join(&self, conn: &Rc<Conn>, name: &str) {
        if !self.running.get() {
            return self.reject(conn, "server shutting down");
        }
        let joined = {
            let mut st = self.state.borrow_mut();
            lowest_free_id(&st.players, self.max_players).map(|id| {
                let welcome = Msg::Welcome {
                    id,
                    name: name.to_string(),
                    server_name: self.name.clone(),
                    tick_rate: self.tick_rate,
                    max_players: self.max_players,
                    players: st
                        .players
                        .values()
                        .map(|p| sixm_protocol::PlayerInfo {
                            id: p.id,
                            name: p.name.clone(),
                        })
                        .collect(),
                    resources: st.client_resources.clone(),
                };
                let now = Instant::now();
                let player = Player {
                    id,
                    name: name.to_string(),
                    conn: conn.clone(),
                    ping: 0,
                    joined_at_ms: unix_millis(),
                    state: EntityState::default(),
                    has_state: false,
                    last_seq: 0,
                    strikes: 0,
                    last_strike_at: now,
                    last_seen: now,
                    ping_sent_at: None,
                    last_snapshot_count: 0,
                    event_bucket: TokenBucket::new(self.limits.event_rate, self.limits.event_burst),
                    state_bucket: TokenBucket::new(self.limits.state_rate, self.limits.state_rate),
                };
                let info = player.info();
                st.players.insert(id, player);
                if conn.pending.replace(false) {
                    st.pending_count -= 1;
                }
                (id, welcome, info)
            })
        };
        let Some((id, welcome, info)) = joined else {
            return self.reject(conn, "server is full");
        };
        conn.player.set(Some(id));
        conn.stage.set(Stage::Joined);
        self.send_to(id, conn, text_frame(&welcome));

        self.log.info(format!(
            "player {id} \"{name}\" joined from {}",
            conn.endpoint
        ));
        self.broadcast(
            &Msg::Join {
                id,
                name: name.to_string(),
            },
            Some(id),
        );
        if let Some(p) = self.state.borrow_mut().players.get_mut(&id) {
            p.send_ping();
        }
        self.call_handler("player_joined", |h| h.player_joined(&info));
    }

    // ---------------------------------------------------------- client frames

    fn strike(&self, id: u16, why: &str) {
        let strikes = {
            let mut st = self.state.borrow_mut();
            let Some(p) = st.players.get_mut(&id) else {
                return;
            };
            let now = Instant::now();
            let decay = self.limits.strike_decay.as_secs_f64();
            let forgiven = (now.duration_since(p.last_strike_at).as_secs_f64() / decay).floor();
            p.strikes = p.strikes.saturating_sub(forgiven as u32) + 1;
            p.last_strike_at = now;
            p.strikes
        };
        let max = self.limits.max_strikes;
        self.log.debug(format!(
            "player {id} sent a malformed frame ({why}); strike {strikes}/{max}"
        ));
        if strikes >= max {
            self.kick(id, "too many malformed messages");
        }
    }

    /// Spend one token of the player's event budget.
    fn take_event_token(&self, id: u16) -> bool {
        self.state
            .borrow_mut()
            .players
            .get_mut(&id)
            .is_some_and(|p| p.event_bucket.take())
    }

    fn on_control(&self, id: u16, text: &str) {
        let Some(mut msg) = decode_control(text) else {
            return self.strike(id, "invalid json");
        };
        match msg["t"].as_str().unwrap_or_default() {
            "event" => {
                if !self.take_event_token(id) {
                    self.kick(id, "event rate limit");
                    return;
                }
                let name = match msg.remove("name") {
                    Some(Value::String(name)) if valid_event_name(&name) => name,
                    _ => return self.strike(id, "bad event name"),
                };
                let args = match msg.remove("args") {
                    None => Vec::new(),
                    Some(Value::Array(args)) => args,
                    Some(_) => return self.strike(id, "event args not an array"),
                };
                self.call_handler("net_event", |h| h.net_event(id, &name, args));
            }
            "ping" => {
                let Some(Value::Number(ts)) = msg.get("ts") else {
                    return self.strike(id, "bad ping");
                };
                // Shares the event budget, but running dry only drops the reply.
                if self.take_event_token(id) {
                    // Echoed digit for digit, so the client gets back exactly what it sent.
                    let pong = format!(r#"{{"t":"pong","ts":{ts}}}"#);
                    self.send_player(id, Message::Text(Utf8Bytes::from(pong)));
                }
            }
            other => {
                let shown: String = other.chars().take(32).collect();
                self.strike(id, &format!("unexpected message type \"{shown}\""));
            }
        }
    }

    fn on_state(&self, id: u16, data: &[u8]) {
        {
            let mut st = self.state.borrow_mut();
            let Some(p) = st.players.get_mut(&id) else {
                return;
            };
            if !p.state_bucket.take() {
                return; // flooding: drop silently
            }
            if let Some((seq, state)) = decode_state(data) {
                // Serial-number arithmetic: "newer" means less than half the uint32
                // range ahead, which keeps working across the 0xFFFFFFFF -> 0 wrap.
                let ahead = seq.wrapping_sub(p.last_seq);
                if p.has_state && (ahead == 0 || ahead >= 0x8000_0000) {
                    return; // duplicate or stale
                }
                p.last_seq = seq;
                p.state = state;
                p.has_state = true;
                return;
            }
        }
        self.strike(id, "bad state frame");
    }

    // ------------------------------------------------------ heartbeat and tick

    async fn heartbeat_loop(self: Rc<Self>) {
        loop {
            sleep(self.limits.heartbeat_interval).await;
            if !self.running.get() {
                return;
            }
            let now = Instant::now();
            let mut silent = Vec::new();
            for p in self.state.borrow_mut().players.values_mut() {
                if now.duration_since(p.last_seen) > self.limits.heartbeat_timeout {
                    silent.push(p.id);
                } else {
                    p.send_ping();
                }
            }
            for id in silent {
                self.send_player(
                    id,
                    text_frame(&Msg::Kick {
                        reason: "timed out".to_string(),
                    }),
                );
                self.drop_player(id, "timed out", true);
            }
        }
    }

    /// Deadlines advance by exactly one interval each, so timer lateness does not accumulate.
    async fn tick_loop(self: Rc<Self>) {
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.tick_rate));
        let mut last = Instant::now();
        let mut due = last + interval;
        loop {
            sleep_until(due).await;
            if !self.running.get() {
                return;
            }
            let now = Instant::now();
            let dt_ms = now.duration_since(last).as_secs_f64() * 1000.0;
            last = now;
            let tick = self.tick.get() + 1;
            self.tick.set(tick);
            self.call_handler("tick", |h| h.tick(tick, dt_ms));
            self.send_snapshots(tick);

            due += interval;
            let now = Instant::now();
            if now.saturating_duration_since(due) > interval * 5 {
                // Stalled badly (debugger, suspended machine): resynchronise instead
                // of firing a burst of catch-up ticks.
                due = now;
            }
        }
    }

    fn send_snapshots(&self, tick: u64) {
        let mut st = self.state.borrow_mut();
        let lone = st.players.len() < 2;
        // A lone player sees nobody, but may still need the one empty snapshot
        // that despawns a neighbour who just left.
        let visible: Vec<(u16, EntityState)> = if lone {
            Vec::new()
        } else {
            st.players
                .values()
                .filter(|p| p.has_state)
                .map(|p| (p.id, p.state))
                .collect()
        };
        let unlimited = self.stream_distance <= 0.0; // never NaN: checked in `Server::new`
        let max_sq = f64::from(self.stream_distance) * f64::from(self.stream_distance);

        let mut entities: Vec<(u16, &EntityState)> = Vec::with_capacity(visible.len());
        for p in st.players.values_mut() {
            entities.clear();
            // With culling on, a player who has not reported a position yet is
            // not "anywhere", so nothing is in range of them.
            if unlimited || p.has_state {
                let me = p.state;
                entities.extend(visible.iter().filter(|(id, _)| *id != p.id).filter_map(
                    |(id, other)| {
                        let (dx, dy, dz) = (
                            f64::from(other.x) - f64::from(me.x),
                            f64::from(other.y) - f64::from(me.y),
                            f64::from(other.z) - f64::from(me.z),
                        );
                        (unlimited || dx * dx + dy * dy + dz * dz <= max_sq).then_some((*id, other))
                    },
                ));
            }
            if entities.is_empty() && p.last_snapshot_count == 0 {
                continue;
            }
            // A client that is not keeping up gets fewer snapshots, not a longer queue.
            if p.conn.queued.get() > SNAPSHOT_BACKLOG_LIMIT {
                continue;
            }
            let frame = encode_snapshot(tick as u32, entities.iter().copied());
            if p.conn.enqueue(Message::Binary(frame.into())).is_ok() {
                p.last_snapshot_count = entities.len();
            }
        }
    }

    // ------------------------------------------------------------------- http

    async fn accept_loop(self: Rc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, remote)) => {
                    let _ = stream.set_nodelay(true);
                    tokio::task::spawn_local(self.clone().serve_http(stream, remote));
                }
                Err(err) => {
                    // Usually out of file descriptors: back off instead of spinning.
                    self.log.error(format!("accept failed: {err}"));
                    sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    async fn serve_http(self: Rc<Self>, stream: TcpStream, remote: SocketAddr) {
        let _guard = SocketGuard::new(&self);
        let mut shutdown = self.shutdown.subscribe();
        let inner = self.clone();
        let service = service_fn(move |req: Request<Incoming>| {
            let response = inner.handle_request(req, remote);
            async move { Ok::<_, Infallible>(response) }
        });
        let served = self
            .http
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades();
        tokio::select! {
            result = served => {
                if let Err(err) = result {
                    self.log.debug(format!("http connection from {remote} failed: {err}"));
                }
            }
            _ = shutdown.changed() => {}
        }
    }

    fn handle_request(self: &Rc<Self>, req: Request<Incoming>, remote: SocketAddr) -> HttpResponse {
        let wants_upgrade = req.headers().contains_key(header::UPGRADE)
            && header_has_token(req.headers().get(header::CONNECTION), "upgrade");
        if wants_upgrade {
            return self.handle_upgrade(req, remote);
        }

        let head = req.method() == Method::HEAD;
        if req.method() != Method::GET && !head {
            let mut res = json_response(
                StatusCode::METHOD_NOT_ALLOWED,
                &ErrorJson {
                    error: "method not allowed",
                },
            );
            res.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return res;
        }
        let st = self.state.borrow();
        match req.uri().path() {
            "/info.json" => json_response(
                StatusCode::OK,
                &InfoJson {
                    server: "SixM",
                    name: &self.name,
                    build: &self.build,
                    protocol: PROTOCOL_VERSION,
                    max_players: self.max_players,
                    tick_rate: self.tick_rate,
                    tags: &self.tags,
                    vars: &st.vars,
                    resources: &st.resource_names,
                },
            ),
            "/players.json" => json_response(StatusCode::OK, &public_players(&st)),
            "/dynamic.json" => json_response(
                StatusCode::OK,
                &DynamicJson {
                    name: &self.name,
                    clients: st.players.len(),
                    max_clients: self.max_players,
                    uptime: st.started_at.map_or(0, |at| at.elapsed().as_secs()),
                },
            ),
            "/" | "/index.html" => {
                let mut res = response(
                    StatusCode::OK,
                    "text/html; charset=utf-8",
                    self.status_page(&st),
                );
                let headers = res.headers_mut();
                headers.insert(
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static(STATUS_PAGE_CSP),
                );
                headers.insert(
                    header::REFERRER_POLICY,
                    HeaderValue::from_static("no-referrer"),
                );
                res
            }
            _ => json_response(StatusCode::NOT_FOUND, &ErrorJson { error: "not found" }),
        }
    }

    /// Answer a WebSocket upgrade request: either a refusal, or the 101 with the
    /// connection already registered and its task started.
    fn handle_upgrade(
        self: &Rc<Self>,
        mut req: Request<Incoming>,
        remote: SocketAddr,
    ) -> HttpResponse {
        let path = req.uri().path();
        if !self.running.get() {
            return refusal(StatusCode::SERVICE_UNAVAILABLE);
        }
        if path != "/" && path != "/ws" {
            return refusal(StatusCode::NOT_FOUND);
        }
        if self.state.borrow().pending_count >= self.limits.max_pending {
            return refusal(StatusCode::SERVICE_UNAVAILABLE);
        }
        if req.method() != Method::GET {
            return refusal(StatusCode::METHOD_NOT_ALLOWED);
        }
        let headers = req.headers();
        let is_websocket = headers
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("websocket"));
        let version_ok = headers
            .get(header::SEC_WEBSOCKET_VERSION)
            .is_some_and(|v| v.as_bytes() == b"13");
        let accept = headers
            .get(header::SEC_WEBSOCKET_KEY)
            .filter(|key| key.len() == 24)
            .and_then(|key| HeaderValue::from_str(&derive_accept_key(key.as_bytes())).ok());
        let (true, true, Some(accept)) = (is_websocket, version_ok, accept) else {
            return refusal(StatusCode::BAD_REQUEST);
        };

        // An IPv4 client on a dual-stack socket shows up as ::ffff:a.b.c.d.
        let ip = remote.ip().to_canonical();
        let endpoint = SocketAddr::new(ip, remote.port()).to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = {
            let mut st = self.state.borrow_mut();
            st.next_conn_id += 1;
            st.pending_count += 1;
            let conn = Rc::new(Conn {
                id: st.next_conn_id,
                endpoint,
                identifiers: vec![format!("ip:{ip}")],
                stage: Cell::new(Stage::Hello),
                player: Cell::new(None),
                pending: Cell::new(true),
                tx,
                queued: Cell::new(0),
                deadline: Cell::new(Some(Instant::now() + self.limits.hello_timeout)),
                terminated: Cell::new(false),
                wake: Notify::new(),
            });
            st.conns.insert(conn.id, conn.clone());
            conn
        };
        tokio::task::spawn_local(self.clone().run_connection(
            conn,
            rx,
            hyper::upgrade::on(&mut req),
        ));

        let mut res = Response::new(Full::default());
        *res.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let headers = res.headers_mut();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
        res
    }

    fn status_page(&self, st: &State) -> String {
        let title = escape_html(&self.name);
        let players = public_players(st);
        let rows = if players.is_empty() {
            "<tr><td colspan=\"3\" class=\"empty\">No players online</td></tr>".to_string()
        } else {
            players
                .iter()
                .map(|p| {
                    format!(
                        "<tr><td>{}</td><td>{}</td><td>{} ms</td></tr>",
                        p.id,
                        escape_html(p.name),
                        p.ping
                    )
                })
                .collect()
        };
        let max = self.max_players;
        // The script in the page contains no interpolated text besides `max`;
        // everything it renders goes through textContent.
        [
            STATUS_PAGE[0],
            &title,
            STATUS_PAGE[1],
            &title,
            STATUS_PAGE[2],
            &format!("{} / {max}", players.len()),
            STATUS_PAGE[3],
            &rows,
            STATUS_PAGE[4],
            &escape_html(&self.build),
            STATUS_PAGE[5],
            &PROTOCOL_VERSION.to_string(),
            STATUS_PAGE[6],
            &max.to_string(),
            STATUS_PAGE[7],
        ]
        .concat()
    }
}

/// `None` (never ready) while no deferral is outstanding.
async fn wait_for(
    deferral: &mut Option<Deferral>,
) -> Option<Result<Result<Option<String>, oneshot::error::RecvError>, tokio::time::error::Elapsed>>
{
    match deferral {
        Some((_, answer)) => Some(answer.as_mut().await),
        None => std::future::pending().await,
    }
}

/// Ids are handed out from 1; `players` is ordered, so the first gap is the lowest free one.
fn lowest_free_id(players: &BTreeMap<u16, Player>, max_players: u32) -> Option<u16> {
    let mut free: u32 = 1;
    for id in players.keys() {
        if u32::from(*id) != free {
            break;
        }
        free += 1;
    }
    u16::try_from(free).ok().filter(|_| free <= max_players)
}

/// The reason sent for a rejection that came without one.
fn rejection(reason: &str) -> &str {
    if reason.is_empty() {
        "connection rejected"
    } else {
        reason
    }
}

/// Writes queued frames to the socket, batching whatever is available between
/// flushes. Returns true when it stopped after sending a close frame, i.e. the
/// peer's side of the close handshake is still worth reading.
async fn write_loop(
    mut sink: SplitSink<WsStream, Message>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    conn: Rc<Conn>,
) -> bool {
    while let Some(mut next) = rx.recv().await {
        loop {
            let Outgoing::Frame(frame) = next else {
                let _ = sink.flush().await;
                return false;
            };
            let len = frame.len();
            let closing = matches!(frame, Message::Close(_));
            let fed = sink.feed(frame).await;
            conn.queued.set(conn.queued.get().saturating_sub(len));
            if fed.is_err() {
                return false;
            }
            if closing {
                return sink.flush().await.is_ok();
            }
            match rx.try_recv() {
                Ok(more) => next = more,
                Err(_) => break,
            }
        }
        if sink.flush().await.is_err() {
            return false;
        }
    }
    false
}

// ------------------------------------------------------------- http responses

const STATUS_PAGE_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                               connect-src 'self'; base-uri 'none'; form-action 'none'";

#[derive(Serialize)]
struct ErrorJson {
    error: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoJson<'a> {
    server: &'static str,
    name: &'a str,
    build: &'a str,
    protocol: u32,
    max_players: u32,
    tick_rate: u32,
    tags: &'a [String],
    vars: &'a BTreeMap<String, String>,
    resources: &'a [String],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DynamicJson<'a> {
    name: &'a str,
    clients: usize,
    max_clients: u32,
    uptime: u64,
}

/// Deliberately no endpoints or identifiers: this list is public.
#[derive(Serialize)]
struct PublicPlayer<'a> {
    id: u16,
    name: &'a str,
    ping: i32,
}

fn public_players(st: &State) -> Vec<PublicPlayer<'_>> {
    st.players
        .values()
        .map(|p| PublicPlayer {
            id: p.id,
            name: &p.name,
            ping: p.ping,
        })
        .collect()
}

/// A response with the headers every HTTP answer carries. hyper adds
/// `Content-Length` and leaves the body out of replies to HEAD.
fn response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> HttpResponse {
    let mut res = Response::new(Full::new(body.into()));
    *res.status_mut() = status;
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res
}

fn json_response(status: StatusCode, body: &impl Serialize) -> HttpResponse {
    // These bodies are plain structs of strings and numbers; serialising them cannot fail.
    let payload = serde_json::to_vec(body).unwrap_or_default();
    let mut res = response(status, "application/json; charset=utf-8", payload);
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res
}

/// An upgrade request is refused with a bare status and the connection closed.
fn refusal(status: StatusCode) -> HttpResponse {
    let mut res = Response::new(Full::default());
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    res
}

fn header_has_token(value: Option<&HeaderValue>, token: &str) -> bool {
    value.and_then(|v| v.to_str().ok()).is_some_and(|v| {
        v.split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(token))
    })
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The status page, cut at the places where a value is inserted: title, title,
/// player count, table rows, build, protocol version, max players.
const STATUS_PAGE: [&str; 8] = [
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>"#,
    r#"</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; background: #0e1116; color: #e6e9ef; font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
  main { max-width: 640px; margin: 0 auto; padding: 48px 16px; }
  h1 { margin: 0 0 4px; font-size: 24px; font-weight: 600; overflow-wrap: anywhere; }
  .meta { color: #8b93a1; font-size: 13px; margin-bottom: 24px; }
  .count { display: inline-block; padding: 2px 10px; border-radius: 999px; background: #17202e; color: #7cc4ff; font-variant-numeric: tabular-nums; }
  table { width: 100%; border-collapse: collapse; background: #151a22; border: 1px solid #232a35; border-radius: 8px; overflow: hidden; }
  th, td { text-align: left; padding: 9px 14px; border-bottom: 1px solid #232a35; overflow-wrap: anywhere; }
  th { font-size: 12px; font-weight: 600; text-transform: uppercase; letter-spacing: .05em; color: #8b93a1; background: #181e28; }
  tr:last-child td { border-bottom: 0; }
  td:first-child, th:first-child { width: 56px; }
  td:last-child, th:last-child { width: 96px; text-align: right; font-variant-numeric: tabular-nums; }
  .empty { text-align: center !important; color: #8b93a1; padding: 22px; }
  footer { margin-top: 20px; color: #5d6573; font-size: 12px; }
</style>
</head>
<body>
<main>
  <h1>"#,
    r#"</h1>
  <div class="meta">Players <span class="count" id="count">"#,
    r#"</span></div>
  <table>
    <thead><tr><th>ID</th><th>Name</th><th>Ping</th></tr></thead>
    <tbody id="players">"#,
    r#"</tbody>
  </table>
  <footer>SixM "#,
    r#" &middot; protocol "#,
    r#"</footer>
</main>
<script>
(function () {
  var max = "#,
    r#";
  var body = document.getElementById('players');
  var count = document.getElementById('count');
  function cell(row, text, cls) {
    var td = document.createElement('td');
    td.textContent = text;
    if (cls) td.className = cls;
    row.appendChild(td);
    return td;
  }
  function render(list) {
    count.textContent = list.length + ' / ' + max;
    body.textContent = '';
    if (!list.length) {
      var empty = document.createElement('tr');
      cell(empty, 'No players online', 'empty').colSpan = 3;
      body.appendChild(empty);
      return;
    }
    list.forEach(function (p) {
      var row = document.createElement('tr');
      cell(row, String(p.id));
      cell(row, String(p.name));
      cell(row, String(p.ping) + ' ms');
      body.appendChild(row);
    });
  }
  function refresh() {
    fetch('players.json', { cache: 'no-store' })
      .then(function (r) { return r.json(); })
      .then(function (list) { if (Array.isArray(list)) render(list); })
      .catch(function () {});
  }
  setInterval(refresh, 5000);
})();
</script>
</body>
</html>
"#,
];
