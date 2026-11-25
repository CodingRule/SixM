//! SixM client core. It is headless: it speaks the protocol, tracks the other
//! players, and runs the client side of the server's resources. A game
//! integration would feed it the local player's state and read remote players
//! from it; without one, the state is set by scripts or by the caller.
//!
//! Everything here is single-threaded and must run inside a tokio `LocalSet`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value as Json;
use sixm_lua::mlua::{self, Lua, Value, Variadic};
use sixm_lua::{lua_to_json, Host, LuaRuntime, Side};
use sixm_protocol::command::split_command;
use sixm_protocol::{
    decode_snapshot, encode_state, sanitize_name, ClientResource, EntityState, Msg, COMMAND_EVENT,
    PROTOCOL_VERSION,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub use sixm_protocol::DEFAULT_PORT;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(2);
const MAX_COMMAND_LENGTH: usize = 512;

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub build: String,
    /// Connect with TLS (`wss://`), for servers behind a TLS reverse proxy.
    pub secure: bool,
    /// Run the server's client-side Lua.
    pub run_scripts: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            host: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            name: "Player".to_string(),
            build: format!("sixm-client/{VERSION}"),
            secure: false,
            run_scripts: true,
        }
    }
}

/// What the embedder is told. Every method has a default no-op.
pub trait ClientListener {
    /// Script output and script errors, already prefixed with the resource name.
    fn print(&self, _line: &str) {}
    fn player_joined(&self, _id: u16, _name: &str) {}
    fn player_left(&self, _id: u16, _name: &str, _reason: &str) {}
    /// Called once per client tick, before the local state is sent.
    fn tick(&self, _client: &Client) {}
    /// The connection ended, for whatever reason.
    fn disconnected(&self, _reason: &str) {}
}

/// Another player, as this client knows them.
#[derive(Clone, Debug, PartialEq)]
pub struct RemotePlayer {
    pub id: u16,
    pub name: String,
    /// `None` while the player is out of streaming range.
    pub state: Option<EntityState>,
}

struct Inner {
    options: ClientOptions,
    listener: Rc<dyn ClientListener>,
    id: Cell<u16>,
    name: RefCell<String>,
    server_name: RefCell<String>,
    tick_rate: Cell<u32>,
    ping: Cell<i32>,
    state: Cell<EntityState>,
    dirty: Cell<bool>,
    seq: Cell<u32>,
    connected: Cell<bool>,
    torn_down: Cell<bool>,
    kick_reason: RefCell<Option<String>>,
    players: RefCell<BTreeMap<u16, RemotePlayer>>,
    runtimes: RefCell<Vec<(String, Rc<LuaRuntime>)>>,
    convars: RefCell<BTreeMap<String, String>>,
    outgoing: mpsc::UnboundedSender<Message>,
    started_at: Instant,
}

/// Cheap handle; clones share one connection.
#[derive(Clone)]
pub struct Client {
    inner: Rc<Inner>,
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

impl Client {
    /// Connect and finish the handshake. Fails with the server's reason when refused.
    pub async fn connect(
        options: ClientOptions,
        listener: Rc<dyn ClientListener>,
    ) -> Result<Client, String> {
        let name = sanitize_name(&options.name);
        let name = if name.is_empty() {
            "Player".to_string()
        } else {
            name
        };
        let scheme = if options.secure { "wss" } else { "ws" };
        let url = format!("{scheme}://{}:{}/ws", options.host, options.port);

        let handshake = async {
            let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
                .await
                .map_err(|e| e.to_string())?;
            let (mut sink, mut stream) = socket.split();
            let hello = Msg::Hello {
                proto: PROTOCOL_VERSION,
                name: name.clone(),
                build: Some(options.build.clone()),
                token: None,
            };
            sink.send(Message::text(hello.encode()))
                .await
                .map_err(|e| e.to_string())?;
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => match Msg::decode(text.as_str()) {
                        Some(welcome @ Msg::Welcome { .. }) => return Ok((sink, stream, welcome)),
                        Some(Msg::Reject { reason }) => return Err(reason),
                        _ => {}
                    },
                    Some(Ok(Message::Close(_))) | None => {
                        return Err("Connection closed before the server answered.".to_string())
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                }
            }
        };
        let (mut sink, mut stream, welcome) = tokio::time::timeout(CONNECT_TIMEOUT, handshake)
            .await
            .map_err(|_| "The server did not answer in time.".to_string())??;

        let (outgoing, mut queue) = mpsc::unbounded_channel::<Message>();
        let inner = Rc::new(Inner {
            options,
            listener,
            id: Cell::new(0),
            name: RefCell::new(name),
            server_name: RefCell::new(String::new()),
            tick_rate: Cell::new(20),
            ping: Cell::new(-1),
            state: Cell::new(EntityState::default()),
            dirty: Cell::new(true),
            seq: Cell::new(0),
            connected: Cell::new(true),
            torn_down: Cell::new(false),
            kick_reason: RefCell::new(None),
            players: RefCell::new(BTreeMap::new()),
            runtimes: RefCell::new(Vec::new()),
            convars: RefCell::new(BTreeMap::new()),
            outgoing,
            started_at: Instant::now(),
        });
        let client = Client { inner };

        // Writer: everything the client sends goes through one queue.
        tokio::task::spawn_local(async move {
            while let Some(message) = queue.recv().await {
                let closing = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() || closing {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        client.on_welcome(welcome);

        // Reader.
        let reader = client.clone();
        tokio::task::spawn_local(async move {
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(Message::Text(text)) => {
                        if let Some(msg) = Msg::decode(text.as_str()) {
                            reader.on_message(msg);
                        }
                    }
                    Ok(Message::Binary(data)) => reader.on_binary(&data),
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let reason = reader
                .inner
                .kick_reason
                .borrow_mut()
                .take()
                .unwrap_or_else(|| "Connection lost.".to_string());
            reader.teardown(&reason);
        });

        // Tick: run scripts and send the local state when it changed.
        let ticker = client.clone();
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(Duration::from_secs_f64(
                1.0 / f64::from(ticker.inner.tick_rate.get()),
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_ping = Instant::now();
            while ticker.inner.connected.get() {
                interval.tick().await;
                if !ticker.inner.connected.get() {
                    break;
                }
                ticker.tick();
                if last_ping.elapsed() >= PING_INTERVAL {
                    last_ping = Instant::now();
                    ticker.send(&Msg::Ping { ts: now_ms() });
                }
            }
        });

        client.trigger_event("onClientReady", &[Json::from(client.id())], 0, false);
        Ok(client)
    }

    /// Leave the server.
    pub fn disconnect(&self, reason: &str) {
        self.inner
            .kick_reason
            .borrow_mut()
            .get_or_insert_with(|| reason.to_string());
        if self.inner.outgoing.send(Message::Close(None)).is_err() {
            self.teardown(reason);
        }
    }

    // ---- what the embedder reads ---------------------------------------

    pub fn id(&self) -> u16 {
        self.inner.id.get()
    }

    pub fn name(&self) -> String {
        self.inner.name.borrow().clone()
    }

    pub fn server_name(&self) -> String {
        self.inner.server_name.borrow().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.get()
    }

    /// Round trip to the server in milliseconds, or -1 before the first measurement.
    pub fn ping(&self) -> i32 {
        self.inner.ping.get()
    }

    pub fn state(&self) -> EntityState {
        self.inner.state.get()
    }

    /// The other players, by id.
    pub fn players(&self) -> Vec<RemotePlayer> {
        self.inner.players.borrow().values().cloned().collect()
    }

    pub fn player(&self, id: u16) -> Option<RemotePlayer> {
        self.inner.players.borrow().get(&id).cloned()
    }

    // ---- local state ----------------------------------------------------

    pub fn set_coords(&self, x: f32, y: f32, z: f32) {
        self.update_state(|s| {
            s.x = x;
            s.y = y;
            s.z = z;
        });
    }

    pub fn set_heading(&self, heading: f32) {
        self.update_state(|s| s.heading = heading);
    }

    /// Change the local state; it is sent on the next tick.
    pub fn update_state(&self, change: impl FnOnce(&mut EntityState)) {
        let mut state = self.inner.state.get();
        change(&mut state);
        if state.is_sane() {
            self.inner.state.set(state);
            self.inner.dirty.set(true);
        }
    }

    // ---- events and commands -------------------------------------------

    /// Send an event to the server (Lua: `TriggerServerEvent`).
    pub fn emit_net(&self, name: &str, args: Vec<Json>) {
        self.send(&Msg::Event {
            name: name.to_string(),
            args,
            src: None,
        });
    }

    /// Deliver an event to every client-side resource. Returns how many handlers ran.
    pub fn trigger_event(&self, name: &str, args: &[Json], source: i64, from_net: bool) -> usize {
        self.runtimes()
            .iter()
            .map(|rt| rt.trigger_event(name, args, source, from_net))
            .sum()
    }

    /// Run a command: a client resource's if one owns it, otherwise the server's.
    pub fn execute_command(&self, raw: &str) {
        let parts = split_command(raw);
        let Some((name, args)) = parts.split_first() else {
            return;
        };
        for runtime in self.runtimes() {
            if runtime.has_command(name) {
                runtime.run_command(name, i64::from(self.id()), args, raw);
                return;
            }
        }
        let line: String = raw.chars().take(MAX_COMMAND_LENGTH).collect();
        self.emit_net(COMMAND_EVENT, vec![Json::from(line)]);
    }

    // ---- protocol -------------------------------------------------------

    fn send(&self, msg: &Msg) {
        if self.inner.connected.get() {
            let _ = self.inner.outgoing.send(Message::text(msg.encode()));
        }
    }

    /// The runtimes, cloned so no borrow is held while Lua runs.
    fn runtimes(&self) -> Vec<Rc<LuaRuntime>> {
        self.inner
            .runtimes
            .borrow()
            .iter()
            .map(|(_, rt)| rt.clone())
            .collect()
    }

    fn on_welcome(&self, welcome: Msg) {
        let Msg::Welcome {
            id,
            name,
            server_name,
            tick_rate,
            players,
            resources,
            ..
        } = welcome
        else {
            return;
        };
        let inner = &self.inner;
        inner.id.set(id);
        if !name.is_empty() {
            *inner.name.borrow_mut() = name;
        }
        *inner.server_name.borrow_mut() = server_name;
        inner.tick_rate.set(tick_rate.clamp(1, 120));
        {
            let mut known = inner.players.borrow_mut();
            for p in players {
                if p.id != id {
                    known.insert(
                        p.id,
                        RemotePlayer {
                            id: p.id,
                            name: p.name,
                            state: None,
                        },
                    );
                }
            }
        }
        if inner.options.run_scripts {
            for resource in resources {
                self.start_resource(resource);
            }
        }
    }

    fn on_message(&self, msg: Msg) {
        let inner = &self.inner;
        match msg {
            Msg::Join { id, name } if id != self.id() => {
                inner.players.borrow_mut().insert(
                    id,
                    RemotePlayer {
                        id,
                        name: name.clone(),
                        state: None,
                    },
                );
                inner.listener.player_joined(id, &name);
                self.trigger_event(
                    "playerJoined",
                    &[Json::from(id), Json::from(name)],
                    i64::from(id),
                    false,
                );
            }
            Msg::Leave { id, reason } => {
                let Some(player) = inner.players.borrow_mut().remove(&id) else {
                    return;
                };
                inner.listener.player_left(id, &player.name, &reason);
                let args = [Json::from(id), Json::from(player.name), Json::from(reason)];
                self.trigger_event("playerLeft", &args, i64::from(id), false);
            }
            Msg::Event { name, args, .. } => {
                self.trigger_event(&name, &args, 0, true);
            }
            Msg::Pong { ts } => inner.ping.set((now_ms() - ts).max(0.0) as i32),
            Msg::Kick { reason } => {
                *inner.kick_reason.borrow_mut() = Some(if reason.is_empty() {
                    "Kicked.".to_string()
                } else {
                    reason
                });
            }
            _ => {}
        }
    }

    fn on_binary(&self, data: &[u8]) {
        let Some((_tick, entities)) = decode_snapshot(data) else {
            return;
        };
        let mut players = self.inner.players.borrow_mut();
        // Anyone missing from the snapshot is out of streaming range.
        for player in players.values_mut() {
            player.state = None;
        }
        for (id, state) in entities {
            if let Some(player) = players.get_mut(&id) {
                player.state = Some(state);
            }
        }
    }

    fn tick(&self) {
        let now = now_ms();
        for runtime in self.runtimes() {
            runtime.tick(now);
        }
        self.inner.listener.tick(self);
        if self.inner.dirty.replace(false) && self.inner.connected.get() {
            let seq = self.inner.seq.get().wrapping_add(1);
            self.inner.seq.set(seq);
            let _ = self
                .inner
                .outgoing
                .send(Message::binary(encode_state(seq, &self.inner.state.get())));
        }
    }

    fn teardown(&self, reason: &str) {
        let inner = &self.inner;
        if inner.torn_down.replace(true) {
            return;
        }
        inner.connected.set(false);
        let runtimes = std::mem::take(&mut *inner.runtimes.borrow_mut());
        for (_, runtime) in runtimes {
            runtime.destroy();
        }
        inner.players.borrow_mut().clear();
        inner.listener.disconnected(reason);
    }

    // ---- client-side Lua -----------------------------------------------

    fn start_resource(&self, resource: ClientResource) {
        let name = resource.name;
        if self.inner.runtimes.borrow().iter().any(|(n, _)| *n == name) {
            return;
        }
        let host = Rc::new(ScriptHost {
            client: Rc::downgrade(&self.inner),
            resource: name.clone(),
        });
        let runtime = match LuaRuntime::new(&name, Side::Client, host) {
            Ok(runtime) => Rc::new(runtime),
            Err(message) => {
                self.inner
                    .listener
                    .print(&format!("[{name}] could not start: {message}"));
                return;
            }
        };
        if let Err(e) = register_natives(&self.inner, runtime.lua()) {
            self.inner
                .listener
                .print(&format!("[{name}] could not start: {e}"));
            return;
        }
        self.inner
            .runtimes
            .borrow_mut()
            .push((name.clone(), runtime.clone()));
        for script in &resource.scripts {
            runtime.load_script(&script.code, &format!("@{name}/{}", script.file));
        }
        self.trigger_event("onClientResourceStart", &[Json::from(name)], 0, false);
    }
}

/// What a client-side Lua runtime asks of the client.
struct ScriptHost {
    client: Weak<Inner>,
    resource: String,
}

impl Host for ScriptHost {
    fn print(&self, line: &str) {
        if let Some(inner) = self.client.upgrade() {
            inner.listener.print(&format!("[{}] {line}", self.resource));
        }
    }

    fn error(&self, context: &str, message: &str) {
        if let Some(inner) = self.client.upgrade() {
            inner.listener.print(&format!(
                "[{}] script error in {context}: {message}",
                self.resource
            ));
        }
    }

    fn trigger_event(&self, name: &str, args: Vec<Json>) {
        if let Some(inner) = self.client.upgrade() {
            Client { inner }.trigger_event(name, &args, 0, false);
        }
    }

    fn call_export(&self, resource: &str, export: &str, args: Vec<Json>) -> Result<Json, String> {
        let inner = self.client.upgrade().ok_or("the client is shutting down")?;
        let target = inner
            .runtimes
            .borrow()
            .iter()
            .find(|(n, _)| n == resource)
            .map(|(_, rt)| rt.clone());
        match target {
            Some(rt) if rt.has_export(export) => rt.call_export(export, &args),
            _ => Err(format!("no export \"{export}\" in resource \"{resource}\"")),
        }
    }

    fn now_ms(&self) -> f64 {
        now_ms()
    }
}

/// A player id as scripts pass it: a number, or a string holding one.
fn player_id(value: &Value) -> Option<u16> {
    let n = match value {
        Value::Integer(i) => *i,
        Value::Number(n) if n.fract() == 0.0 => *n as i64,
        Value::String(s) => s.to_str().ok()?.trim().parse().ok()?,
        _ => return None,
    };
    u16::try_from(n).ok()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn coords(lua: &Lua, state: Option<EntityState>) -> mlua::Result<Value> {
    let Some(s) = state else {
        return Ok(Value::Nil);
    };
    let table = lua.create_table()?;
    table.set("x", s.x)?;
    table.set("y", s.y)?;
    table.set("z", s.z)?;
    Ok(Value::Table(table))
}

impl Inner {
    /// The state of a player as this client sees it: its own, or a streamed remote one.
    fn state_of(&self, id: Option<u16>) -> Option<EntityState> {
        let id = id?;
        if id == self.id.get() {
            Some(self.state.get())
        } else {
            self.players.borrow().get(&id).and_then(|p| p.state)
        }
    }
}

/// The functions client-side scripts can call.
fn register_natives(inner: &Rc<Inner>, lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let weak = Rc::downgrade(inner);
    macro_rules! native {
        ($name:literal, |$c:ident, $lua:ident, $args:pat_param, $ty:ty| $body:expr) => {{
            let weak = weak.clone();
            globals.set(
                $name,
                lua.create_function(move |$lua, $args: $ty| {
                    let $c = weak.upgrade().ok_or_else(|| mlua::Error::runtime("the client is shutting down"))?;
                    // Bound first so borrows inside the body end before the manager handle is dropped.
                    let result = $body;
                    result
                })?,
            )?;
        }};
    }

    native!("TriggerServerEvent", |c, _lua, (name, args), (Value, Variadic<Value>)| {
        let Value::String(name) = name else {
            return Err(mlua::Error::runtime("TriggerServerEvent: event name must be a string"));
        };
        let args = args.iter().map(lua_to_json).collect::<Result<Vec<_>, _>>().map_err(mlua::Error::runtime)?;
        Client { inner: c }.emit_net(&name.to_string_lossy(), args);
        Ok(())
    });
    native!("GetLocalPlayerId", |c, _lua, (), ()| Ok(c.id.get()));
    native!("GetLocalCoords", |c, lua, (), ()| coords(
        lua,
        Some(c.state.get())
    ));
    native!("GetLocalHeading", |c, _lua, (), ()| Ok(c
        .state
        .get()
        .heading));
    native!("SetLocalCoords", |c, _lua, (x, y, z), (f32, f32, f32)| {
        Client { inner: c }.set_coords(x, y, z);
        Ok(())
    });
    native!("SetLocalHeading", |c, _lua, heading, f32| {
        Client { inner: c }.set_heading(heading);
        Ok(())
    });
    native!("GetActivePlayers", |c, _lua, (), ()| {
        let mut ids: Vec<u16> = c.players.borrow().keys().copied().collect();
        ids.push(c.id.get());
        ids.sort_unstable();
        Ok(ids)
    });
    native!("GetPlayerName", |c, _lua, id, Value| {
        let id = player_id(&id);
        Ok(if id == Some(c.id.get()) {
            Some(c.name.borrow().clone())
        } else {
            id.and_then(|id| c.players.borrow().get(&id).map(|p| p.name.clone()))
        })
    });
    native!("GetPlayerCoords", |c, lua, id, Value| coords(
        lua,
        c.state_of(player_id(&id))
    ));
    native!("GetPlayerHeading", |c, _lua, id, Value| Ok(c
        .state_of(player_id(&id))
        .map_or(0.0, |s| s.heading)));
    native!("IsPlayerStreamed", |c, _lua, id, Value| Ok(c
        .state_of(player_id(&id))
        .is_some()));
    native!("GetPing", |c, _lua, (), ()| Ok(c.ping.get()));
    native!("GetServerName", |c, _lua, (), ()| Ok(c
        .server_name
        .borrow()
        .clone()));
    native!("GetConvar", |c, lua, (name, fallback), (Value, Value)| {
        let found = c.convars.borrow().get(&text(&name)).cloned();
        match found {
            Some(value) => Ok(Value::String(lua.create_string(&value)?)),
            None => Ok(fallback),
        }
    });
    native!("GetConvarInt", |c, _lua, (name, fallback), (Value, Option<i64>)| {
        let found = c.convars.borrow().get(&text(&name)).cloned();
        Ok(found.and_then(|v| v.trim().parse::<i64>().ok()).or(fallback).unwrap_or(0))
    });
    native!("ExecuteCommand", |c, _lua, raw, String| {
        Client { inner: c }.execute_command(&raw);
        Ok(())
    });
    native!("GetGameTimer", |c, _lua, (), ()| Ok(
        c.started_at.elapsed().as_millis() as i64
    ));
    Ok(())
}
