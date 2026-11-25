//! Behaviour tests for `LuaRuntime`: the scripting API, 64-bit integers, the
//! instruction watchdog and re-entrancy.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use serde_json::json;
use sixm_lua::mlua::{self, IntoLuaMulti, Value, Variadic};
use sixm_lua::{json_to_lua, lua_to_json, Host, Json, LuaRuntime, RuntimeOptions, Side};

/// A `TriggerEvent` call as the host sees it.
type Event = (String, Vec<Json>);

/// A host that records everything and plays the role of the resource manager:
/// events fan out to every registered runtime, exports are routed by name.
#[derive(Default)]
struct TestHost {
    lines: RefCell<Vec<String>>,
    errors: RefCell<Vec<(String, String)>>,
    /// When set, `TriggerEvent` calls are recorded here instead of dispatched.
    captured_events: RefCell<Option<Vec<Event>>>,
    clock: Cell<f64>,
    runtimes: RefCell<Vec<Rc<LuaRuntime>>>,
}

impl TestHost {
    fn runtimes(&self) -> Vec<Rc<LuaRuntime>> {
        self.runtimes.borrow().clone()
    }
}

impl Host for TestHost {
    fn print(&self, line: &str) {
        self.lines.borrow_mut().push(line.to_owned());
    }

    fn error(&self, context: &str, message: &str) {
        self.errors
            .borrow_mut()
            .push((context.to_owned(), message.to_owned()));
    }

    fn trigger_event(&self, name: &str, args: Vec<Json>) {
        if let Some(captured) = self.captured_events.borrow_mut().as_mut() {
            captured.push((name.to_owned(), args));
            return;
        }
        for runtime in self.runtimes() {
            runtime.trigger_event(name, &args, 0, false);
        }
    }

    fn call_export(&self, resource: &str, export: &str, args: Vec<Json>) -> Result<Json, String> {
        let target = self
            .runtimes()
            .into_iter()
            .find(|runtime| runtime.resource_name() == resource)
            .ok_or_else(|| format!("no such resource '{resource}'"))?;
        target.call_export(export, &args)
    }

    fn now_ms(&self) -> f64 {
        self.clock.get()
    }
}

/// One runtime wired to a recording host. `log(...)` in Lua appends its
/// arguments (through the JSON bridge) to `logged`; `echo(v)` round-trips a
/// value through the bridge.
struct Env {
    host: Rc<TestHost>,
    rt: Rc<LuaRuntime>,
    logged: Rc<RefCell<Vec<Json>>>,
}

impl Env {
    fn new() -> Self {
        Self::on(Rc::new(TestHost::default()), "test", Side::Server)
    }

    fn with_limit(instruction_limit: u64) -> Self {
        Self::build(
            Rc::new(TestHost::default()),
            "test",
            Side::Server,
            RuntimeOptions { instruction_limit },
        )
    }

    fn on(host: Rc<TestHost>, name: &str, side: Side) -> Self {
        Self::build(host, name, side, RuntimeOptions::default())
    }

    fn build(host: Rc<TestHost>, name: &str, side: Side, options: RuntimeOptions) -> Self {
        let rt =
            Rc::new(LuaRuntime::with_options(name, side, host.clone(), options).expect("runtime"));
        host.runtimes.borrow_mut().push(rt.clone());
        let logged = Rc::new(RefCell::new(Vec::new()));
        let sink = logged.clone();
        let env = Self { host, rt, logged };
        env.native("log", move |_, values: Variadic<Value>| {
            let mut items = values
                .iter()
                .map(lua_to_json)
                .collect::<Result<Vec<Json>, String>>()
                .map_err(mlua::Error::runtime)?;
            sink.borrow_mut().push(if items.len() == 1 {
                items.remove(0)
            } else {
                Json::Array(items)
            });
            Ok(())
        });
        env.native("echo", |lua, value: Value| {
            let json = lua_to_json(&value).map_err(mlua::Error::runtime)?;
            json_to_lua(lua, &json)
        });
        env
    }

    /// Registers a host native, the way the server does.
    fn native<F, A, R>(&self, name: &str, function: F)
    where
        F: Fn(&mlua::Lua, A) -> mlua::Result<R> + 'static,
        A: mlua::FromLuaMulti,
        R: mlua::IntoLuaMulti,
    {
        let lua = self.rt.lua();
        let function = lua.create_function(function).expect("create_function");
        lua.globals().set(name, function).expect("set global");
    }

    fn set_global(&self, name: &str, value: &Json) {
        let lua = self.rt.lua();
        let value = json_to_lua(lua, value).expect("json_to_lua");
        lua.globals().set(name, value).expect("set global");
    }

    /// Loads a script and fails the test with the Lua error if it did not run cleanly.
    #[track_caller]
    fn run(&self, code: &str) {
        let ok = self.rt.load_script(code, "test.lua");
        assert_eq!(
            self.errors(),
            Vec::<(String, String)>::new(),
            "script raised errors"
        );
        assert!(ok);
    }

    fn logged(&self) -> Vec<Json> {
        self.logged.borrow().clone()
    }

    fn errors(&self) -> Vec<(String, String)> {
        self.host.errors.borrow().clone()
    }

    fn lines(&self) -> Vec<String> {
        self.host.lines.borrow().clone()
    }
}

#[track_caller]
fn assert_contains(text: &str, needle: &str) {
    assert!(text.contains(needle), "expected {needle:?} in:\n{text}");
}

// ------------------------------------------------------------------ JSON bridge

#[test]
fn bridge_primitives_integers_vs_floats() {
    let env = Env::new();
    env.run(
        r#"
        log(echo(nil) == nil, echo(true), echo(false))
        log(math.type(echo(3)), math.type(echo(3.5)), echo(-7), echo(0.25))
        local big = echo(1 << 40)
        log(math.type(big), big == 1099511627776)
        log(math.type(echo(2^40)), echo(2^40) == 2^40, math.type(echo(3.0)))
        log(echo(0/0) == nil, echo(math.huge) == nil, echo(-math.huge) == nil)
    "#,
    );
    assert_eq!(
        env.logged(),
        vec![
            json!([true, true, false]),
            json!(["integer", "float", -7, 0.25]),
            json!(["integer", true]),
            json!(["float", true, "float"]),
            json!([true, true, true]), // NaN and the infinities become null
        ]
    );

    env.set_global(
        "fromHost",
        &json!({ "int": 42, "float": 1.5, "huge": 1u64 << 40, "nothing": null }),
    );
    env.run("log(math.type(fromHost.int), math.type(fromHost.float), math.type(fromHost.huge), fromHost.huge, fromHost.nothing == nil)");
    assert_eq!(
        env.logged()[5],
        json!(["integer", "float", "integer", 1u64 << 40, true])
    );
}

#[test]
fn bridge_64_bit_integers_survive_end_to_end() {
    let env = Env::new();
    *env.host.captured_events.borrow_mut() = Some(Vec::new());
    env.run(
        r#"
        AddEventHandler('stamp', function(at, record)
          log(math.type(at), at, math.type(record.id), math.type(record.max), math.type(record.min))
          log(at + 1, record.id == 9007199254740993, record.max == math.maxinteger)
          TriggerEvent('stamp:reply', at, record)
          log(json.encode(record))
        end)
    "#,
    );
    let at = json!(1_795_597_672_181_i64);
    let record = json!({ "id": 9_007_199_254_740_993_i64, "max": i64::MAX, "min": i64::MIN });
    assert_eq!(
        env.rt
            .trigger_event("stamp", &[at.clone(), record.clone()], 0, false),
        1
    );
    assert_eq!(
        env.logged(),
        vec![
            json!([
                "integer",
                1_795_597_672_181_i64,
                "integer",
                "integer",
                "integer"
            ]),
            json!([1_795_597_672_182_i64, true, true]),
            json!(
                r#"{"id":9007199254740993,"max":9223372036854775807,"min":-9223372036854775808}"#
            ),
        ]
    );
    let captured = env.host.captured_events.borrow().clone().unwrap();
    assert_eq!(captured, vec![("stamp:reply".to_owned(), vec![at, record])]);
    assert!(captured[0].1[0].is_i64());

    // Above the i64 range a JSON number can only be a float.
    env.set_global("unsigned", &json!(u64::MAX));
    env.run("log(math.type(unsigned))");
    assert_eq!(env.logged()[3], json!("float"));
}

#[test]
fn bridge_unicode_and_invalid_utf8() {
    let env = Env::new();
    let text = "héllo wörld ✓ 日本語 😀";
    env.set_global("text", &json!(text));
    env.run(
        r#"
        log(text, #text)
        log(echo('añb€'))
        log(type(echo('\xff\xfe not utf-8')), #echo('\xff'))
    "#,
    );
    assert_eq!(
        env.logged(),
        vec![
            json!([text, text.len()]),
            json!("añb€"),
            json!(["string", 3]), // invalid UTF-8 is converted leniently (U+FFFD)
        ]
    );
}

#[test]
fn bridge_tables_to_json() {
    let env = Env::new();
    env.run(
        r#"
        log({ 1, 2, 3 })
        log({ name = 'x', list = { 'a', 'b' }, nested = { deep = { ok = true } } })
        log({})
        log({ [1] = 'a', [3] = 'c' })
        log({ 'a', extra = 1 })
        log({ [1.5] = 'f', [true] = 'b' })
        log({ fn = print, co = coroutine.create(print), list = { print, 2 } })
        log(vector3(1, 2, 3))
    "#,
    );
    assert_eq!(
        env.logged(),
        vec![
            json!([1, 2, 3]),
            json!({ "name": "x", "list": ["a", "b"], "nested": { "deep": { "ok": true } } }),
            json!({}), // an empty table cannot be told apart from an empty array
            json!({ "1": "a", "3": "c" }),
            json!({ "1": "a", "extra": 1 }),
            json!({ "1.5": "f", "true": "b" }),
            json!({ "fn": null, "co": null, "list": [null, 2] }),
            json!({ "x": 1.0, "y": 2.0, "z": 3.0 }),
        ]
    );
}

#[test]
fn bridge_json_to_lua_round_trips_nested_data() {
    let env = Env::new();
    let value = json!({
        "players": [{ "id": 1, "name": "a" }, { "id": 2, "name": "b" }],
        "meta": { "tags": ["x", "y"], "n": 2.5 },
    });
    env.set_global("data", &value);
    env.run(
        r#"
        log(#data.players, data.players[2].name, data.meta.tags[1], data.meta.n)
        log(echo(data))
        log(echo({ [5] = 'five', [9] = 'nine' })[9], echo({ [5] = 'five' })['5'] == nil)
        log(echo({ ['07'] = 'padded', ['-3'] = 'negative' }))
    "#,
    );
    let logged = env.logged();
    assert_eq!(logged[0], json!([2, "b", "x", 2.5]));
    assert_eq!(logged[1], value);
    // Integer keys survive Lua -> JSON -> Lua even though JSON keys are strings.
    assert_eq!(logged[2], json!(["nine", true]));
    assert_eq!(logged[3], json!({ "07": "padded", "-3": "negative" }));
}

#[test]
fn bridge_cycles_and_excessive_depth_raise_clear_errors() {
    let env = Env::new();
    env.run(
        r#"
        local t = {} ; t.self = t
        local ok, err = pcall(log, t)
        log(ok, err)
        local deep = {} ; local cur = deep
        for i = 1, 80 do cur.next = {} ; cur = cur.next end
        ok, err = pcall(log, deep)
        log(ok, err)
        local shared = { 1 }
        log({ a = shared, b = shared })
        ok, err = pcall(TriggerEvent, 'x', t)
        log(ok, err)
    "#,
    );
    let logged = env.logged();
    assert_eq!(logged[0][0], json!(false));
    assert_contains(logged[0][1].as_str().unwrap(), "cycle");
    assert_eq!(logged[1][0], json!(false));
    assert_contains(logged[1][1].as_str().unwrap(), "deeper than 64");
    assert_eq!(logged[2], json!({ "a": [1], "b": [1] })); // shared, not cyclic
    assert_eq!(logged[3][0], json!(false));
    assert_contains(
        logged[3][1].as_str().unwrap(),
        "TriggerEvent: cannot convert table: it contains a cycle",
    );
}

// ---------------------------------------------------------------- host natives

#[test]
fn host_native_errors_are_catchable_and_reported_with_a_traceback() {
    let env = Env::new();
    env.native("fail", |_, reason: String| -> mlua::Result<()> {
        Err(mlua::Error::runtime(format!("native failed: {reason}")))
    });
    // The pattern the server uses for natives that take script values.
    let sent = Rc::new(RefCell::new(Vec::new()));
    let outbox = sent.clone();
    env.native(
        "TriggerClientEvent",
        move |_, (name, target, args): (String, i64, Variadic<Value>)| {
            let json: Vec<Json> = args
                .iter()
                .map(lua_to_json)
                .collect::<Result<_, _>>()
                .map_err(mlua::Error::runtime)?;
            outbox.borrow_mut().push((name, target, json));
            Ok(())
        },
    );
    env.run(
        r#"
        local ok, err = pcall(fail, 'caught')
        log(ok, err, type(err))
        local cyclic = {} ; cyclic.self = cyclic
        log(pcall(TriggerClientEvent, 'bad', 1, cyclic))
        TriggerClientEvent('chat:message', -1, 'hi', { 1, 2 })
        log('still alive')
    "#,
    );
    assert_eq!(
        env.logged(),
        vec![
            json!([false, "native failed: caught", "string"]),
            json!([false, "cannot convert table: it contains a cycle"]),
            json!("still alive"),
        ]
    );
    assert_eq!(
        *sent.borrow(),
        vec![(
            "chat:message".to_owned(),
            -1,
            vec![json!("hi"), json!([1, 2])]
        )]
    );

    // Uncaught at load time.
    assert!(!env
        .rt
        .load_script("local function go() fail('at load') end\ngo()", "bad.lua"));
    // Uncaught in a thread, after a Wait.
    assert!(env.rt.load_script(
        "CreateThread(function()\n  Wait(1)\n  fail('in thread')\nend)",
        "thread.lua"
    ));
    env.rt.tick(1.0);
    let errors = env.errors();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].0, "load:bad.lua");
    assert!(
        errors[0]
            .1
            .starts_with("native failed: at load\nstack traceback:"),
        "{}",
        errors[0].1
    );
    assert_contains(&errors[0].1, "bad.lua:1:");
    assert_eq!(errors[1].0, "thread");
    assert!(
        errors[1]
            .1
            .starts_with("native failed: in thread\nstack traceback:"),
        "{}",
        errors[1].1
    );
    assert_contains(&errors[1].1, "thread.lua:3:");

    env.rt.load_script("log('usable afterwards')", "after.lua");
    assert_eq!(env.logged()[3], json!("usable afterwards"));
}

#[test]
fn host_can_hold_and_call_lua_functions() {
    let env = Env::new();
    let kept: Rc<RefCell<Option<mlua::Function>>> = Rc::default();
    let slot = kept.clone();
    env.native("keep", move |_, function: mlua::Function| {
        *slot.borrow_mut() = Some(function);
        Ok(())
    });
    env.run("keep(function(a, b) return a * b, 'ignored' end)");
    let function = kept.borrow().clone().unwrap();
    assert_eq!(function.call::<i64>((6, 7)).unwrap(), 42);

    env.run("keep(function() error('callback failed') end)");
    let function = kept.borrow().clone().unwrap();
    let err = function.call::<()>(()).unwrap_err();
    assert_contains(&err.to_string(), "callback failed");
}

// ------------------------------------------------------------------ load_script

#[test]
fn load_script_reports_syntax_and_runtime_errors_with_a_traceback() {
    let env = Env::new();
    assert!(!env.rt.load_script("local = 5", "syntax.lua"));
    assert_contains(&env.errors()[0].1, "syntax.lua:1:");
    assert_eq!(env.errors()[0].0, "load:syntax.lua");

    assert!(!env
        .rt
        .load_script("local function f() error(\"oops\") end\nf()", "runtime.lua"));
    let errors = env.errors();
    assert_contains(&errors[1].1, "runtime.lua:1: oops");
    assert_contains(&errors[1].1, "stack traceback");
    assert_eq!(errors[1].0, "load:runtime.lua");

    assert!(env.rt.load_script("x = 1", "ok.lua"));
    assert_eq!(env.errors().len(), 2);
}

#[test]
fn print_is_tab_joined_and_routed_to_the_host() {
    let env = Env::new();
    env.run("print('a', 1, 2.5, nil, true) print() print(vector3(1, 2, 3))");
    assert_eq!(
        env.lines(),
        vec!["a\t1\t2.5\tnil\ttrue", "", "vector3(1.0, 2.0, 3.0)"]
    );
}

#[test]
fn environment_globals() {
    let server = Env::on(Rc::new(TestHost::default()), "my-res", Side::Server);
    server.run("log(GetCurrentResourceName(), IsDuplicityVersion(), IS_SERVER, IS_CLIENT)");
    assert_eq!(server.logged()[0], json!(["my-res", true, true, false]));
    assert_eq!(server.rt.resource_name(), "my-res");

    let client = Env::on(Rc::new(TestHost::default()), "test", Side::Client);
    client.run("log(IsDuplicityVersion(), IS_SERVER, IS_CLIENT)");
    assert_eq!(client.logged()[0], json!([false, false, true]));

    let host: Rc<dyn Host> = Rc::new(TestHost::default());
    assert!(LuaRuntime::new("", Side::Server, host).is_err());
}

// --------------------------------------------------------------------- threads

#[test]
fn threads_wait_ordering_is_driven_by_tick() {
    let env = Env::new();
    env.run(
        r#"
        Citizen.CreateThread(function()
          log('a0') ; Citizen.Wait(100) ; log('a100') ; Wait(0) ; log('a-next')
        end)
        CreateThread(function()
          log('b0') ; Wait(50) ; log('b50') ; Wait(50) ; log('b100')
        end)
    "#,
    );
    assert_eq!(env.logged(), vec![json!("a0"), json!("b0")]); // threads start immediately
    assert_eq!(env.rt.pending_threads(), 2);

    env.rt.tick(49.0);
    assert_eq!(env.logged().len(), 2);
    env.rt.tick(50.0);
    assert_eq!(env.logged()[2..], [json!("b50")]);
    env.rt.tick(100.0);
    // Same deadline: the one that started waiting first resumes first.
    // Wait(0) does not resume within the same tick.
    assert_eq!(env.logged()[3..], [json!("a100"), json!("b100")]);
    assert_eq!(env.rt.pending_threads(), 1);
    env.rt.tick(100.0);
    assert_eq!(env.logged()[5..], [json!("a-next")]);
    assert_eq!(env.rt.pending_threads(), 0);
}

#[test]
fn threads_wait_outside_a_thread_is_a_clear_error() {
    let env = Env::new();
    assert!(!env.rt.load_script("Wait(10)", "main.lua"));
    assert_contains(
        &env.errors()[0].1,
        "main.lua:1: Wait() can only be called inside a thread",
    );
}

#[test]
fn threads_wait_outside_a_tick_is_measured_on_the_host_clock() {
    let env = Env::new();
    env.host.clock.set(5.0);
    env.run("CreateThread(function() Wait(10) log('woke') end)");
    env.rt.tick(14.0);
    assert_eq!(env.logged(), Vec::<Json>::new());
    env.rt.tick(15.0);
    assert_eq!(env.logged(), vec![json!("woke")]);
}

#[test]
fn set_timeout_fires_once_when_due() {
    let env = Env::new();
    env.run(
        r#"
        Citizen.SetTimeout(30, function() log('late') end)
        SetTimeout(10, function() log('early') ; Wait(5) ; log('early+5') end)
    "#,
    );
    assert_eq!(env.rt.pending_threads(), 2);
    env.rt.tick(9.0);
    assert_eq!(env.logged().len(), 0);
    env.rt.tick(10.0);
    assert_eq!(env.logged(), vec![json!("early")]);
    env.rt.tick(30.0);
    assert_eq!(
        env.logged(),
        vec![json!("early"), json!("early+5"), json!("late")]
    );
    env.rt.tick(1000.0);
    assert_eq!(env.logged().len(), 3);
    assert_eq!(env.rt.pending_threads(), 0);
}

#[test]
fn threads_an_error_kills_only_that_thread_and_is_reported_with_a_traceback() {
    let env = Env::new();
    assert!(env.rt.load_script(
        r#"
    CreateThread(function()
      Wait(10)
      error('boom')
    end)
    CreateThread(function()
      while true do Wait(10) ; log('healthy') end
    end)
    CreateThread(function() local x = nil ; x.field = 1 end)
    SetTimeout(10, function() error('timer broke') end)
  "#,
        "threads.lua"
    ));
    // The third thread failed synchronously, during load_script.
    let errors = env.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "thread");
    assert_contains(&errors[0].1, "attempt to index a nil value");

    env.rt.tick(10.0);
    env.rt.tick(20.0);
    let errors = env.errors();
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[1].0, "thread");
    assert_contains(&errors[1].1, "threads.lua:4: boom");
    assert_contains(&errors[1].1, "stack traceback");
    assert_eq!(errors[2].0, "timeout");
    assert_contains(&errors[2].1, "timer broke");
    assert_eq!(env.logged(), vec![json!("healthy"), json!("healthy")]);
    assert_eq!(env.rt.pending_threads(), 1);
}

#[test]
fn stress_2000_waiting_threads_finish_quickly() {
    let env = Env::new();
    env.run(
        r#"
        finished = 0
        for i = 1, 2000 do
          CreateThread(function()
            for step = 1, 5 do Wait(i % 50) end
            finished = finished + 1
          end)
        end
    "#,
    );
    assert_eq!(env.rt.pending_threads(), 2000);
    let started = Instant::now();
    let mut now = 0.0;
    while now <= 300.0 && env.rt.pending_threads() > 0 {
        env.rt.tick(now);
        now += 1.0;
    }
    let elapsed = started.elapsed();
    env.run("log(finished)");
    assert_eq!(env.logged()[0], json!(2000));
    assert_eq!(env.rt.pending_threads(), 0);
    assert!(
        elapsed.as_millis() < 5000,
        "10,000 resumes took {elapsed:?}"
    );

    // Idle ticks must be cheap.
    let idle_start = Instant::now();
    for i in 0..20_000 {
        env.rt.tick(1000.0 + f64::from(i));
    }
    let idle = idle_start.elapsed();
    assert!(idle.as_millis() < 2000, "20,000 idle ticks took {idle:?}");
}

#[test]
fn threads_wait_inside_pcall_and_natives_re_entering_the_runtime() {
    let env = Env::new();
    let host = env.host.clone();
    env.native("emit", move |_, (name, arg): (String, Value)| {
        let arg = lua_to_json(&arg).map_err(mlua::Error::runtime)?;
        Ok(host.runtimes()[0].trigger_event(&name, &[arg], 99, false))
    });
    env.native("callBack", |_, function: mlua::Function| {
        function.call::<Value>(21)
    });
    env.run(
        r#"
        AddEventHandler('inner', function(x) log('inner', x, source) ; Wait(5) ; log('inner done') end)
        CreateThread(function()
          Wait(1)
          log(emit('inner', 'v'))
          log(callBack(function(n) return n * 2 end))
          local ok, err = pcall(function() Wait(2) ; error('after wait', 0) end)
          log(ok, err, source == nil)
        end)
    "#,
    );
    env.rt.tick(1.0);
    assert_eq!(
        env.logged(),
        vec![json!(["inner", "v", 99]), json!(1), json!(42)]
    );
    env.rt.tick(3.0);
    assert_eq!(env.logged()[3], json!([false, "after wait", true]));
    env.rt.tick(6.0);
    assert_eq!(env.logged()[4], json!("inner done"));
    assert_eq!(env.rt.pending_threads(), 0);
}

#[test]
fn threads_odd_wait_arguments_are_treated_as_zero() {
    let env = Env::new();
    env.run("CreateThread(function() Wait() ; Wait(-5) ; Wait(0/0) ; Wait('3') ; log('done') end)");
    for _ in 0..3 {
        env.rt.tick(0.0);
    }
    assert_eq!(env.logged().len(), 0);
    env.rt.tick(2.0);
    assert_eq!(env.logged().len(), 0);
    env.rt.tick(3.0);
    assert_eq!(env.logged(), vec![json!("done")]);
}

#[test]
fn threads_script_coroutines_still_work() {
    let env = Env::new();
    env.run(
        r#"
        local gen = coroutine.wrap(function() for i = 1, 3 do coroutine.yield(i) end end)
        log(gen(), gen(), gen())
        local co = coroutine.create(function(a) local b = coroutine.yield(a + 1) ; error('inner ' .. b) end)
        log(coroutine.resume(co, 1))
        log(coroutine.resume(co, 'x'))
        log(coroutine.status(co), select('#', pcall(error)), xpcall(error, function(m) return 'handled ' .. m end, 'e'))
    "#,
    );
    let logged = env.logged();
    assert_eq!(logged[0], json!([1, 2, 3]));
    assert_eq!(logged[1], json!([true, 2]));
    assert_eq!(logged[2][0], json!(false));
    assert_contains(logged[2][1].as_str().unwrap(), "inner x");
    assert_eq!(logged[3], json!(["dead", 2, false, "handled e"]));
}

// ---------------------------------------------------------------------- events

#[test]
fn events_local_handlers_arguments_and_the_source_global() {
    let env = Env::new();
    env.run(
        r#"
        AddEventHandler('greet', function(name, data) log('first', source, name, data.n) end)
        AddEventHandler('greet', function(name) log('second', source, name) end)
        source = 'outer'
    "#,
    );
    assert_eq!(
        env.rt
            .trigger_event("greet", &[json!("bob"), json!({ "n": 1 })], 7, false),
        2
    );
    assert_eq!(
        env.logged(),
        vec![json!(["first", 7, "bob", 1]), json!(["second", 7, "bob"])]
    );
    assert_eq!(env.rt.trigger_event("unknown", &[], 0, false), 0);
    env.run("log(source)");
    assert_eq!(env.logged()[2], json!("outer")); // restored after the handlers
}

#[test]
fn events_from_net_is_gated_by_register_net_event() {
    let env = Env::new();
    env.run(
        r#"
        AddEventHandler('local:only', function() log('local:only') end)
        RegisterNetEvent('net:a')
        AddEventHandler('net:a', function(x) log('net:a', x, source) end)
        RegisterNetEvent('net:b', function(x) log('net:b', x) end)
        RegisterNetEvent('net:silent')
    "#,
    );
    assert_eq!(env.rt.trigger_event("local:only", &[], 3, true), 0);
    assert_eq!(env.rt.trigger_event("local:only", &[], 3, false), 1);
    assert_eq!(env.rt.trigger_event("net:a", &[json!(1)], 3, true), 1);
    assert_eq!(env.rt.trigger_event("net:b", &[json!(2)], 3, true), 1);
    assert_eq!(env.rt.trigger_event("net:silent", &[], 3, true), 0);
    assert_eq!(
        env.logged(),
        vec![
            json!("local:only"),
            json!(["net:a", 1, 3]),
            json!(["net:b", 2])
        ]
    );
}

#[test]
fn events_remove_event_handler_including_from_inside_a_dispatch() {
    let env = Env::new();
    env.run(
        r#"
        local second
        local first = AddEventHandler('e', function()
          log('first')
          RemoveEventHandler(second) -- removed mid-dispatch: must not run
        end)
        second = AddEventHandler('e', function() log('second') end)
        AddEventHandler('e', function() log('third') end)
        function removeFirst() RemoveEventHandler(first) end
    "#,
    );
    assert_eq!(env.rt.trigger_event("e", &[], 0, false), 2);
    assert_eq!(env.logged(), vec![json!("first"), json!("third")]);
    env.run("removeFirst() removeFirst()"); // removing twice is harmless
    assert_eq!(env.rt.trigger_event("e", &[], 0, false), 1);
    assert_eq!(
        env.logged(),
        vec![json!("first"), json!("third"), json!("third")]
    );
}

#[test]
fn events_a_handler_may_wait_and_keeps_its_own_source() {
    let env = Env::new();
    env.run(
        r#"
        AddEventHandler('slow', function(tag)
          log(tag .. ':start', source)
          Wait(20)
          log(tag .. ':end', source)
        end)
    "#,
    );
    assert_eq!(env.rt.trigger_event("slow", &[json!("a")], 1, false), 1);
    assert_eq!(env.rt.trigger_event("slow", &[json!("b")], 2, false), 1);
    assert_eq!(env.rt.pending_threads(), 2);
    env.rt.tick(20.0);
    assert_eq!(
        env.logged(),
        vec![
            json!(["a:start", 1]),
            json!(["b:start", 2]),
            json!(["a:end", 1]),
            json!(["b:end", 2]),
        ]
    );
}

#[test]
fn events_a_failing_handler_is_reported_and_does_not_stop_the_others() {
    let env = Env::new();
    env.rt.load_script(
        "AddEventHandler('e', function() error('handler broke') end)\nAddEventHandler('e', function() log('ran') end)",
        "ev.lua",
    );
    assert_eq!(env.rt.trigger_event("e", &[], 0, false), 2);
    assert_eq!(env.logged(), vec![json!("ran")]);
    let errors = env.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "event:e");
    assert_contains(&errors[0].1, "ev.lua:1: handler broke");
}

#[test]
fn events_trigger_event_goes_through_the_host() {
    // The host fans the event out to every runtime, including the caller.
    let fan_out = Env::new();
    fan_out.run(
        r#"
        AddEventHandler('ping', function(a, b, c) log(a, b, c) end)
        TriggerEvent('ping', 1, nil, 'three')
    "#,
    );
    assert_eq!(fan_out.logged(), vec![json!([1, null, "three"])]);

    // The host decides: one that does not dispatch means no handler runs.
    let hosted = Env::new();
    *hosted.host.captured_events.borrow_mut() = Some(Vec::new());
    hosted.run(
        r#"
        AddEventHandler('ping', function() log('should not run: the host decides') end)
        TriggerEvent('ping', 'x', { y = 1 })
    "#,
    );
    assert_eq!(
        hosted.host.captured_events.borrow().clone().unwrap(),
        vec![("ping".to_owned(), vec![json!("x"), json!({ "y": 1 })])]
    );
    assert_eq!(hosted.logged().len(), 0);
}

#[test]
fn events_trigger_event_with_passes_rust_callbacks_as_lua_functions() {
    let env = Env::new();
    env.run(
        r#"
        AddEventHandler('playerConnecting', function(name, setKickReason, deferrals)
          log(name, source)
          setKickReason('banned: ' .. name)
          deferrals.done()
        end)
    "#,
    );
    let reason = Rc::new(RefCell::new(String::new()));
    let done = Rc::new(Cell::new(false));
    let build = |lua: &mlua::Lua| {
        let reason = reason.clone();
        let set_kick_reason = lua.create_function(move |_, text: String| {
            *reason.borrow_mut() = text;
            Ok(())
        })?;
        let done = done.clone();
        let deferrals = lua.create_table()?;
        deferrals.set(
            "done",
            lua.create_function(move |_, ()| {
                done.set(true);
                Ok(())
            })?,
        )?;
        (String::from("eve"), set_kick_reason, deferrals).into_lua_multi(lua)
    };
    assert_eq!(
        env.rt
            .trigger_event_with("playerConnecting", 12, false, build),
        1
    );
    assert_eq!(env.logged(), vec![json!(["eve", 12])]);
    assert_eq!(*reason.borrow(), "banned: eve");
    assert!(done.get());

    // The builder is not called when nothing listens.
    let count = env.rt.trigger_event_with("nobody", 0, false, |_| {
        panic!("arguments built for an event without handlers")
    });
    assert_eq!(count, 0);
}

// -------------------------------------------------------------------- commands

#[test]
fn commands_registration_dispatch_and_arguments() {
    let env = Env::new();
    env.run(
        r#"
        RegisterCommand('heal', function(source, args, raw)
          log(source, args, raw)
        end, true)
        RegisterCommand('slow', function(source) Wait(5) log('slow done', source) end)
        RegisterCommand('bare', function(source, args) log(#args, type(args)) end)
    "#,
    );
    assert!(env.rt.has_command("heal"));
    assert!(!env.rt.has_command("nope"));
    assert!(env.rt.is_command_restricted("heal"));
    assert!(!env.rt.is_command_restricted("slow"));
    assert!(!env.rt.is_command_restricted("nope"));

    let args = ["me".to_owned(), "50".to_owned()];
    assert!(env.rt.run_command("heal", 4, &args, "heal me 50"));
    assert!(!env.rt.run_command("nope", 4, &[], "nope"));
    assert_eq!(env.logged(), vec![json!([4, ["me", "50"], "heal me 50"])]);

    assert!(env.rt.run_command("slow", 9, &[], "slow"));
    env.rt.tick(5.0);
    assert_eq!(env.logged()[1], json!(["slow done", 9]));

    assert!(env.rt.run_command("bare", 0, &[], "bare"));
    assert_eq!(env.logged()[2], json!([0, "table"]));
}

#[test]
fn commands_a_failing_command_is_reported() {
    let env = Env::new();
    env.run("RegisterCommand('bad', function() error('nope') end)");
    assert!(env.rt.run_command("bad", 0, &[], "bad"));
    let errors = env.errors();
    assert_eq!(errors[0].0, "command:bad");
    assert_contains(&errors[0].1, "nope");
}

// --------------------------------------------------------------------- exports

#[test]
fn exports_publish_has_export_and_call_export() {
    let env = Env::on(Rc::new(TestHost::default()), "bank", Side::Server);
    env.run(
        r#"
        exports('getBalance', function(id, bonus) return 100 + id + (bonus or 0), 'second result' end)
        exports('describe', function() return { owner = 'bob', accounts = { 1, 2 } } end)
        exports('fail', function() error('vault locked') end)
        exports('later', function() Wait(10) log('later finished') end)
        exports('handle', function() return function() end end)
        -- a resource can call its own exports
        log(exports.bank:getBalance(1), exports['bank']:getBalance(2, 10), exports.bank.getBalance(3))
    "#,
    );
    assert_eq!(env.logged(), vec![json!([101, 112, 103])]);
    assert!(env.rt.has_export("getBalance"));
    assert!(!env.rt.has_export("missing"));
    assert_eq!(
        env.rt.call_export("getBalance", &[json!(5)]),
        Ok(json!(105))
    );
    assert_eq!(
        env.rt.call_export("describe", &[]),
        Ok(json!({ "owner": "bob", "accounts": [1, 2] }))
    );
    assert_contains(
        &env.rt.call_export("fail", &[]).unwrap_err(),
        "vault locked",
    );
    assert_eq!(
        env.rt.call_export("missing", &[]),
        Err("resource 'bank' has no export named 'missing'".to_owned())
    );
    assert_eq!(env.rt.call_export("handle", &[]), Ok(Json::Null));

    assert_eq!(env.rt.call_export("later", &[]), Ok(Json::Null));
    env.rt.tick(10.0);
    assert_eq!(env.logged()[1], json!("later finished"));
    assert_eq!(env.errors().len(), 0);
}

#[test]
fn exports_calling_another_resource_through_the_host() {
    let host = Rc::new(TestHost::default());
    let bank = Env::on(host.clone(), "bank", Side::Server);
    let shop = Env::on(host, "shop", Side::Server);

    bank.run(
        r#"
        local balances = { alice = 50 }
        exports('withdraw', function(who, amount)
          if (balances[who] or 0) < amount then error('insufficient funds') end
          balances[who] = balances[who] - amount
          return balances[who]
        end)
    "#,
    );
    shop.run(
        r#"
        log(exports.bank:withdraw('alice', 20))
        log(exports['bank']:withdraw('alice', 5))
        local ok, err = pcall(function() return exports.bank:withdraw('alice', 999) end)
        log(ok, err)
        ok, err = pcall(function() return exports.ghost:anything() end)
        log(ok, err)
        log(pcall(function() exports.bank.withdraw = print end))
    "#,
    );
    let logged = shop.logged();
    assert_eq!(logged[0], json!(30));
    assert_eq!(logged[1], json!(25));
    assert_eq!(logged[2][0], json!(false));
    assert_contains(logged[2][1].as_str().unwrap(), "insufficient funds");
    assert_eq!(logged[3][0], json!(false));
    assert_contains(logged[3][1].as_str().unwrap(), "no such resource 'ghost'");
    assert_eq!(logged[4][0], json!(false));
    assert_contains(logged[4][1].as_str().unwrap(), "read-only");
}

// ------------------------------------------------------------------------ json

#[test]
fn json_encode_and_decode() {
    let env = Env::new();
    env.run(
        r#"
        log(json.encode({ 1, 2, 3 }), json.encode({ a = 'é', b = true }), json.encode('s'), json.encode(nil), json.encode({}))
        local t = json.decode('{"list":[1,2.5,"x"],"obj":{"k":null,"7":"seven"},"flag":false}')
        log(#t.list, math.type(t.list[1]), t.list[2], t.list[3], t.obj.k == nil, t.obj['7'], t.flag)
        log(json.decode(json.encode({ pos = vector3(1, 2, 3) })).pos)
        log(pcall(json.decode, 'not json'))
        log(pcall(json.encode, { f = function() end }))
        log(json.decode('null') == nil, json.decode('"str"'), json.decode('12'))
        log(json.encode({ big = 1795597672181, half = 0.5, nan = 0/0 }), json.decode('1795597672181'))
        log(pcall(json.decode, 12))
    "#,
    );
    let logged = env.logged();
    assert_eq!(
        logged[0],
        json!(["[1,2,3]", r#"{"a":"é","b":true}"#, r#""s""#, "null", "{}"])
    );
    assert_eq!(
        logged[1],
        json!([3, "integer", 2.5, "x", true, "seven", false])
    );
    assert_eq!(logged[2], json!({ "x": 1.0, "y": 2.0, "z": 3.0 }));
    assert_eq!(logged[3][0], json!(false));
    assert_contains(logged[3][1].as_str().unwrap(), "json.decode: ");
    assert_eq!(logged[4][0], json!(false));
    assert_contains(logged[4][1].as_str().unwrap(), "cannot encode a function");
    assert_eq!(logged[5], json!([true, "str", 12]));
    assert_eq!(
        logged[6],
        json!([
            r#"{"big":1795597672181,"half":0.5,"nan":null}"#,
            1_795_597_672_181_i64
        ])
    );
    assert_eq!(logged[7][0], json!(false));
    assert_contains(
        logged[7][1].as_str().unwrap(),
        "json.decode expects a string",
    );
}

// --------------------------------------------------------------------- vector3

#[test]
fn vector3_fields_operators_and_bridge() {
    let env = Env::new();
    env.run(
        r#"
        local a, b = vector3(1, 2, 3), vector3(4, 5, 6)
        local sum, diff, scaled, scaled2, neg = a + b, b - a, a * 2, 2 * a, -a
        log(sum.x, sum.y, sum.z)
        log(diff.x, diff.y, diff.z)
        log(scaled == vector3(2, 4, 6), scaled2 == scaled, neg == vector3(-1, -2, -3))
        log(a == vector3(1, 2, 3), a == b, a ~= b, a == 5)
        log(#vector3(3, 4, 0), tostring(a), tostring(vector3(0.5, -1, 2)))
        log((b / 2).x, vector3().z)
        log(a)
        log(pcall(function() return a + 1 end))
        log(pcall(vector3, 'x', 0, 0))
    "#,
    );
    let logged = env.logged();
    assert_eq!(logged[0], json!([5.0, 7.0, 9.0]));
    assert_eq!(logged[1], json!([3.0, 3.0, 3.0]));
    assert_eq!(logged[2], json!([true, true, true]));
    assert_eq!(logged[3], json!([true, false, true, false]));
    assert_eq!(
        logged[4],
        json!([5.0, "vector3(1.0, 2.0, 3.0)", "vector3(0.5, -1.0, 2.0)"])
    );
    assert_eq!(logged[5], json!([2.0, 0.0]));
    assert_eq!(logged[6], json!({ "x": 1.0, "y": 2.0, "z": 3.0 }));
    assert_eq!(logged[7][0], json!(false));
    assert_contains(logged[7][1].as_str().unwrap(), "non-vector3");
    assert_eq!(logged[8][0], json!(false));
}

// --------------------------------------------------------------------- sandbox

#[test]
fn sandbox_dangerous_libraries_and_functions_are_unavailable() {
    let env = Env::new();
    env.run(
        r#"
        log(io == nil, package == nil, loadfile == nil, dofile == nil)
        log(os.execute == nil, os.exit == nil, os.remove == nil, os.rename == nil, os.tmpname == nil, os.getenv == nil, os.setlocale == nil)
        log(type(os.time()), type(os.clock()), type(os.date()), type(os.difftime(2, 1)))
        log(type(debug.traceback), debug.getinfo == nil, debug.getregistry == nil, debug.sethook == nil, debug.setmetatable == nil, debug.getupvalue == nil)
        local ok, err = pcall(require, 'socket')
        log(ok, err)
        -- text chunks load, bytecode does not
        log(load('return 1 + 1')(), load(string.dump(function() return 1 end)) == nil)
        log(load('return x', 'env', 't', { x = 'from env' })())
        log(load('return x', 'env', 'b', { x = 'mode is ignored' })())
        -- the standard safe libraries are present
        log(type(string.format), type(table.concat), type(math.floor), type(utf8.char), type(coroutine.wrap))
        local count = 0
        for _ in pairs(os) do count = count + 1 end
        for _ in pairs(debug) do count = count + 1 end
        log(count)
    "#,
    );
    let logged = env.logged();
    assert_eq!(logged[0], json!([true, true, true, true]));
    assert_eq!(logged[1], json!([true, true, true, true, true, true, true]));
    assert_eq!(logged[2], json!(["number", "number", "string", "number"]));
    assert_eq!(logged[3], json!(["function", true, true, true, true, true]));
    assert_eq!(logged[4][0], json!(false));
    assert_contains(
        logged[4][1].as_str().unwrap(),
        r#"require("socket") is not available"#,
    );
    assert_eq!(logged[5], json!([2, true]));
    assert_eq!(logged[6], json!("from env"));
    assert_eq!(logged[7], json!("mode is ignored"));
    assert_eq!(
        logged[8],
        json!(["function", "function", "function", "function", "function"])
    );
    assert_eq!(logged[9], json!(5));
}

#[test]
fn sandbox_runtimes_are_isolated_from_each_other() {
    let host = Rc::new(TestHost::default());
    let a = Env::on(host.clone(), "a", Side::Server);
    let b = Env::on(host, "b", Side::Server);
    a.run("shared = 'from a' ; string.custom = true");
    b.run("log(shared == nil, string.custom == nil)");
    assert_eq!(b.logged()[0], json!([true, true]));
}

// -------------------------------------------------------------------- watchdog

const SMALL_LIMIT: u64 = 200_000;
const LIMIT_MESSAGE: &str =
    "resource 'test' exceeded instruction limit (200000 instructions without yielding)";

#[test]
fn watchdog_kills_an_infinite_loop_at_load() {
    let env = Env::with_limit(SMALL_LIMIT);
    assert!(!env
        .rt
        .load_script("local n = 0\nwhile true do n = n + 1 end", "spin.lua"));
    let errors = env.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "load:spin.lua");
    assert!(errors[0].1.starts_with(LIMIT_MESSAGE), "{}", errors[0].1);
    assert_contains(&errors[0].1, "stack traceback");
    assert_contains(&errors[0].1, "spin.lua:2:");

    // The runtime stays usable, with a fresh budget for every entry.
    for _ in 0..3 {
        env.rt.load_script(
            "local n = 0 for i = 1, 50000 do n = n + i end log(n)",
            "ok.lua",
        );
    }
    assert_eq!(env.logged(), vec![json!(1_250_025_000_i64); 3]);
    assert_eq!(env.errors().len(), 1);
}

#[test]
fn watchdog_kills_an_infinite_loop_in_a_thread() {
    let env = Env::with_limit(SMALL_LIMIT);
    // Spinning from its first instruction: killed during load_script, which
    // itself carries on (the thread has its own budget).
    assert!(env.rt.load_script(
        r#"
        CreateThread(function() while true do end end)
        CreateThread(function()
          Wait(10)
          log('about to spin')
          while true do end
        end)
        CreateThread(function()
          while true do Wait(10) ; log('healthy') end
        end)
        SetTimeout(10, function() repeat until false end)
        log('load finished')
    "#,
        "threads.lua"
    ));
    assert_eq!(env.logged(), vec![json!("load finished")]);
    assert_eq!(env.errors().len(), 1);
    assert_eq!(env.errors()[0].0, "thread");
    assert!(env.errors()[0].1.starts_with(LIMIT_MESSAGE));

    // Spinning after a Wait: killed inside tick, the other threads still run.
    env.rt.tick(10.0);
    env.rt.tick(20.0);
    let errors = env.errors();
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[1].0, "thread");
    assert!(errors[1].1.starts_with(LIMIT_MESSAGE));
    assert_contains(&errors[1].1, "threads.lua:6:");
    assert_eq!(errors[2].0, "timeout");
    assert!(errors[2].1.starts_with(LIMIT_MESSAGE));
    assert_eq!(
        env.logged()[1..],
        [json!("about to spin"), json!("healthy"), json!("healthy")]
    );
    assert_eq!(env.rt.pending_threads(), 1);
}

#[test]
fn watchdog_kills_an_infinite_loop_in_an_event_handler_command_and_export() {
    let env = Env::with_limit(SMALL_LIMIT);
    env.run(
        r#"
        AddEventHandler('spin', function() while true do end end)
        AddEventHandler('spin', function(x) log('second handler', x) end)
        RegisterCommand('spin', function() while true do end end)
        exports('spin', function() while true do end end)
        exports('fine', function() return 'ok' end)
    "#,
    );
    assert_eq!(env.rt.trigger_event("spin", &[json!(1)], 0, false), 2);
    assert_eq!(env.logged(), vec![json!(["second handler", 1])]);
    assert!(env.rt.run_command("spin", 0, &[], "spin"));
    let export_error = env.rt.call_export("spin", &[]).unwrap_err();
    assert!(export_error.starts_with(LIMIT_MESSAGE), "{export_error}");

    let errors = env.errors();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].0, "event:spin");
    assert!(errors[0].1.starts_with(LIMIT_MESSAGE));
    assert_eq!(errors[1].0, "command:spin");
    assert!(errors[1].1.starts_with(LIMIT_MESSAGE));

    // Later calls still work.
    assert_eq!(env.rt.trigger_event("spin", &[json!(2)], 0, false), 2);
    assert_eq!(env.logged()[1], json!(["second handler", 2]));
    assert_eq!(env.rt.call_export("fine", &[]), Ok(json!("ok")));
}

#[test]
fn watchdog_cannot_be_swallowed_and_covers_script_coroutines() {
    let env = Env::with_limit(SMALL_LIMIT);
    let spins = [
        // pcall / xpcall / coroutine.resume must not let the script carry on.
        "while true do pcall(function() while true do end end) end",
        "while true do xpcall(function() while true do end end, function(m) return m end) end",
        "while true do coroutine.resume(coroutine.create(function() while true do end end)) end",
        // Coroutines the script creates itself are covered by the same hook.
        "coroutine.wrap(function() while true do end end)()",
        "local co = coroutine.wrap(function() while true do coroutine.yield() end end)\nwhile true do co() end",
        // A chunk named like the bootstrap gets no exemption.
        "load('while true do end', '@sixm/bootstrap.lua')()",
        // Inside a metamethod.
        "local t = setmetatable({}, { __index = function(t, k) while true do end end })\nreturn t.x",
    ];
    for (index, code) in spins.iter().enumerate() {
        let started = Instant::now();
        assert!(
            !env.rt.load_script(code, "spin.lua"),
            "spin {index} was not stopped"
        );
        let errors = env.errors();
        assert_eq!(errors.len(), index + 1);
        assert_contains(&errors[index].1, LIMIT_MESSAGE);
        assert!(
            started.elapsed().as_secs() < 20,
            "spin {index} took too long"
        );
    }
    // The same inside a scheduler thread, after a Wait.
    env.rt.load_script(
        "CreateThread(function() Wait(1) while true do pcall(function() while true do end end) end end)",
        "thread.lua",
    );
    env.rt.tick(1.0);
    assert_eq!(env.errors().len(), spins.len() + 1);
    assert_eq!(env.errors()[spins.len()].0, "thread");

    // A caught, ordinary error is still an ordinary error afterwards.
    env.rt.load_script("log(pcall(error, 'plain'))", "ok.lua");
    assert_eq!(env.logged(), vec![json!([false, "plain"])]);
}

#[test]
fn watchdog_budget_is_per_resume_and_nested_calls_do_not_refill_it() {
    let env = Env::with_limit(SMALL_LIMIT);
    // Far more than the limit in total, but never without yielding.
    env.run(
        r#"
        CreateThread(function()
          local total = 0
          for round = 1, 20 do
            for i = 1, 50000 do total = total + 1 end
            Wait(0)
          end
          log(total)
        end)
    "#,
    );
    for _ in 0..25 {
        env.rt.tick(0.0);
    }
    assert_eq!(env.logged(), vec![json!(1_000_000)]);
    assert_eq!(env.errors().len(), 0);

    // Each handler gets its own budget, but the caller's keeps counting
    // across the nested calls: this loop never yields and must die.
    env.rt.load_script(
        "AddEventHandler('noop', function() end)\nwhile true do TriggerEvent('noop') end",
        "nested.lua",
    );
    let errors = env.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "load:nested.lua");
    assert_contains(&errors[0].1, LIMIT_MESSAGE);
}

#[test]
fn watchdog_can_be_disabled() {
    let env = Env::with_limit(0);
    env.run("local n = 0 for i = 1, 2000000 do n = n + 1 end log(n)");
    assert_eq!(env.logged(), vec![json!(2_000_000)]);
}

// ----------------------------------------------------------------- re-entrancy

#[test]
fn re_entrant_calls_through_the_host_do_not_panic() {
    let env = Env::on(Rc::new(TestHost::default()), "self", Side::Server);
    let host = env.host.clone();
    env.native("hostTick", move |_, now: f64| {
        host.runtimes()[0].tick(now);
        Ok(())
    });
    let host = env.host.clone();
    env.native("hostCommand", move |_, name: String| {
        let rt = host.runtimes()[0].clone();
        Ok((
            rt.has_command(&name),
            rt.run_command(&name, 5, &[], &name),
            rt.pending_threads(),
        ))
    });
    let host = env.host.clone();
    env.native("hostLoad", move |_, code: String| {
        Ok(host.runtimes()[0].load_script(&code, "nested.lua"))
    });
    env.run(
        r#"
        exports('double', function(n) return n * 2 end)
        RegisterCommand('cmd', function(source) log('cmd', source) end)
        AddEventHandler('a', function(n)
          log('a', n)
          TriggerEvent('b', n + 1)          -- Lua -> host -> this runtime
          log('a done', exports.self:double(n))
        end)
        AddEventHandler('b', function(n)
          log('b', n)
          TriggerEvent('c', n + 1)          -- and again, three levels deep
          print('b printed')
        end)
        AddEventHandler('c', function(n)
          log('c', n, hostCommand('cmd'))
          log(hostLoad("log('nested load') CreateThread(function() Wait(0) log('nested thread') end)"))
          hostTick(0)
        end)
        CreateThread(function()
          Wait(0)
          TriggerEvent('a', 1)
        end)
    "#,
    );
    env.rt.tick(0.0);
    assert_eq!(
        env.logged(),
        vec![
            json!(["a", 1]),
            json!(["b", 2]),
            json!(["cmd", 5]),
            json!(["c", 3, true, true, 0]),
            json!("nested load"),
            json!(true),
            json!("nested thread"),
            json!(["a done", 2]),
        ]
    );
    assert_eq!(env.lines(), vec!["b printed"]);
    assert_eq!(env.errors().len(), 0);
    assert_eq!(env.rt.pending_threads(), 0);
}

// --------------------------------------------------------------------- destroy

#[test]
fn destroy_every_method_becomes_a_safe_no_op() {
    let env = Env::new();
    env.run(
        r#"
        AddEventHandler('e', function() log('handler') end)
        RegisterCommand('c', function() log('command') end)
        exports('x', function() return 1 end)
        CreateThread(function() while true do Wait(1) log('tick') end end)
    "#,
    );
    assert!(!env.rt.is_destroyed());
    env.rt.destroy();
    env.rt.destroy();
    assert!(env.rt.is_destroyed());

    assert!(!env.rt.load_script("log('nope')", "after.lua"));
    assert_eq!(env.rt.trigger_event("e", &[], 0, false), 0);
    assert_eq!(
        env.rt
            .trigger_event_with("e", 0, false, |_| panic!("built")),
        0
    );
    assert!(!env.rt.run_command("c", 0, &[], "c"));
    assert!(!env.rt.has_command("c"));
    assert!(!env.rt.is_command_restricted("c"));
    assert!(!env.rt.has_export("x"));
    assert_eq!(
        env.rt.call_export("x", &[]),
        Err("resource 'test' is not running".to_owned())
    );
    assert_eq!(env.rt.pending_threads(), 0);
    env.rt.tick(100.0);
    // The state is still there for the host, but empty.
    assert_eq!(env.rt.lua().globals().pairs::<Value, Value>().count(), 0);
    assert!(env
        .rt
        .lua()
        .globals()
        .get::<Value>("AddEventHandler")
        .unwrap()
        .is_nil());
    assert_eq!(env.logged().len(), 0);
    assert_eq!(env.errors().len(), 0);
}

#[test]
fn destroy_releases_what_the_resource_held() {
    let env = Env::new();
    let token = Rc::new(());
    let held = token.clone();
    env.native("holdsToken", move |_, ()| Ok(Rc::strong_count(&held)));
    env.run(
        r#"
        local big = string.rep('x', 4 * 1024 * 1024)
        CreateThread(function() while true do Wait(1000) ; local _ = #big end end)
        string.stash = big
    "#,
    );
    let before = env.rt.lua().used_memory();
    assert!(before > 4 * 1024 * 1024);
    assert_eq!(Rc::strong_count(&token), 2);
    env.rt.destroy();
    assert_eq!(Rc::strong_count(&token), 1, "host natives are dropped");
    assert!(env.rt.lua().used_memory() < before - 4 * 1024 * 1024);
}

#[test]
fn destroy_called_from_inside_a_running_script_closes_after_it_unwinds() {
    let env = Env::new();
    let host = env.host.clone();
    env.native("stopSelf", move |_, ()| {
        host.runtimes()[0].destroy();
        Ok(())
    });
    env.run(
        r#"
        RegisterCommand('stop', function()
          stopSelf()
          log('still finishing the current call')
          TriggerEvent('late')
        end)
        AddEventHandler('late', function() log('must not run: destroyed') end)
    "#,
    );
    assert!(env.rt.run_command("stop", 0, &[], "stop"));
    assert_eq!(
        env.logged(),
        vec![json!("still finishing the current call")]
    );
    assert!(env.rt.is_destroyed());
    assert!(!env.rt.load_script("log('after')", "after.lua"));
    assert!(!env.rt.has_command("stop"));
    assert_eq!(env.rt.lua().globals().pairs::<Value, Value>().count(), 0);
    assert_eq!(env.errors().len(), 0);
}

#[test]
fn destroy_mid_dispatch_stops_the_remaining_handlers_and_threads() {
    let env = Env::new();
    let host = env.host.clone();
    env.native("stopSelf", move |_, ()| {
        host.runtimes()[0].destroy();
        Ok(())
    });
    env.run(
        r#"
        AddEventHandler('stop', function() log('first') ; stopSelf() ; log('first done') end)
        AddEventHandler('stop', function() log('second: must not run') end)
    "#,
    );
    assert_eq!(env.rt.trigger_event("stop", &[], 0, false), 1);
    assert_eq!(env.logged(), vec![json!("first"), json!("first done")]);

    let env = Env::new();
    let host = env.host.clone();
    env.native("stopSelf", move |_, ()| {
        host.runtimes()[0].destroy();
        Ok(())
    });
    env.run(
        r#"
        CreateThread(function() Wait(1) ; log('one') ; stopSelf() ; Wait(1) ; log('never') end)
        CreateThread(function() Wait(1) ; log('two: must not run') end)
    "#,
    );
    env.rt.tick(1.0);
    env.rt.tick(2.0);
    assert_eq!(env.logged(), vec![json!("one")]);
    assert_eq!(env.errors().len(), 0);
}

// ---------------------------------------------------------- shipped resources

/// Every script under `resources/` loads and runs unchanged on Lua 5.4 with
/// the natives stubbed, and the chat resource works end to end.
#[test]
fn shipped_resources_run_unchanged() {
    fn scripts(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read resources") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                scripts(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "lua")
                && path.file_name().is_some_and(|name| name != "manifest.lua")
            {
                found.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources");
    let mut found = Vec::new();
    scripts(&root, &mut found);
    assert!(found.len() >= 5, "resources not found under {root:?}");

    for path in found {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let resource = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let side = if file == "client.lua" {
            Side::Client
        } else {
            Side::Server
        };
        let env = Env::on(Rc::new(TestHost::default()), &resource, side);
        let sent = Rc::new(RefCell::new(Vec::new()));
        let outbox = sent.clone();
        env.native("TriggerClientEvent", move |_, args: Variadic<Value>| {
            let args: Vec<Json> = args
                .iter()
                .map(lua_to_json)
                .collect::<Result<_, _>>()
                .map_err(mlua::Error::runtime)?;
            outbox.borrow_mut().push(Json::Array(args));
            Ok(())
        });
        env.native("GetConvarInt", |_, (_, default): (String, i64)| Ok(default));
        env.native("GetConvar", |_, (_, default): (String, String)| Ok(default));
        env.native("GetNumPlayers", |_, ()| Ok(0));
        // Any other native: a function that returns nothing.
        env.run("setmetatable(_G, { __index = function() return function() end end })");

        let code = std::fs::read_to_string(&path).expect("read script");
        let chunk = format!("{resource}/{file}");
        assert!(
            env.rt.load_script(&code, &chunk),
            "{chunk}: {:?}",
            env.errors()
        );
        env.rt.tick(1.0);
        assert_eq!(env.errors(), Vec::new(), "{chunk}");

        if resource == "chat" && side == Side::Server {
            assert_eq!(
                env.rt.trigger_event(
                    "chat:send",
                    &[json!(
                        "hi
 there"
                    )],
                    3,
                    true
                ),
                1
            );
            assert!(env.rt.has_command("say") && env.rt.is_command_restricted("say"));
            assert_eq!(
                env.rt.call_export("announce", &[json!("soon")]),
                Ok(Json::Null)
            );
            assert_eq!(
                *sent.borrow(),
                vec![
                    json!(["chat:message", -1, "player 3", "hi there"]),
                    json!(["chat:message", -1, "server", "soon"]),
                ]
            );
            assert_eq!(env.errors(), Vec::new());
        }
    }
}

// ------------------------------------------------------------------ benchmarks

/// Not a test: prints timings. Run with
/// `cargo test -p sixm-lua --release -- --ignored --nocapture benchmarks`.
#[test]
#[ignore = "benchmark; run explicitly in release mode"]
fn benchmarks() {
    const LOOP: &str = "local n = 0 for i = 1, 100000000 do n = n + (i % 7) end";
    let tight_loop = |limit: u64| {
        let env = Env::with_limit(limit);
        let mut best = f64::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            assert!(env.rt.load_script(LOOP, "bench.lua"));
            best = best.min(started.elapsed().as_secs_f64());
        }
        best
    };
    let without = tight_loop(0);
    let with = tight_loop(u64::MAX);
    println!(
        "tight loop, 100M iterations: watchdog off {:.3}s, on {:.3}s, overhead {:+.1}%",
        without,
        with,
        (with / without - 1.0) * 100.0
    );

    let env = Env::new();
    env.run("for i = 1, 2000 do CreateThread(function() while true do Wait(1) end end) end");
    const IDLE: u32 = 1_000_000;
    let started = Instant::now();
    for _ in 0..IDLE {
        env.rt.tick(0.0); // nothing is due before t = 1
    }
    println!(
        "idle tick: {:.0} ns",
        started.elapsed().as_secs_f64() * 1e9 / f64::from(IDLE)
    );

    const ROUNDS: u32 = 500;
    let started = Instant::now();
    for round in 1..=ROUNDS {
        env.rt.tick(f64::from(round));
    }
    println!(
        "resume: {:.0} ns per thread wake-up (2,000 threads x {ROUNDS} ticks)",
        started.elapsed().as_secs_f64() * 1e9 / f64::from(ROUNDS * 2000)
    );
    assert_eq!(env.rt.pending_threads(), 2000);
    assert_eq!(env.errors().len(), 0);
}
