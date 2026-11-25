//! Puts the pieces together: reads `server.cfg`, starts the server and its
//! resources, and defines the operator's console commands.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::config::{apply_config, parse_config};
use crate::core::{Server, ServerOptions};
use crate::logger::Logger;
use crate::resources::ResourceManager;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AppOptions {
    /// `server.cfg`. When the file does not exist, defaults are used and every resource is started.
    pub config_path: Option<PathBuf>,
    pub resources_dir: PathBuf,
    /// Overrides the port from the configuration.
    pub port: Option<u16>,
    pub logger: Logger,
}

/// A running server with its resources.
#[derive(Clone)]
pub struct App {
    pub server: Server,
    pub resources: ResourceManager,
    log: Logger,
    /// Set by the `quit` console command.
    quit: Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl App {
    /// Start a server. Must be called inside a tokio `LocalSet`.
    pub async fn start(options: AppOptions) -> Result<App, String> {
        let log = options.logger.clone();
        let mut server_options = ServerOptions {
            logger: log.clone(),
            build: VERSION.to_string(),
            ..ServerOptions::default()
        };
        let mut convars = BTreeMap::new();
        // None = no configuration file: start everything that is found.
        let ensure: Option<Vec<String>>;

        match options.config_path.as_deref() {
            Some(path) if path.is_file() => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let parsed = parse_config(&text);
                let applied = apply_config(&parsed.commands);
                let file = file_name(path);
                let mut errors: Vec<_> =
                    parsed.errors.iter().chain(applied.errors.iter()).collect();
                errors.sort_by_key(|e| e.line);
                for e in errors {
                    log.warn(format!("{file}: line {}: {}", e.line, e.message));
                }
                for unknown in &applied.unknown {
                    log.warn(format!(
                        "{file}: line {}: unknown command \"{}\"",
                        unknown.line, unknown.name
                    ));
                }
                let s = applied.settings;
                if let Some(v) = s.port {
                    server_options.port = v;
                }
                if let Some(v) = s.host {
                    server_options.host = v;
                }
                if let Some(v) = s.name {
                    server_options.name = v;
                }
                if let Some(v) = s.max_players {
                    server_options.max_players = v;
                }
                if let Some(v) = s.tick_rate {
                    server_options.tick_rate = v;
                }
                if let Some(v) = s.stream_distance {
                    server_options.stream_distance = v;
                }
                if let Some(v) = s.tags {
                    server_options.tags = v;
                }
                server_options.vars = applied.info_vars.into_iter().collect();
                convars = applied.convars.into_iter().collect();
                ensure = Some(applied.resources);
            }
            Some(path) => {
                log.warn(format!(
                    "no configuration at {}; using defaults and starting every resource",
                    path.display()
                ));
                ensure = None;
            }
            None => ensure = None,
        }
        if let Some(port) = options.port {
            server_options.port = port;
        }

        let server = Server::new(server_options)?;
        let resources = ResourceManager::new(
            server.clone(),
            options.resources_dir.clone(),
            log.clone(),
            convars,
        );
        let found = resources.scan();
        log.info(format!(
            "found {} resource(s) in {}",
            found.len(),
            options.resources_dir.display()
        ));

        let app = App {
            server: server.clone(),
            resources: resources.clone(),
            log: log.clone(),
            quit: Rc::new(RefCell::new(None)),
        };
        app.add_console_commands();

        let port = server
            .start()
            .await
            .map_err(|e| format!("could not listen: {e}"))?;
        for name in ensure.unwrap_or(found) {
            resources.start(&name);
        }
        log.info(format!(
            "{} is listening on port {port} (http://127.0.0.1:{port}/)",
            server.name()
        ));
        Ok(app)
    }

    /// Stop the resources, disconnect everyone and close the listener. Safe to call twice.
    pub async fn stop(&self, reason: &str) {
        self.resources.stop_all();
        self.server.stop(reason).await;
    }

    /// Run one line typed in the server console.
    pub fn console_line(&self, line: &str) {
        let raw = line.trim();
        if raw.is_empty() {
            return;
        }
        if !self.resources.execute_command(raw, 0) {
            let name = raw.split_whitespace().next().unwrap_or(raw);
            self.log
                .warn(format!("unknown command: {name} (try \"help\")"));
        }
    }

    /// Resolves when the operator types `quit`.
    pub fn quit_requested(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.quit.borrow_mut() = Some(tx);
        rx
    }

    fn add_console_commands(&self) {
        let r = &self.resources;

        {
            let (resources, log) = (r.clone(), self.log.clone());
            r.add_builtin("help", "list console commands", move |_, _| {
                let rows: Vec<String> = resources.builtins().iter().map(|(name, help)| format!("  {name:<10} {help}")).collect();
                log.info(format!(
                    "console commands:\n{}\n  (resources add their own, e.g. \"say\" and \"players\")",
                    rows.join("\n")
                ));
            });
        }
        {
            let (server, resources, log) = (self.server.clone(), r.clone(), self.log.clone());
            r.add_builtin("status", "show players and uptime", move |_, _| {
                log.info(format!(
                    "{} player(s), tick {}, {} resource(s) running",
                    server.player_count(),
                    server.tick(),
                    resources.running().len()
                ));
                for id in server.player_ids() {
                    if let Some(p) = server.player(id) {
                        log.info(format!(
                            "  #{} {}  {}ms  {}",
                            p.id, p.name, p.ping, p.endpoint
                        ));
                    }
                }
            });
        }
        {
            let (resources, log) = (r.clone(), self.log.clone());
            r.add_builtin("resources", "list resources", move |_, _| {
                for name in resources.available() {
                    let state = if resources.is_running(&name) {
                        "started"
                    } else {
                        "stopped"
                    };
                    log.info(format!("  {state}  {name}"));
                }
            });
        }
        {
            let (resources, log) = (r.clone(), self.log.clone());
            r.add_builtin("refresh", "look for new resources on disk", move |_, _| {
                log.info(format!("found {} resource(s)", resources.scan().len()));
            });
        }
        for (command, help) in [
            ("start", "start a resource"),
            ("ensure", "start or restart a resource"),
            ("restart", "restart a resource"),
            ("stop", "stop a resource"),
        ] {
            let (resources, log) = (r.clone(), self.log.clone());
            r.add_builtin(command, help, move |args, _| {
                let Some(name) = args.first() else {
                    log.warn(format!("usage: {command} <resource>"));
                    return;
                };
                match command {
                    "start" => {
                        resources.start(name);
                    }
                    "stop" => {
                        if !resources.stop(name) {
                            log.warn(format!("\"{name}\" is not running"));
                        }
                    }
                    _ => {
                        resources.restart(name);
                    }
                }
            });
        }
        {
            let (server, log) = (self.server.clone(), self.log.clone());
            r.add_builtin(
                "kick",
                "kick a player: kick <id> [reason]",
                move |args, _| {
                    let id = args.first().and_then(|a| a.parse::<u16>().ok());
                    let reason = if args.len() > 1 {
                        args[1..].join(" ")
                    } else {
                        "Kicked by the server.".to_string()
                    };
                    if !id.is_some_and(|id| server.kick(id, &reason)) {
                        log.warn("usage: kick <id> [reason]  (see \"status\" for ids)");
                    }
                },
            );
        }
        {
            let quit = self.quit.clone();
            r.add_builtin("quit", "stop the server", move |_, _| {
                if let Some(tx) = quit.borrow_mut().take() {
                    let _ = tx.send(());
                }
            });
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}
