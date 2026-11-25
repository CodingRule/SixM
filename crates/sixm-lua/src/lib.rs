//! Sandboxed Lua 5.4 runtime for SixM resources.
//!
//! A [`LuaRuntime`] is one isolated Lua state: one per resource per side. The
//! runtime owns only Lua. The host owns the event bus, the network and the
//! clock, and drives the runtime through [`LuaRuntime::trigger_event`],
//! [`LuaRuntime::run_command`] and [`LuaRuntime::tick`]; the runtime calls back
//! through the [`Host`] trait.
//!
//! The scheduler, events, commands and exports are written in Lua
//! (`bootstrap.lua`); this crate is the state setup, the sandbox, the JSON
//! bridge ([`lua_to_json`] / [`json_to_lua`]), the instruction watchdog and
//! the protected entry points.
//!
//! Everything is single-threaded and re-entrant: a script may call into the
//! host, and the host may call straight back into the same runtime.

mod json;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mlua::{
    ChunkMode, Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Thread,
    ThreadStatus, Value, Variadic, VmState,
};

pub use json::{json_to_lua, lua_to_json, MAX_DEPTH};
pub use mlua;
pub use serde_json::Value as Json;

const BOOTSTRAP: &str = include_str!("bootstrap.lua");
/// Chunk name of the bootstrap. The watchdog never interrupts code from it.
const BOOTSTRAP_CHUNK: &str = "@sixm/bootstrap.lua";
/// The watchdog hook runs every this many VM instructions (at most).
const WATCHDOG_STEP: u64 = 1000;

/// Which half of a resource a runtime executes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// Server scripts (`IS_SERVER`, `IsDuplicityVersion() == true`).
    Server,
    /// Client scripts (`IS_CLIENT`).
    Client,
}

/// Callbacks a runtime makes into its host.
///
/// Every method may be called while a call into the runtime is on the stack,
/// and may itself call back into the same runtime.
pub trait Host {
    /// One `print(...)` call from a script, arguments already tab-joined.
    fn print(&self, line: &str);

    /// A script error that nobody caught. `context` says where it happened:
    /// `load:<chunk>`, `thread`, `timeout`, `event:<name>`, `command:<name>`,
    /// `export:<name>` or `tick`. `message` ends with a Lua traceback when one
    /// is available. A script stopped by the watchdog is reported under the
    /// context it was running in, with an "exceeded instruction limit" message.
    fn error(&self, context: &str, message: &str);

    /// Lua `TriggerEvent(name, ...)`. The host fans the event out to every
    /// runtime that should see it (including the caller) with
    /// [`LuaRuntime::trigger_event`].
    fn trigger_event(&self, name: &str, args: Vec<Json>);

    /// Lua `exports.<resource>:<export>(...)`. An `Err` becomes a Lua error in
    /// the calling script.
    fn call_export(&self, resource: &str, export: &str, args: Vec<Json>) -> Result<Json, String>;

    /// The scheduler clock in milliseconds; `Wait` deadlines are measured on it.
    fn now_ms(&self) -> f64;
}

/// Tunables for [`LuaRuntime::with_options`].
#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    /// Watchdog budget: the number of VM instructions a script may execute
    /// without yielding, per entry from the host and per coroutine resume.
    /// `0` disables the watchdog (and its overhead).
    pub instruction_limit: u64,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            instruction_limit: 50_000_000,
        }
    }
}

/// The instruction budget, shared with the closures that live inside Lua.
struct Budget {
    /// Instructions used by the entry / resume in progress.
    used: Cell<u64>,
    /// 0 when the watchdog is disabled.
    limit: u64,
}

impl Budget {
    fn tripped(&self) -> bool {
        self.limit != 0 && self.used.get() > self.limit
    }
}

/// bootstrap.lua's internal entry points.
struct EntryPoints {
    tick: Function,
    pending: Function,
    kill: Function,
    wipe: Function,
    listening: Function,
    trigger: Function,
    command: Function,
    has_command: Function,
    is_command_restricted: Function,
    call_export: Function,
    has_export: Function,
}

/// One isolated, sandboxed Lua state: a resource's scripts for one side.
pub struct LuaRuntime {
    resource_name: String,
    host: Rc<dyn Host>,
    lua: Lua,
    budget: Rc<Budget>,
    /// `None` once the runtime has been closed.
    entry_points: RefCell<Option<Rc<EntryPoints>>>,
    /// Nesting of host → runtime calls in progress.
    depth: Cell<usize>,
    dead: Cell<bool>,
}

/// One host → runtime call in progress. Dropping it restores the caller's
/// instruction budget and finishes a `destroy()` requested during the call.
struct Entry<'a> {
    runtime: &'a LuaRuntime,
    fns: Rc<EntryPoints>,
    saved_budget: u64,
}

impl Drop for Entry<'_> {
    fn drop(&mut self) {
        let runtime = self.runtime;
        runtime.budget.used.set(self.saved_budget);
        let depth = runtime.depth.get() - 1;
        runtime.depth.set(depth);
        if depth == 0 && runtime.dead.get() {
            runtime.close();
        }
    }
}

impl LuaRuntime {
    /// Creates a runtime with the default [`RuntimeOptions`].
    pub fn new(resource_name: &str, side: Side, host: Rc<dyn Host>) -> Result<Self, String> {
        Self::with_options(resource_name, side, host, RuntimeOptions::default())
    }

    /// Creates a runtime: opens the sandboxed libraries and installs the SixM
    /// Lua API. No resource code has run yet.
    pub fn with_options(
        resource_name: &str,
        side: Side,
        host: Rc<dyn Host>,
        options: RuntimeOptions,
    ) -> Result<Self, String> {
        if resource_name.is_empty() {
            return Err("LuaRuntime: resource_name must not be empty".to_owned());
        }
        let budget = Rc::new(Budget {
            used: Cell::new(0),
            limit: options.instruction_limit,
        });
        let lua = open_sandbox().map_err(|err| format!("SixM sandbox setup failed: {err}"))?;
        install_watchdog(&lua, resource_name, &budget)
            .map_err(|err| format!("SixM watchdog setup failed: {err}"))?;
        let entry_points = run_bootstrap(&lua, resource_name, side, &host, &budget)
            .map_err(|err| format!("SixM bootstrap failed: {}", describe_error(&err)))?;
        Ok(Self {
            resource_name: resource_name.to_owned(),
            host,
            lua,
            budget,
            entry_points: RefCell::new(Some(Rc::new(entry_points))),
            depth: Cell::new(0),
            dead: Cell::new(false),
        })
    }

    /// Name of the resource this runtime belongs to.
    pub fn resource_name(&self) -> &str {
        &self.resource_name
    }

    /// The Lua state, for registering host natives:
    /// `rt.lua().globals().set("GetPlayers", rt.lua().create_function(...)?)`.
    ///
    /// The state outlives [`destroy`](Self::destroy) (it is freed when the
    /// runtime is dropped), but a destroyed runtime's state is empty.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Runs a chunk of resource code. Errors go to `Host::error` under
    /// `load:<chunk_name>` with a traceback. Returns whether it ran without error.
    pub fn load_script(&self, code: &str, chunk_name: &str) -> bool {
        let Some(_entry) = self.enter() else {
            return false;
        };
        let result = self
            .lua
            .load(code)
            .set_name(format!("@{chunk_name}"))
            .set_mode(ChunkMode::Text)
            .exec();
        match result {
            Ok(()) => true,
            Err(err) => {
                self.report(&format!("load:{chunk_name}"), &err);
                false
            }
        }
    }

    /// Runs this runtime's handlers for the event `name`; with `from_net`, only
    /// if the event was allowed with `RegisterNetEvent`. The Lua global
    /// `source` equals `source` while each handler runs. Every handler runs in
    /// its own coroutine and may `Wait`. Returns the number of handlers invoked.
    pub fn trigger_event(&self, name: &str, args: &[Json], source: i64, from_net: bool) -> usize {
        self.trigger_event_with(name, source, from_net, |lua| {
            args.iter().map(|arg| json_to_lua(lua, arg)).collect()
        })
    }

    /// Like [`trigger_event`](Self::trigger_event), but the caller builds the
    /// Lua arguments, so it can pass values JSON cannot carry (such as Rust
    /// callbacks as Lua functions). `build_args` is only called when at least
    /// one handler will run.
    pub fn trigger_event_with<F>(
        &self,
        name: &str,
        source: i64,
        from_net: bool,
        build_args: F,
    ) -> usize
    where
        F: FnOnce(&Lua) -> mlua::Result<MultiValue>,
    {
        let Some(entry) = self.enter() else {
            return 0;
        };
        let run = || -> mlua::Result<usize> {
            if !entry.fns.listening.call::<bool>((name, from_net))? {
                return Ok(0);
            }
            let mut args = build_args(&self.lua)?;
            args.push_front(Value::Boolean(from_net));
            args.push_front(Value::Integer(source));
            args.push_front(Value::String(self.lua.create_string(name)?));
            entry.fns.trigger.call(args)
        };
        run().unwrap_or_else(|err| {
            self.report(&format!("event:{name}"), &err);
            0
        })
    }

    /// Whether a script registered the command `name` with `RegisterCommand`.
    pub fn has_command(&self, name: &str) -> bool {
        self.query(|fns| &fns.has_command, name)
    }

    /// Whether the command was registered with a truthy `restricted` flag
    /// (for the host's permission checks).
    pub fn is_command_restricted(&self, name: &str) -> bool {
        self.query(|fns| &fns.is_command_restricted, name)
    }

    /// Runs a command as `fn(source, args, raw)` in its own coroutine. Returns
    /// whether this runtime owns the command.
    pub fn run_command(&self, name: &str, source: i64, args: &[String], raw: &str) -> bool {
        let Some(entry) = self.enter() else {
            return false;
        };
        let run = || -> mlua::Result<bool> {
            let list = self
                .lua
                .create_sequence_from(args.iter().map(String::as_str))?;
            entry.fns.command.call((name, source, list, raw))
        };
        run().unwrap_or_else(|err| {
            self.report(&format!("command:{name}"), &err);
            false
        })
    }

    /// Resumes coroutines whose `Wait` deadline has passed and fires due
    /// timeouts. Cheap when nothing is due.
    pub fn tick(&self, now_ms: f64) {
        let Some(entry) = self.enter() else {
            return;
        };
        if let Err(err) = entry.fns.tick.call::<()>(now_ms) {
            self.report("tick", &err);
        }
    }

    /// Number of sleeping threads and pending timeouts.
    pub fn pending_threads(&self) -> usize {
        self.enter()
            .and_then(|entry| entry.fns.pending.call(()).ok())
            .unwrap_or(0)
    }

    /// Whether a script published `name` with `exports('name', fn)`.
    pub fn has_export(&self, name: &str) -> bool {
        self.query(|fns| &fns.has_export, name)
    }

    /// Calls a function published with `exports('name', fn)` and returns its
    /// first result. `Err` if the export is missing, raises, or the runtime is
    /// destroyed. An export that `Wait`s finishes later; `Json::Null` is
    /// returned now.
    pub fn call_export(&self, name: &str, args: &[Json]) -> Result<Json, String> {
        let Some(entry) = self.enter() else {
            return Err(format!("resource '{}' is not running", self.resource_name));
        };
        let run = || -> mlua::Result<(bool, Value)> {
            let mut call_args = MultiValue::with_capacity(args.len() + 1);
            call_args.push_back(Value::String(self.lua.create_string(name)?));
            for arg in args {
                call_args.push_back(json_to_lua(&self.lua, arg)?);
            }
            entry.fns.call_export.call(call_args)
        };
        match run() {
            Ok((true, value)) => lua_to_json(&value),
            Ok((false, message)) => Err(lossy_text(&message)),
            Err(err) => Err(describe_error(&err)),
        }
    }

    /// Stops the runtime. Afterwards every method is a safe no-op (`false`,
    /// `0` or `Err`). Safe to call while a call into this runtime is on the
    /// stack (a resource stopping itself): the function that is running
    /// finishes, nothing new starts, and the state is emptied when the
    /// outermost call unwinds.
    pub fn destroy(&self) {
        if self.dead.replace(true) {
            return;
        }
        if self.depth.get() == 0 {
            self.close();
            return;
        }
        // Lua is running right now: tell the dispatch loops to stop.
        let fns = self.entry_points.borrow().clone();
        if let Some(fns) = fns {
            if let Err(err) = fns.kill.call::<()>(()) {
                self.report("tick", &err);
            }
        }
    }

    /// Whether [`destroy`](Self::destroy) has been called.
    pub fn is_destroyed(&self) -> bool {
        self.dead.get()
    }

    /// Begins a host → runtime call, or returns `None` if the runtime is dead.
    fn enter(&self) -> Option<Entry<'_>> {
        if self.dead.get() {
            return None;
        }
        let fns = self.entry_points.borrow().clone()?;
        self.depth.set(self.depth.get() + 1);
        Some(Entry {
            runtime: self,
            fns,
            saved_budget: self.budget.used.replace(0),
        })
    }

    /// A boolean lookup by name that never raises.
    fn query(&self, pick: impl FnOnce(&EntryPoints) -> &Function, name: &str) -> bool {
        self.enter()
            .and_then(|entry| pick(&entry.fns).call(name).ok())
            .unwrap_or(false)
    }

    fn report(&self, context: &str, err: &mlua::Error) {
        self.host.error(context, &describe_error(err));
    }

    /// Releases everything the resource created: the entry points, every
    /// global (and with them the scheduler, handlers and host natives), then
    /// collects. Only called when no call into the runtime is in progress.
    fn close(&self) {
        let Some(fns) = self.entry_points.borrow_mut().take() else {
            return;
        };
        self.budget.used.set(0); // finalizers get a fresh (and enforced) budget
        let emptied = fns.wipe.call::<()>(());
        drop(fns);
        let collected = emptied
            .and_then(|()| self.lua.gc_collect())
            .and_then(|()| self.lua.gc_collect());
        if let Err(err) = collected {
            self.report("tick", &err);
        }
    }
}

/// Opens a state with only the safe standard libraries: base, coroutine,
/// table, string, utf8, math, `os` reduced to its clock functions and `debug`
/// reduced to `traceback`. `io` and `package` are never loaded.
fn open_sandbox() -> mlua::Result<Lua> {
    let libs = StdLib::COROUTINE
        | StdLib::TABLE
        | StdLib::STRING
        | StdLib::UTF8
        | StdLib::MATH
        | StdLib::OS
        | StdLib::DEBUG;
    // SAFETY: mlua's safe constructor refuses to open the `debug` library at
    // all, and scripts need `debug.traceback`. "Unsafe" here means only that
    // mlua does not vouch for what scripts can do with the opened libraries:
    // `debug` is cut down to `traceback` right below, before any script runs,
    // `package` (C modules) is not opened, and bootstrap.lua restricts `load`
    // to text chunks. No memory-safety invariant is delegated to this crate.
    let lua = unsafe { Lua::unsafe_new_with(libs, LuaOptions::default()) };
    let globals = lua.globals();
    for (library, keep) in [
        ("os", &["time", "clock", "date", "difftime"][..]),
        ("debug", &["traceback"][..]),
    ] {
        let full: Table = globals.get(library)?;
        let trimmed = lua.create_table_with_capacity(0, keep.len())?;
        for name in keep {
            trimmed.set(*name, full.get::<Value>(*name)?)?;
        }
        globals.set(library, trimmed)?;
    }
    Ok(lua)
}

/// Installs the instruction-count hook. Coroutines inherit it from the thread
/// that creates them, so it covers scheduler threads and script coroutines.
fn install_watchdog(lua: &Lua, resource_name: &str, budget: &Rc<Budget>) -> mlua::Result<()> {
    let limit = budget.limit;
    if limit == 0 {
        return Ok(());
    }
    let step = limit.min(WATCHDOG_STEP);
    let budget = Rc::clone(budget);
    let message = format!(
        "resource '{resource_name}' exceeded instruction limit ({limit} instructions without yielding)"
    );
    let triggers = HookTriggers::new().every_nth_instruction(step as u32);
    lua.set_global_hook(triggers, move |lua, debug| {
        let used = budget.used.get().saturating_add(step);
        budget.used.set(used);
        if used <= limit {
            return Ok(VmState::Continue);
        }
        // Never interrupt the scheduler itself: it would be left half-updated.
        // The budget stays exhausted, so the next hook in resource code fires.
        if debug.source().source.as_deref() == Some(BOOTSTRAP_CHUNK) {
            return Ok(VmState::Continue);
        }
        // Hook errors carry no traceback of their own: attach the one of the
        // thread that was interrupted.
        let report = lua.traceback(Some(&message), 0);
        Err(mlua::Error::runtime(report.map_or_else(
            |_| message.clone(),
            |text| text.to_string_lossy(),
        )))
    })
}

/// Runs bootstrap.lua with the bridge table and collects its entry points.
fn run_bootstrap(
    lua: &Lua,
    resource_name: &str,
    side: Side,
    host: &Rc<dyn Host>,
    budget: &Rc<Budget>,
) -> mlua::Result<EntryPoints> {
    let bridge = lua.create_table()?;
    bridge.set("resourceName", resource_name)?;
    bridge.set("chunkName", BOOTSTRAP_CHUNK)?;
    bridge.set("isServer", side == Side::Server)?;

    let print_host = Rc::clone(host);
    bridge.set(
        "print",
        lua.create_function(move |_, line: mlua::String| {
            print_host.print(&line.to_string_lossy());
            Ok(())
        })?,
    )?;

    let report_host = Rc::clone(host);
    bridge.set(
        "report",
        lua.create_function(move |_, (message, context): (mlua::String, mlua::String)| {
            report_host.error(&context.to_string_lossy(), &message.to_string_lossy());
            Ok(())
        })?,
    )?;

    let clock_host = Rc::clone(host);
    bridge.set(
        "now",
        lua.create_function(move |_, ()| Ok(clock_host.now_ms()))?,
    )?;

    // triggerEvent(name, ...) -> true | false, message
    let event_host = Rc::clone(host);
    bridge.set(
        "triggerEvent",
        lua.create_function(move |_, (name, args): (mlua::String, Variadic<Value>)| {
            Ok(match args.iter().map(lua_to_json).collect() {
                Ok(args) => {
                    event_host.trigger_event(&name.to_string_lossy(), args);
                    (true, None)
                }
                Err(message) => (false, Some(message)),
            })
        })?,
    )?;

    // callExport(resource, export, ...) -> true, value | false, message
    let export_host = Rc::clone(host);
    bridge.set(
        "callExport",
        lua.create_function(
            move |lua, (resource, export, args): (mlua::String, mlua::String, Variadic<Value>)| {
                let result = args
                    .iter()
                    .map(lua_to_json)
                    .collect::<Result<Vec<Json>, String>>()
                    .and_then(|args| {
                        export_host.call_export(
                            &resource.to_string_lossy(),
                            &export.to_string_lossy(),
                            args,
                        )
                    });
                match result {
                    Ok(value) => Ok((true, json_to_lua(lua, &value)?)),
                    Err(message) => Ok((false, Value::String(lua.create_string(message)?))),
                }
            },
        )?,
    )?;

    // jsonEncode(value) -> text | nil, message
    bridge.set(
        "jsonEncode",
        lua.create_function(|_, value: Value| {
            Ok(match json::encode(&value) {
                Ok(text) => (Some(text), None),
                Err(message) => (None, Some(message)),
            })
        })?,
    )?;

    // jsonDecode(text) -> true, value | false, message
    bridge.set(
        "jsonDecode",
        lua.create_function(|lua, text: mlua::String| {
            Ok(match json::decode(lua, &text.to_string_lossy()) {
                Ok(value) => (true, value),
                Err(message) => (false, Value::String(lua.create_string(message)?)),
            })
        })?,
    )?;

    // resume(co, ...): see the scheduler notes in bootstrap.lua.
    let resume_budget = Rc::clone(budget);
    bridge.set(
        "resume",
        lua.create_function(move |lua, (thread, args): (Thread, MultiValue)| {
            let outer_budget = resume_budget.used.replace(0);
            let outcome = thread.resume::<MultiValue>(args);
            resume_budget.used.set(outer_budget);
            match outcome {
                Ok(mut values) => {
                    let suspended = thread.status() == ThreadStatus::Resumable;
                    values.push_front(Value::Boolean(suspended));
                    values.push_front(Value::Boolean(true));
                    Ok(values)
                }
                Err(err) => {
                    let message = lua.create_string(describe_error(&err))?;
                    Ok(MultiValue::from_iter([
                        Value::Boolean(false),
                        Value::String(message),
                    ]))
                }
            }
        })?,
    )?;

    let tripped_budget = Rc::clone(budget);
    bridge.set(
        "tripped",
        lua.create_function(move |_, ()| Ok(tripped_budget.tripped()))?,
    )?;

    // errorText(value) -> the message if `value` is a Rust error object, else nil
    bridge.set(
        "errorText",
        lua.create_function(|_, value: Value| {
            Ok(match value {
                Value::Error(err) => Some(root_message(&err)),
                _ => None,
            })
        })?,
    )?;

    let internal: Table = lua.load(BOOTSTRAP).set_name(BOOTSTRAP_CHUNK).call(bridge)?;
    Ok(EntryPoints {
        tick: internal.get("tick")?,
        pending: internal.get("pending")?,
        kill: internal.get("kill")?,
        wipe: internal.get("wipe")?,
        listening: internal.get("listening")?,
        trigger: internal.get("trigger")?,
        command: internal.get("command")?,
        has_command: internal.get("hasCommand")?,
        is_command_restricted: internal.get("isCommandRestricted")?,
        call_export: internal.get("callExport")?,
        has_export: internal.get("hasExport")?,
    })
}

/// The text of an error for `Host::error`: the message, then the Lua traceback
/// when the error carries one separately (errors raised by Rust functions do;
/// Lua errors already have it appended to their message).
fn describe_error(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { traceback, .. } if !traceback.is_empty() => {
            format!("{}\n{traceback}", root_message(err))
        }
        other => root_message(other),
    }
}

/// The innermost message of an error, without mlua's wrapping.
fn root_message(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { cause, .. } => root_message(cause),
        mlua::Error::RuntimeError(message) | mlua::Error::SyntaxError { message, .. } => {
            message.clone()
        }
        mlua::Error::ExternalError(cause) => cause.to_string(),
        other => other.to_string(),
    }
}

/// An error value as text: strings as they are, anything else like `tostring`.
fn lossy_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_string_lossy(),
        Value::Error(err) => root_message(err),
        other => other
            .to_string()
            .unwrap_or_else(|_| other.type_name().to_owned()),
    }
}
