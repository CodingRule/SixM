//! End-to-end: a real server with the bundled resources, and real clients
//! running the client-side Lua the server hands them.

use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use sixm_client::{Client, ClientListener, ClientOptions};
use sixm_server::app::{App, AppOptions};
use sixm_server::logger::Logger;

/// Run a test body on the single-threaded runtime the server and clients need.
fn run<F: Future<Output = ()>>(body: F) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, body);
}

/// Poll until `check` returns something.
async fn until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// What one client printed.
#[derive(Default)]
struct Lines(RefCell<Vec<String>>);

impl Lines {
    fn any(&self, test: impl Fn(&str) -> bool) -> Option<()> {
        self.0.borrow().iter().any(|l| test(l)).then_some(())
    }

    fn contains(&self, needle: &str) -> Option<()> {
        self.any(|l| l.contains(needle))
    }

    fn len(&self) -> usize {
        self.0.borrow().len()
    }
}

impl ClientListener for Lines {
    fn print(&self, line: &str) {
        self.0.borrow_mut().push(line.to_string());
    }
}

struct Harness {
    app: App,
    dir: PathBuf,
    server_log: Rc<RefCell<Vec<String>>>,
    clients: RefCell<Vec<Client>>,
}

impl Harness {
    async fn boot() -> Harness {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sixm-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources");
        copy_dir(&bundled, &dir.join("resources"));
        // A resource whose handler is local only: clients must not be able to trigger it.
        let probe = dir.join("resources/probe");
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(
            probe.join("manifest.lua"),
            "name 'probe'\nserver_script 'server.lua'\n",
        )
        .unwrap();
        std::fs::write(
            probe.join("server.lua"),
            "AddEventHandler('secret:local', function() TriggerClientEvent('chat:message', -1, 'x', 'leaked') end)\n",
        )
        .unwrap();

        let config = dir.join("server.cfg");
        std::fs::write(
            &config,
            "sv_hostname \"Test Server\"\nsv_maxclients 8\nsv_tickrate 60\nsv_streamdistance 100\n\
             endpoint_add_tcp \"127.0.0.1:0\"\nset welcome_message \"hi there\"\n\
             ensure chat\nensure welcome\nensure hello\nensure probe\n",
        )
        .unwrap();

        let (logger, server_log) = Logger::capture();
        let app = App::start(AppOptions {
            config_path: Some(config),
            resources_dir: dir.join("resources"),
            port: None,
            logger,
        })
        .await
        .unwrap();
        Harness {
            app,
            dir,
            server_log,
            clients: RefCell::new(Vec::new()),
        }
    }

    async fn try_join(&self, name: &str) -> Result<(Client, Rc<Lines>), String> {
        let lines = Rc::new(Lines::default());
        let options = ClientOptions {
            port: self.app.server.port(),
            name: name.to_string(),
            ..ClientOptions::default()
        };
        let client = Client::connect(options, lines.clone()).await?;
        self.clients.borrow_mut().push(client.clone());
        Ok((client, lines))
    }

    async fn join(&self, name: &str) -> (Client, Rc<Lines>) {
        self.try_join(name).await.unwrap()
    }

    async fn shutdown(self) {
        for client in self.clients.borrow().iter() {
            client.disconnect("Test over.");
        }
        self.app.stop("Test over.").await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn bundled_resources_start_without_script_errors() {
    run(async {
        let h = Harness::boot().await;
        assert_eq!(
            h.app.resources.running(),
            ["chat", "welcome", "hello", "probe"]
        );
        let errors: Vec<String> = h
            .server_log
            .borrow()
            .iter()
            .filter(|l| l.contains("ERROR"))
            .cloned()
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        h.shutdown().await;
    });
}

#[test]
fn a_joining_player_is_welcomed_and_announced() {
    run(async {
        let h = Harness::boot().await;
        let (alice, alice_lines) = h.join("Alice").await;
        until("the welcome message", || {
            alice_lines.contains("<server> hi there")
        })
        .await;
        let (bob, _) = h.join("Bob").await;
        until("the join announcement", || {
            alice_lines.contains("Bob joined (2 online)")
        })
        .await;
        assert_eq!(alice.player(bob.id()).unwrap().name, "Bob");
        assert_eq!(bob.player(alice.id()).unwrap().name, "Alice");
        h.shutdown().await;
    });
}

#[test]
fn chat_goes_from_a_client_through_server_lua_to_every_client() {
    run(async {
        let h = Harness::boot().await;
        let (alice, alice_lines) = h.join("Alice").await;
        let (_bob, bob_lines) = h.join("Bob").await;
        alice.execute_command("say hello \"big world\"");
        until("chat at Bob", || {
            bob_lines.contains("<Alice> hello big world")
        })
        .await;
        until("chat echoed to Alice", || {
            alice_lines.contains("<Alice> hello big world")
        })
        .await;
        h.shutdown().await;
    });
}

#[test]
fn net_event_round_trip() {
    run(async {
        let h = Harness::boot().await;
        let (alice, lines) = h.join("Alice").await;
        alice.execute_command("/hello");
        until("the pong", || {
            lines.any(|l| l.contains("pong: round trip") && l.contains("1 player(s) online"))
        })
        .await;
        h.shutdown().await;
    });
}

#[test]
fn a_command_the_client_does_not_own_runs_on_the_server_for_that_player() {
    run(async {
        let h = Harness::boot().await;
        let (alice, lines) = h.join("Alice").await;
        alice.set_coords(1.0, 2.0, 3.0);
        until("the server to know the position", || {
            h.app.server.player(alice.id()).filter(|p| p.state.z == 3.0)
        })
        .await;
        alice.execute_command("players");
        until("the player list", || {
            lines.any(|l| l.contains("#1 Alice") && l.contains("(1.0, 2.0, 3.0)"))
        })
        .await;
        h.shutdown().await;
    });
}

#[test]
fn players_cannot_run_restricted_commands_or_unregistered_net_events() {
    run(async {
        let h = Harness::boot().await;
        let (alice, _) = h.join("Alice").await;
        let (_bob, bob_lines) = h.join("Bob").await;
        // `say` on the server is console-only; sent raw it must not be broadcast as "console".
        alice.emit_net(
            "__sixm:command",
            vec!["say impersonating the console".into()],
        );
        // An event no resource registered as a net event must be ignored.
        alice.emit_net("secret:local", vec![]);
        alice.execute_command("say marker");
        until("the marker line", || bob_lines.contains("<Alice> marker")).await;
        assert!(
            bob_lines.contains("impersonating").is_none(),
            "a restricted command ran for a player"
        );
        assert!(
            bob_lines.contains("leaked").is_none(),
            "an unregistered net event was delivered"
        );
        // The same event does reach the handler when the server itself triggers it.
        h.app.resources.trigger_event("secret:local", &[], 0, false);
        until("the local event", || bob_lines.contains("leaked")).await;
        h.shutdown().await;
    });
}

#[test]
fn positions_sync_and_respect_the_stream_distance() {
    run(async {
        let h = Harness::boot().await;
        let (alice, _) = h.join("Alice").await;
        let (bob, _) = h.join("Bob").await;
        alice.set_coords(10.0, 20.0, 30.0);
        alice.set_heading(90.0);
        bob.set_coords(12.0, 20.0, 30.0);
        // Alice's first tick may already have sent her starting position; wait for the new one.
        let seen = until("Alice streamed to Bob", || {
            bob.player(alice.id())
                .and_then(|p| p.state)
                .filter(|s| s.heading == 90.0)
        })
        .await;
        assert_eq!(
            (seen.x, seen.y, seen.z, seen.heading),
            (10.0, 20.0, 30.0, 90.0)
        );
        until("Bob streamed to Alice", || {
            alice
                .player(bob.id())
                .and_then(|p| p.state)
                .filter(|s| s.x == 12.0)
        })
        .await;

        bob.set_coords(5000.0, 0.0, 0.0); // well past sv_streamdistance 100
        until("Bob to leave Alice's range", || {
            alice.player(bob.id()).filter(|p| p.state.is_none())
        })
        .await;
        until("Alice to leave Bob's range", || {
            bob.player(alice.id()).filter(|p| p.state.is_none())
        })
        .await;

        bob.set_coords(11.0, 20.0, 30.0);
        until("Bob back in range", || {
            alice
                .player(bob.id())
                .and_then(|p| p.state)
                .filter(|s| s.x == 11.0)
        })
        .await;
        h.shutdown().await;
    });
}

#[test]
fn a_resource_can_refuse_a_connection_in_player_connecting() {
    run(async {
        let h = Harness::boot().await;
        let refused = h
            .try_join("X")
            .await
            .err()
            .expect("a one-letter name is refused");
        assert!(refused.contains("at least 2 characters"), "{refused}");
        assert_eq!(h.app.server.player_count(), 0);
        h.shutdown().await;
    });
}

#[test]
fn leaving_is_announced_and_stopping_a_resource_removes_its_handlers() {
    run(async {
        let h = Harness::boot().await;
        let (alice, lines) = h.join("Alice").await;
        let (bob, _) = h.join("Bob").await;
        let bob_id = bob.id();
        bob.disconnect("Quit.");
        until("the leave announcement", || lines.contains("Bob left")).await;
        assert!(alice.player(bob_id).is_none());

        assert!(h.app.resources.stop("hello"));
        let before = lines.len();
        alice.execute_command("hello"); // the client side still sends the ping; nobody answers
        pause(150).await;
        assert!(!lines.0.borrow()[before..]
            .iter()
            .any(|l| l.contains("pong")));
        h.shutdown().await;
    });
}

#[test]
fn a_script_that_never_yields_does_not_stop_the_server() {
    run(async {
        let h = Harness::boot().await;
        let (alice, lines) = h.join("Alice").await;
        let spin = h.dir.join("resources/spin");
        std::fs::create_dir_all(&spin).unwrap();
        std::fs::write(
            spin.join("manifest.lua"),
            "name 'spin'\nserver_script 'server.lua'\n",
        )
        .unwrap();
        std::fs::write(
            spin.join("server.lua"),
            "CreateThread(function() Wait(0) while true do end end)\n",
        )
        .unwrap();
        h.app.console_line("refresh");
        h.app.console_line("start spin");
        // The runaway thread is killed and reported, and the server keeps serving.
        until("the watchdog report", || {
            h.server_log
                .borrow()
                .iter()
                .any(|l| l.contains("[spin]") && l.contains("instruction limit"))
                .then_some(())
        })
        .await;
        alice.execute_command("/hello");
        until("the pong after the runaway script", || {
            lines.contains("pong: round trip")
        })
        .await;
        h.shutdown().await;
    });
}

#[test]
fn console_commands_work() {
    run(async {
        let h = Harness::boot().await;
        let (_alice, lines) = h.join("Alice").await;
        let (bob, _) = h.join("Bob").await;
        h.app.console_line("say from the console");
        until("the console chat line", || {
            lines.contains("<console> from the console")
        })
        .await;
        h.app
            .console_line(&format!("kick {} being a test", bob.id()));
        until("the kick", || (!bob.is_connected()).then_some(())).await;
        until("the kick announcement", || {
            lines.contains("Bob left (being a test)")
        })
        .await;
        h.app.console_line("nonsense");
        assert!(h
            .server_log
            .borrow()
            .iter()
            .any(|l| l.contains("unknown command: nonsense")));
        h.shutdown().await;
    });
}
