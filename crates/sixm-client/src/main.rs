//! sixm-client: a headless client. It connects, runs the server's client-side
//! Lua, prints what happens, and takes commands from standard input.

use std::cell::Cell;
use std::io::BufRead;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sixm_client::{Client, ClientListener, ClientOptions, DEFAULT_PORT, VERSION};

const HELP: &str = "\
Usage: sixm-client [options]

  -H, --host <address>   server address            (default: 127.0.0.1)
  -p, --port <number>    server port               (default: 30120)
  -n, --name <name>      player name               (default: Player)
      --pos <x,y,z>      starting position         (default: 0,0,0)
      --wander           walk around at random
      --secure           connect with TLS (for servers behind a TLS reverse proxy)
  -v, --version          print the version
  -h, --help             print this help

While connected, type a command and press Enter. Commands the server's
resources define (such as \"say hello\" or \"players\") work here, plus:

  /pos x y z     move to a position
  /list          list the players you know about
  /quit          disconnect
";

struct Args {
    options: ClientOptions,
    pos: Option<(f32, f32, f32)>,
    wander: bool,
}

fn parse_pos(text: &str, separator: char) -> Option<(f32, f32, f32)> {
    let parts: Vec<f32> = text
        .split(separator)
        .filter(|p| !p.is_empty())
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    match parts[..] {
        [x, y, z] if x.is_finite() && y.is_finite() && z.is_finite() => Some((x, y, z)),
        _ => None,
    }
}

/// `Ok(None)` means the request was answered (help or version) and there is nothing to run.
fn parse_args(argv: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut args = Args {
        options: ClientOptions::default(),
        pos: None,
        wander: false,
    };
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
                print!("SixM headless client {VERSION}\n\n{HELP}");
                return Ok(None);
            }
            "-v" | "--version" => {
                println!("{VERSION}");
                return Ok(None);
            }
            "-H" | "--host" => args.options.host = value("--host")?,
            "-n" | "--name" => args.options.name = value("--name")?,
            "-p" | "--port" => {
                args.options.port = value("--port")?
                    .parse()
                    .map_err(|_| "--port must be a number from 1 to 65535".to_string())?;
            }
            "--pos" => {
                let text = value("--pos")?;
                args.pos = Some(
                    parse_pos(&text, ',')
                        .ok_or("--pos expects three numbers, e.g. --pos 10,20,0")?,
                );
            }
            "--wander" => args.wander = true,
            "--secure" => args.options.secure = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if args.options.port == 0 {
        args.options.port = DEFAULT_PORT;
    }
    Ok(Some(args))
}

/// Prints what happens and, with `--wander`, walks the player about.
struct Terminal {
    wander: bool,
    heading: Cell<f32>,
    last_step: Cell<Instant>,
    /// xorshift state; good enough for a random walk.
    rng: Cell<u64>,
    done: Cell<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Terminal {
    /// Uniform in [0, 1).
    fn random(&self) -> f32 {
        let mut x = self.rng.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.set(x);
        (x >> 40) as f32 / (1u64 << 24) as f32
    }
}

impl ClientListener for Terminal {
    fn print(&self, line: &str) {
        println!("{line}");
    }

    fn player_joined(&self, id: u16, name: &str) {
        println!("* {name} (#{id}) joined");
    }

    fn player_left(&self, id: u16, name: &str, reason: &str) {
        if reason.is_empty() {
            println!("* {name} (#{id}) left");
        } else {
            println!("* {name} (#{id}) left: {reason}");
        }
    }

    fn tick(&self, client: &Client) {
        if !self.wander {
            return;
        }
        // Walk in a slowly turning line, as a stand-in for a player moving about.
        const SPEED: f32 = 4.0; // metres per second
        let dt = self
            .last_step
            .replace(Instant::now())
            .elapsed()
            .as_secs_f32();
        let heading = (self.heading.get() + (self.random() - 0.5) * 400.0 * dt).rem_euclid(360.0);
        self.heading.set(heading);
        let (sin, cos) = heading.to_radians().sin_cos();
        client.update_state(|s| {
            s.vx = cos * SPEED;
            s.vy = sin * SPEED;
            s.x += s.vx * dt;
            s.y += s.vy * dt;
            s.heading = heading;
        });
    }

    fn disconnected(&self, reason: &str) {
        println!("disconnected: {reason}");
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

fn handle_line(client: &Client, line: &str) {
    let raw = line.trim();
    if raw.is_empty() {
        return;
    }
    let bare = raw.strip_prefix('/').unwrap_or(raw);
    let (command, rest) = bare.split_once(char::is_whitespace).unwrap_or((bare, ""));
    match command {
        "quit" | "exit" => client.disconnect("Quit."),
        "pos" => match parse_pos(rest, ' ') {
            Some((x, y, z)) => {
                client.set_coords(x, y, z);
                println!("moved to {x}, {y}, {z}");
            }
            None => println!("usage: /pos x y z"),
        },
        "list" => {
            println!("#{} {} (you)", client.id(), client.name());
            for p in client.players() {
                match p.state {
                    Some(s) => {
                        println!("#{} {} at {:.1}, {:.1}, {:.1}", p.id, p.name, s.x, s.y, s.z)
                    }
                    None => println!("#{} {} (out of range)", p.id, p.name),
                }
            }
        }
        _ => client.execute_command(raw),
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
    local.block_on(&runtime, async move {
        let (done_tx, mut done) = tokio::sync::oneshot::channel();
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            | 1;
        let terminal = Rc::new(Terminal {
            wander: args.wander,
            heading: Cell::new((seed % 360) as f32),
            last_step: Cell::new(Instant::now()),
            rng: Cell::new(seed),
            done: Cell::new(Some(done_tx)),
        });

        let (host, port) = (args.options.host.clone(), args.options.port);
        let client = match Client::connect(args.options, terminal).await {
            Ok(client) => client,
            Err(reason) => {
                eprintln!("could not join {host}:{port}: {reason}");
                return ExitCode::FAILURE;
            }
        };
        if let Some((x, y, z)) = args.pos {
            client.set_coords(x, y, z);
        }
        println!(
            "joined \"{}\" as {} (#{}), {} other player(s) online",
            client.server_name(),
            client.name(),
            client.id(),
            client.players().len()
        );

        // Standard input is read on its own thread; lines are handed to the client's thread.
        let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });

        let mut input_open = true;
        loop {
            tokio::select! {
                line = lines.recv(), if input_open => match line {
                    Some(line) => handle_line(&client, &line),
                    // Standard input ended (no terminal attached): stay connected.
                    None => input_open = false,
                },
                _ = &mut done => break,
                _ = tokio::signal::ctrl_c() => client.disconnect("Quit."),
            }
        }
        ExitCode::SUCCESS
    })
}
