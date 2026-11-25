//! Resource manager: finds resources on disk, runs their server-side Lua,
//! hands their client-side Lua to joining players, and connects every Lua
//! runtime to the server (events, players, commands, convars, exports).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value as Json;
use sixm_lua::mlua::{self, Lua, Value, Variadic};
use sixm_lua::{lua_to_json, Host, LuaRuntime, Side};
use sixm_protocol::command::split_command;
use sixm_protocol::{ClientResource, ClientScript, COMMAND_EVENT};
use tokio::sync::oneshot;

use crate::core::{ConnectDecision, ConnectInfo, PlayerInfo, Server, ServerHandler};
use crate::logger::Logger;
use crate::manifest::{parse_manifest, MANIFEST_FILE};

const MAX_SCAN_DEPTH: usize = 4;
const MAX_COMMAND_LENGTH: usize = 512;

/// A console command implemented by the host rather than by a resource.
pub type Builtin = Rc<dyn Fn(&[String], &str)>;

struct Resource {
    name: String,
    runtime: LuaRuntime,
    client_scripts: Vec<ClientScript>,
}

struct Inner {
    server: Server,
    dir: PathBuf,
    log: Logger,
    started_at: Instant,
    convars: RefCell<BTreeMap<String, String>>,
    /// name -> folder, for every resource found on disk
    available: RefCell<BTreeMap<String, PathBuf>>,
    /// started resources, in start order
    running: RefCell<Vec<Rc<Resource>>>,
    builtins: RefCell<BTreeMap<String, (String, Builtin)>>,
    /// The player a `playerDropped` event is about; their slot is already free.
    dropping: RefCell<Option<PlayerInfo>>,
}

/// Cheap handle; clones share one manager.
#[derive(Clone)]
pub struct ResourceManager {
    inner: Rc<Inner>,
}

impl ResourceManager {
    pub fn new(
        server: Server,
        resources_dir: impl Into<PathBuf>,
        log: Logger,
        convars: BTreeMap<String, String>,
    ) -> Self {
        let inner = Rc::new(Inner {
            server: server.clone(),
            dir: resources_dir.into(),
            log,
            started_at: Instant::now(),
            convars: RefCell::new(convars),
            available: RefCell::new(BTreeMap::new()),
            running: RefCell::new(Vec::new()),
            builtins: RefCell::new(BTreeMap::new()),
            dropping: RefCell::new(None),
        });
        server.set_handler(Rc::new(Hooks {
            inner: Rc::downgrade(&inner),
        }));
        ResourceManager { inner }
    }

    /// Find every folder holding a manifest. `[category]` folders are looked into.
    pub fn scan(&self) -> Vec<String> {
        self.inner.scan()
    }

    /// Returns whether the resource is running afterwards.
    pub fn start(&self, name: &str) -> bool {
        Inner::start(&self.inner, name)
    }

    pub fn stop(&self, name: &str) -> bool {
        self.inner.stop(name)
    }

    /// Stop, look at the disk again, and start.
    pub fn restart(&self, name: &str) -> bool {
        self.inner.stop(name);
        self.inner.scan();
        Inner::start(&self.inner, name)
    }

    pub fn stop_all(&self) {
        let names: Vec<String> = self
            .inner
            .running
            .borrow()
            .iter()
            .rev()
            .map(|r| r.name.clone())
            .collect();
        for name in names {
            self.inner.stop(&name);
        }
    }

    pub fn running(&self) -> Vec<String> {
        self.inner
            .running
            .borrow()
            .iter()
            .map(|r| r.name.clone())
            .collect()
    }

    pub fn available(&self) -> Vec<String> {
        self.inner.available.borrow().keys().cloned().collect()
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.inner.find(name).is_some()
    }

    /// Deliver an event to every running resource. Returns how many handlers ran.
    pub fn trigger_event(&self, name: &str, args: &[Json], source: i64, from_net: bool) -> usize {
        self.inner.trigger_event(name, args, source, from_net)
    }

    /// Register a console command implemented in Rust.
    pub fn add_builtin(&self, name: &str, help: &str, run: impl Fn(&[String], &str) + 'static) {
        self.inner
            .builtins
            .borrow_mut()
            .insert(name.to_lowercase(), (help.to_string(), Rc::new(run)));
    }

    /// `(name, help)` of every built-in console command.
    pub fn builtins(&self) -> Vec<(String, String)> {
        self.inner
            .builtins
            .borrow()
            .iter()
            .map(|(n, (h, _))| (n.clone(), h.clone()))
            .collect()
    }

    /// Run a command line. Source 0 is the server console; anything else is a
    /// player, who can only run commands a resource registered as unrestricted.
    /// Returns whether something handled it.
    pub fn execute_command(&self, raw: &str, source: i64) -> bool {
        self.inner.execute_command(raw, source)
    }
}

impl Inner {
    fn find(&self, name: &str) -> Option<Rc<Resource>> {
        self.running
            .borrow()
            .iter()
            .find(|r| r.name == name)
            .cloned()
    }

    /// The running resources, cloned so no borrow is held while Lua runs.
    fn snapshot(&self) -> Vec<Rc<Resource>> {
        self.running.borrow().clone()
    }

    fn scan(&self) -> Vec<String> {
        let mut found = BTreeMap::new();
        self.walk(&self.dir, 0, &mut found);
        let names = found.keys().cloned().collect();
        *self.available.borrow_mut() = found;
        names
    }

    fn walk(&self, dir: &Path, depth: usize, found: &mut BTreeMap<String, PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for path in dirs {
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if name.len() >= 2 && name.starts_with('[') && name.ends_with(']') {
                if depth < MAX_SCAN_DEPTH {
                    self.walk(&path, depth + 1, found);
                }
            } else if path.join(MANIFEST_FILE).is_file() {
                if let Some(first) = found.get(&name) {
                    self.log.warn(format!(
                        "duplicate resource \"{name}\" at {}; keeping {}",
                        path.display(),
                        first.display()
                    ));
                } else {
                    found.insert(name, path);
                }
            }
        }
    }

    fn start(this: &Rc<Inner>, name: &str) -> bool {
        if this.find(name).is_some() {
            return true;
        }
        let Some(dir) = this.available.borrow().get(name).cloned() else {
            this.log.error(format!(
                "cannot start \"{name}\": no such resource in {}",
                this.dir.display()
            ));
            return false;
        };
        let resource = match Inner::load(this, name, &dir) {
            Ok(resource) => resource,
            Err(message) => {
                this.log
                    .error(format!("cannot start \"{name}\": {message}"));
                return false;
            }
        };
        this.sync_server();
        this.trigger_event(
            "onResourceStart",
            &[Json::from(resource.name.as_str())],
            0,
            false,
        );
        this.log.info(format!("started resource {name}"));
        true
    }

    fn load(this: &Rc<Inner>, name: &str, dir: &Path) -> Result<Rc<Resource>, String> {
        let text = std::fs::read_to_string(dir.join(MANIFEST_FILE))
            .map_err(|e| format!("{MANIFEST_FILE}: {e}"))?;
        let manifest = parse_manifest(&text, &format!("@{name}/{MANIFEST_FILE}"))?;

        // Scripts are read up front so a missing file fails the start cleanly.
        let read = |rel: &String| -> Result<ClientScript, String> {
            let code = std::fs::read_to_string(dir.join(rel))
                .map_err(|e| format!("script \"{rel}\": {e}"))?;
            Ok(ClientScript {
                file: rel.clone(),
                code,
            })
        };
        let shared = manifest
            .shared_scripts
            .iter()
            .map(read)
            .collect::<Result<Vec<_>, _>>()?;
        let mut server_scripts = shared.clone();
        for rel in &manifest.server_scripts {
            server_scripts.push(read(rel)?);
        }
        let mut client_scripts = shared;
        for rel in &manifest.client_scripts {
            client_scripts.push(read(rel)?);
        }

        let host = Rc::new(ResourceHost {
            manager: Rc::downgrade(this),
            resource: name.to_string(),
        });
        let runtime = LuaRuntime::new(name, Side::Server, host)?;
        register_natives(this, runtime.lua()).map_err(|e| e.to_string())?;

        let resource = Rc::new(Resource {
            name: name.to_string(),
            runtime,
            client_scripts,
        });
        // Registered before its scripts run, so they can already trigger events and call exports.
        this.running.borrow_mut().push(resource.clone());
        for script in &server_scripts {
            resource
                .runtime
                .load_script(&script.code, &format!("@{name}/{}", script.file));
        }
        Ok(resource)
    }

    fn stop(&self, name: &str) -> bool {
        let Some(resource) = self.find(name) else {
            return false;
        };
        self.trigger_event("onResourceStop", &[Json::from(name)], 0, false);
        self.running.borrow_mut().retain(|r| r.name != name);
        resource.runtime.destroy();
        self.sync_server();
        self.log.info(format!("stopped resource {name}"));
        true
    }

    /// Tell the server what joining clients should run and what `/info.json` lists.
    fn sync_server(&self) {
        let running = self.snapshot();
        self.server
            .set_resource_names(running.iter().map(|r| r.name.clone()).collect());
        self.server.set_client_resources(
            running
                .iter()
                .filter(|r| !r.client_scripts.is_empty())
                .map(|r| ClientResource {
                    name: r.name.clone(),
                    scripts: r.client_scripts.clone(),
                })
                .collect(),
        );
    }

    fn trigger_event(&self, name: &str, args: &[Json], source: i64, from_net: bool) -> usize {
        self.snapshot()
            .iter()
            .map(|r| r.runtime.trigger_event(name, args, source, from_net))
            .sum()
    }

    fn call_export(&self, resource: &str, export: &str, args: &[Json]) -> Result<Json, String> {
        let target = self
            .find(resource)
            .ok_or_else(|| format!("no running resource \"{resource}\""))?;
        if !target.runtime.has_export(export) {
            return Err(format!(
                "resource \"{resource}\" has no export \"{export}\""
            ));
        }
        target.runtime.call_export(export, args)
    }

    fn execute_command(&self, raw: &str, source: i64) -> bool {
        let parts = split_command(raw);
        let Some((name, args)) = parts.split_first() else {
            return false;
        };

        // Resource commands are matched exactly as registered; console built-ins ignore case.
        for resource in self.snapshot() {
            if !resource.runtime.has_command(name) {
                continue;
            }
            if source != 0 && resource.runtime.is_command_restricted(name) {
                self.log.warn(format!(
                    "player {source} was refused restricted command \"{name}\""
                ));
                return false;
            }
            resource.runtime.run_command(name, source, args, raw);
            return true;
        }
        if source == 0 {
            let builtin = self
                .builtins
                .borrow()
                .get(&name.to_lowercase())
                .map(|(_, run)| run.clone());
            if let Some(run) = builtin {
                run(args, raw);
                return true;
            }
        }
        false
    }

    /// A connected player, or the one a `playerDropped` handler is asking about.
    fn player(&self, id: i64) -> Option<PlayerInfo> {
        let id = u16::try_from(id).ok()?;
        self.server.player(id).or_else(|| {
            self.dropping
                .borrow()
                .as_ref()
                .filter(|p| p.id == id)
                .cloned()
        })
    }
}

/// What a resource's Lua runtime asks of the manager.
struct ResourceHost {
    manager: Weak<Inner>,
    resource: String,
}

impl Host for ResourceHost {
    fn print(&self, line: &str) {
        if let Some(m) = self.manager.upgrade() {
            m.log.info(format!("[{}] {line}", self.resource));
        }
    }

    fn error(&self, context: &str, message: &str) {
        if let Some(m) = self.manager.upgrade() {
            m.log
                .error(format!("[{}] {context}: {message}", self.resource));
        }
    }

    fn trigger_event(&self, name: &str, args: Vec<Json>) {
        if let Some(m) = self.manager.upgrade() {
            m.trigger_event(name, &args, 0, false);
        }
    }

    fn call_export(&self, resource: &str, export: &str, args: Vec<Json>) -> Result<Json, String> {
        match self.manager.upgrade() {
            Some(m) => m.call_export(resource, export, &args),
            None => Err("the server is shutting down".to_string()),
        }
    }

    fn now_ms(&self) -> f64 {
        now_ms()
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Turns what the server reports into Lua events.
struct Hooks {
    inner: Weak<Inner>,
}

/// What `playerConnecting` handlers decided, shared with the Lua callbacks.
#[derive(Default)]
struct Connecting {
    kick_reason: Option<String>,
    deferred: Option<oneshot::Receiver<Option<String>>>,
    finish: Option<oneshot::Sender<Option<String>>>,
}

impl ServerHandler for Hooks {
    fn player_connecting(&self, info: &ConnectInfo) -> ConnectDecision {
        let Some(inner) = self.inner.upgrade() else {
            return ConnectDecision::Accept;
        };
        let state = Rc::new(RefCell::new(Connecting::default()));

        for resource in inner.snapshot() {
            let state = state.clone();
            let name = info.name.clone();
            resource
                .runtime
                .trigger_event_with("playerConnecting", 0, false, move |lua| {
                    let set_kick_reason = {
                        let state = state.clone();
                        lua.create_function(move |_, reason: Option<String>| {
                            state.borrow_mut().kick_reason =
                                Some(reason.unwrap_or_else(|| "Rejected.".to_string()));
                            Ok(())
                        })?
                    };
                    let deferrals = lua.create_table()?;
                    {
                        let state = state.clone();
                        deferrals.set(
                            "defer",
                            lua.create_function(move |_, ()| {
                                let mut s = state.borrow_mut();
                                if s.deferred.is_none() && s.finish.is_none() {
                                    let (tx, rx) = oneshot::channel();
                                    s.finish = Some(tx);
                                    s.deferred = Some(rx);
                                }
                                Ok(())
                            })?,
                        )?;
                    }
                    deferrals.set(
                        "update",
                        lua.create_function(|_, _: Variadic<Value>| Ok(()))?,
                    )?;
                    {
                        let state = state.clone();
                        deferrals.set(
                            "done",
                            lua.create_function(move |_, reason: Option<String>| {
                                let mut s = state.borrow_mut();
                                match s.finish.take() {
                                    Some(finish) => {
                                        let _ = finish.send(reason);
                                    }
                                    // Not deferred: done("reason") is a plain rejection.
                                    None if s.deferred.is_none() => {
                                        if let Some(reason) = reason {
                                            s.kick_reason = Some(reason);
                                        }
                                    }
                                    None => {}
                                }
                                Ok(())
                            })?,
                        )?;
                    }
                    (name, set_kick_reason, deferrals).into_lua_multi(lua)
                });
        }

        let mut state = state.borrow_mut();
        if let Some(reason) = state.kick_reason.take() {
            return ConnectDecision::Reject(reason);
        }
        match state.deferred.take() {
            Some(pending) => ConnectDecision::Defer(pending),
            None => ConnectDecision::Accept,
        }
    }

    fn player_joined(&self, player: &PlayerInfo) {
        if let Some(inner) = self.inner.upgrade() {
            let args = [Json::from(player.id), Json::from(player.name.as_str())];
            inner.trigger_event("playerJoined", &args, i64::from(player.id), false);
        }
    }

    fn player_dropped(&self, player: &PlayerInfo, reason: &str) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let reason = if reason.is_empty() {
            "Disconnected."
        } else {
            reason
        };
        *inner.dropping.borrow_mut() = Some(player.clone());
        inner.trigger_event(
            "playerDropped",
            &[Json::from(reason)],
            i64::from(player.id),
            false,
        );
        *inner.dropping.borrow_mut() = None;
    }

    fn net_event(&self, player: u16, name: &str, args: Vec<Json>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if name == COMMAND_EVENT {
            if let Some(raw) = args.first().and_then(Json::as_str) {
                if raw.len() <= MAX_COMMAND_LENGTH {
                    inner.execute_command(raw, i64::from(player));
                }
            }
            return;
        }
        inner.trigger_event(name, &args, i64::from(player), true);
    }

    fn tick(&self, _tick: u64, _dt_ms: f64) {
        if let Some(inner) = self.inner.upgrade() {
            let now = now_ms();
            for resource in inner.snapshot() {
                resource.runtime.tick(now);
            }
        }
    }
}

use mlua::IntoLuaMulti;

/// A player id as scripts pass it: a number, or a string holding one.
fn player_id(value: &Value) -> i64 {
    match value {
        Value::Integer(i) => *i,
        Value::Number(n) if n.fract() == 0.0 => *n as i64,
        Value::String(s) => s
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(-1),
        _ => -1,
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        _ => String::new(),
    }
}

/// The functions server-side scripts can call. Each holds the manager weakly,
/// so a runtime never keeps the server alive.
fn register_natives(inner: &Rc<Inner>, lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let weak = Rc::downgrade(inner);
    // Shorthand: a native that needs the manager, and fails cleanly once it is gone.
    macro_rules! native {
        ($name:literal, |$m:ident, $lua:ident, $args:pat_param, $ty:ty| $body:expr) => {{
            let weak = weak.clone();
            globals.set(
                $name,
                lua.create_function(move |$lua, $args: $ty| {
                    let $m = weak.upgrade().ok_or_else(|| mlua::Error::runtime("the server is shutting down"))?;
                    // Bound first so borrows inside the body end before the manager handle is dropped.
                    let result = $body;
                    result
                })?,
            )?;
        }};
    }

    // players
    native!("GetPlayers", |m, _lua, (), ()| Ok(m.server.player_ids()));
    native!("GetNumPlayers", |m, _lua, (), ()| Ok(m
        .server
        .player_count()));
    native!("GetPlayerName", |m, _lua, id, Value| Ok(m
        .player(player_id(&id))
        .map(|p| p.name)));
    native!("GetPlayerPing", |m, _lua, id, Value| Ok(m
        .player(player_id(&id))
        .map_or(-1, |p| p.ping)));
    native!("GetPlayerEndpoint", |m, _lua, id, Value| Ok(m
        .player(player_id(&id))
        .map(|p| p.endpoint)));
    native!("GetPlayerIdentifiers", |m, _lua, id, Value| Ok(m
        .player(player_id(&id))
        .map(|p| p.identifiers)
        .unwrap_or_default()));
    native!("GetPlayerCoords", |m, lua, id, Value| {
        match m.player(player_id(&id)) {
            Some(p) => {
                let coords = lua.create_table()?;
                coords.set("x", p.state.x)?;
                coords.set("y", p.state.y)?;
                coords.set("z", p.state.z)?;
                Ok(Value::Table(coords))
            }
            None => Ok(Value::Nil),
        }
    });
    native!("GetPlayerHeading", |m, _lua, id, Value| Ok(m
        .player(player_id(&id))
        .map_or(0.0, |p| p.state.heading)));
    native!("DropPlayer", |m, _lua, (id, reason), (Value, Option<String>)| {
        if let Ok(id) = u16::try_from(player_id(&id)) {
            m.server.kick(id, reason.as_deref().unwrap_or("Dropped by the server."));
        }
        Ok(())
    });

    // events
    native!("TriggerClientEvent", |m, _lua, (name, target, args), (Value, Value, Variadic<Value>)| {
        let Value::String(name) = name else {
            return Err(mlua::Error::runtime("TriggerClientEvent: event name must be a string"));
        };
        let target = match target {
            Value::Integer(i) => i32::try_from(i).ok(),
            Value::Number(n) if n.fract() == 0.0 => Some(n as i32),
            _ => None,
        }
        .ok_or_else(|| mlua::Error::runtime("TriggerClientEvent: target must be a player id or -1"))?;
        let args = args.iter().map(lua_to_json).collect::<Result<Vec<_>, _>>().map_err(mlua::Error::runtime)?;
        m.server.emit_net(target, &name.to_string_lossy(), args);
        Ok(())
    });

    // convars
    native!("GetConvar", |m, lua, (name, fallback), (Value, Value)| {
        let found = m.convars.borrow().get(&text(&name)).cloned();
        match found {
            Some(value) => Ok(Value::String(lua.create_string(&value)?)),
            None => Ok(fallback),
        }
    });
    native!("GetConvarInt", |m, _lua, (name, fallback), (Value, Option<i64>)| {
        let found = m.convars.borrow().get(&text(&name)).cloned();
        Ok(found.and_then(|v| leading_int(&v)).or(fallback).unwrap_or(0))
    });
    native!("SetConvar", |m, _lua, (name, value), (Value, Value)| {
        m.convars.borrow_mut().insert(text(&name), text(&value));
        Ok(())
    });

    // resources and commands
    native!("GetResourceState", |m, _lua, name, String| {
        Ok(if m.find(&name).is_some() {
            "started"
        } else if m.available.borrow().contains_key(&name) {
            "stopped"
        } else {
            "missing"
        })
    });
    native!("GetNumResources", |m, _lua, (), ()| Ok(m
        .running
        .borrow()
        .len()));
    native!("ExecuteCommand", |m, _lua, raw, String| Ok(
        m.execute_command(&raw, 0)
    ));
    native!("GetGameTimer", |m, _lua, (), ()| Ok(
        m.started_at.elapsed().as_millis() as i64
    ));
    native!("GetHostName", |m, _lua, (), ()| Ok(m.server.name()));
    Ok(())
}

/// The integer a string starts with (`"30 seconds"` reads as 30).
fn leading_int(text: &str) -> Option<i64> {
    let t = text.trim_start();
    let (sign, digits) = match t.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().ok().map(|n| sign * n)
}
