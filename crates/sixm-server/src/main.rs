//! sixm-server: the dedicated server executable.

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;

use sixm_server::app::{App, AppOptions, VERSION};
use sixm_server::logger::Logger;

const HELP: &str = "\
Usage: sixm-server [options]

  -c, --config <file>      configuration file        (default: server.cfg)
  -r, --resources <dir>    resources folder          (default: resources)
  -p, --port <number>      override the port from the configuration
      --no-console         do not read commands from standard input
      --log-level <level>  debug | info | warn | error (default: info)
  -v, --version            print the version
  -h, --help               print this help
";

#[derive(Default)]
struct Args {
    config: Option<String>,
    resources: Option<String>,
    port: Option<u16>,
    no_console: bool,
    log_level: Option<String>,
}

/// `Ok(None)` means the request was answered (help or version) and there is nothing to run.
fn parse_args(argv: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut argv = argv.peekable();
    while let Some(arg) = argv.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| argv.next())
                .ok_or(format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => {
                print!("SixM server {VERSION}\n\n{HELP}");
                return Ok(None);
            }
            "-v" | "--version" => {
                println!("{VERSION}");
                return Ok(None);
            }
            "-c" | "--config" => args.config = Some(value("--config")?),
            "-r" | "--resources" => args.resources = Some(value("--resources")?),
            "-p" | "--port" => {
                let text = value("--port")?;
                args.port = Some(
                    text.parse()
                        .map_err(|_| "--port must be a number from 0 to 65535".to_string())?,
                );
            }
            "--log-level" => args.log_level = Some(value("--log-level")?),
            "--no-console" => args.no_console = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(Some(args))
}

/// Where `server.cfg` and `resources` are looked for: next to the executable
/// when they are there (a release unpacked anywhere), else the working directory.
fn base_dir() -> PathBuf {
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    match beside_exe {
        Some(dir) if dir.join("resources").is_dir() => dir,
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprint!("{message}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    let logger = Logger::new("");
    let logger = match args.log_level.as_deref().map(str::parse) {
        Some(Ok(level)) => logger.with_level(level),
        Some(Err(_)) => {
            eprintln!("--log-level must be debug, info, warn or error");
            return ExitCode::from(2);
        }
        None => logger,
    };

    let base = base_dir();
    let options = AppOptions {
        config_path: Some(base.join(args.config.as_deref().unwrap_or("server.cfg"))),
        resources_dir: base.join(args.resources.as_deref().unwrap_or("resources")),
        port: args.port,
        logger: logger.clone(),
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let local = tokio::task::LocalSet::new();
    let result: Result<(), String> = local.block_on(&runtime, async move {
        let app = App::start(options).await?;

        // Standard input is read on its own thread; lines are handed to the server's thread.
        let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if !args.no_console {
            std::thread::spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
            });
            logger.info("type \"help\" for console commands");
        }

        let mut quit = app.quit_requested();
        let mut console_open = !args.no_console;
        loop {
            tokio::select! {
                line = line_rx.recv(), if console_open => match line {
                    Some(line) => app.console_line(&line),
                    // Standard input ended (no terminal attached): keep serving.
                    None => console_open = false,
                },
                _ = &mut quit => break,
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        app.stop("Server shutting down.").await;
        Ok(())
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
