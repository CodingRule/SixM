//! `server.cfg` parser.
//!
//! Syntax: one command per line, arguments separated by whitespace.
//!   - `#` or `//` starts a comment when it appears at the start of a token
//!     (so an unquoted `https://example.com` is NOT cut short).
//!   - "double quoted" arguments may contain spaces; `\"` and `\\` are escapes.
//!   - Blank lines are ignored.
//!
//! Nothing here fails on bad input: problems are reported as [`ConfigError`]
//! entries so the caller can print them all at once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One parsed line: a lower-cased command name and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
    /// 1-based line number in the source text.
    pub line: usize,
}

/// A syntax or validation problem on one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub message: String,
}

/// Result of [`parse_config`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedConfig {
    pub commands: Vec<Command>,
    pub errors: Vec<ConfigError>,
}

/// Server settings found in a config. Only what was actually configured is `Some`,
/// so defaults are never clobbered.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub name: Option<String>,
    pub max_players: Option<u32>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub tick_rate: Option<u32>,
    pub stream_distance: Option<f32>,
    /// Proxies whose `X-Forwarded-For` is believed, by address.
    pub trusted_proxies: Option<Vec<std::net::IpAddr>>,
    pub tags: Option<Vec<String>>,
}

/// Result of [`apply_config`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppliedConfig {
    /// Every convar set with `set`, `setr`, `sets` or a bare `sv_*` line.
    pub convars: BTreeMap<String, String>,
    /// Names of the convars set with `setr`, in first-seen order.
    pub replicated: Vec<String>,
    /// Resources to start, in order, without duplicates.
    pub resources: Vec<String>,
    pub settings: Settings,
    /// Convars set with `sets`: these are public (shown in `/info.json`).
    pub info_vars: BTreeMap<String, String>,
    /// Commands this module does not know; the caller decides what to do with them.
    pub unknown: Vec<Command>,
    pub errors: Vec<ConfigError>,
}

/// Result of [`load_config`].
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub commands: Vec<Command>,
    /// `applied.errors` holds the parse and validation errors together, in line order.
    pub applied: AppliedConfig,
}

fn is_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\x0c' | '\x0b')
}

/// Split one line into tokens.
fn tokenize_line(line: &str) -> Result<Vec<String>, &'static str> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && is_space(chars[i]) {
            i += 1;
        }
        if i >= n || chars[i] == '#' || (chars[i] == '/' && chars.get(i + 1) == Some(&'/')) {
            break; // end of line or comment
        }
        let mut token = String::new();
        while i < n && !is_space(chars[i]) {
            if chars[i] != '"' {
                token.push(chars[i]);
                i += 1;
                continue;
            }
            i += 1;
            let mut closed = false;
            while i < n {
                let ch = chars[i];
                let next = chars.get(i + 1).copied();
                if ch == '\\' && matches!(next, Some('"' | '\\')) {
                    token.extend(next);
                    i += 2;
                } else if ch == '"' {
                    closed = true;
                    i += 1;
                    break;
                } else {
                    token.push(ch);
                    i += 1;
                }
            }
            if !closed {
                return Err("unterminated quoted string");
            }
        }
        tokens.push(token);
    }
    Ok(tokens)
}

/// Split config text into commands. Lines that cannot be tokenised are reported and skipped.
pub fn parse_config(text: &str) -> ParsedConfig {
    let mut parsed = ParsedConfig::default();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // BOM
    for (idx, raw) in text.split('\n').enumerate() {
        let line = idx + 1;
        let mut tokens = match tokenize_line(raw) {
            Ok(tokens) => tokens,
            Err(message) => {
                parsed.errors.push(ConfigError {
                    line,
                    message: message.to_string(),
                });
                continue;
            }
        };
        if tokens.is_empty() {
            continue;
        }
        let name = tokens.remove(0).to_lowercase();
        if name.is_empty() {
            parsed.errors.push(ConfigError {
                line,
                message: "empty command name".to_string(),
            });
            continue;
        }
        parsed.commands.push(Command {
            name,
            args: tokens,
            line,
        });
    }
    parsed
}

/// An optionally signed run of ASCII digits within `min..=max`.
fn parse_integer(value: &str, min: i64, max: i64) -> Option<i64> {
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value
        .parse::<i64>()
        .ok()
        .filter(|n| (min..=max).contains(n))
}

/// `port`, `host:port` or `[v6]:port`.
fn parse_endpoint(value: &str) -> Option<(Option<String>, u16)> {
    let s = value.trim();
    let (host, port_text) = match s.rfind(':') {
        None => (None, s),
        Some(colon) => {
            let host = &s[..colon];
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            (
                (!host.is_empty()).then(|| host.to_string()),
                &s[colon + 1..],
            )
        }
    };
    let port = parse_integer(port_text, 0, 65535)?;
    Some((host, port as u16))
}

/// Convars that also drive a server setting. `None` when `key` is not one of them,
/// otherwise the validation result.
fn apply_setting(settings: &mut Settings, key: &str, value: &str) -> Option<Result<(), String>> {
    let result = match key {
        "sv_hostname" => {
            if value.trim().is_empty() {
                Err("sv_hostname must not be empty".to_string())
            } else {
                settings.name = Some(value.to_string());
                Ok(())
            }
        }
        "sv_maxclients" => match parse_integer(value, 1, 65535) {
            Some(n) => {
                settings.max_players = Some(n as u32);
                Ok(())
            }
            None => Err(format!(
                "sv_maxclients must be an integer between 1 and 65535 (got \"{value}\")"
            )),
        },
        "sv_port" => match parse_integer(value, 0, 65535) {
            Some(n) => {
                settings.port = Some(n as u16);
                Ok(())
            }
            None => Err(format!(
                "sv_port must be an integer between 0 and 65535 (got \"{value}\")"
            )),
        },
        "sv_host" => {
            if value.trim().is_empty() {
                Err("sv_host must not be empty".to_string())
            } else {
                settings.host = Some(value.trim().to_string());
                Ok(())
            }
        }
        "sv_tickrate" => match parse_integer(value, 1, 240) {
            Some(n) => {
                settings.tick_rate = Some(n as u32);
                Ok(())
            }
            None => Err(format!(
                "sv_tickrate must be an integer between 1 and 240 (got \"{value}\")"
            )),
        },
        "sv_streamdistance" => match value.trim().parse::<f32>().ok().filter(|n| n.is_finite()) {
            Some(n) => {
                settings.stream_distance = Some(n);
                Ok(())
            }
            None => Err(format!(
                "sv_streamdistance must be a number (got \"{value}\")"
            )),
        },
        "sv_proxies" => {
            let mut proxies = Vec::new();
            for part in value.split([',', ' ']).filter(|p| !p.trim().is_empty()) {
                match part.trim().parse::<std::net::IpAddr>() {
                    Ok(ip) => proxies.push(ip),
                    Err(_) => {
                        return Some(Err(format!(
                            "sv_proxies must be IP addresses (got \"{}\")",
                            part.trim()
                        )))
                    }
                }
            }
            settings.trusted_proxies = Some(proxies);
            Ok(())
        }
        _ => return None,
    };
    Some(result)
}

fn is_setting_convar(key: &str) -> bool {
    matches!(
        key,
        "sv_hostname"
            | "sv_maxclients"
            | "sv_port"
            | "sv_host"
            | "sv_tickrate"
            | "sv_streamdistance"
            | "sv_proxies"
    )
}

#[derive(PartialEq)]
enum ConvarKind {
    Private,
    Replicated,
    Public,
}

fn set_convar(cfg: &mut AppliedConfig, line: usize, key: &str, value: String, kind: ConvarKind) {
    let lower = key.to_lowercase();
    match kind {
        ConvarKind::Private => {}
        ConvarKind::Replicated => {
            if !cfg.replicated.iter().any(|k| k == key) {
                cfg.replicated.push(key.to_string());
            }
        }
        ConvarKind::Public => {
            cfg.info_vars.insert(key.to_string(), value.clone());
            if lower == "tags" {
                let tags = value
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from);
                cfg.settings.tags = Some(tags.collect());
            }
        }
    }
    if let Some(Err(message)) = apply_setting(&mut cfg.settings, &lower, &value) {
        cfg.errors.push(ConfigError { line, message });
    }
    cfg.convars.insert(key.to_string(), value);
}

/// Interpret parsed commands: convars, resources to start and server settings.
pub fn apply_config(commands: &[Command]) -> AppliedConfig {
    let mut cfg = AppliedConfig::default();
    for cmd in commands {
        let Command { name, args, line } = cmd;
        let line = *line;
        let error = |cfg: &mut AppliedConfig, message: String| {
            cfg.errors.push(ConfigError { line, message })
        };
        match name.as_str() {
            "set" | "setr" | "sets" => {
                if args.len() < 2 {
                    error(&mut cfg, format!("{name} expects a name and a value"));
                    continue;
                }
                let kind = match name.as_str() {
                    "setr" => ConvarKind::Replicated,
                    "sets" => ConvarKind::Public,
                    _ => ConvarKind::Private,
                };
                set_convar(&mut cfg, line, &args[0], args[1..].join(" "), kind);
            }
            "ensure" | "start" | "stop" => {
                let [resource] = args.as_slice() else {
                    error(
                        &mut cfg,
                        format!("{name} expects exactly one resource name"),
                    );
                    continue;
                };
                if resource.is_empty() {
                    error(
                        &mut cfg,
                        format!("{name} expects exactly one resource name"),
                    );
                } else if name == "stop" {
                    cfg.resources.retain(|r| r != resource);
                } else if !cfg.resources.contains(resource) {
                    cfg.resources.push(resource.clone());
                }
            }
            "endpoint_add_tcp" => {
                let endpoint = match args.as_slice() {
                    [value] => parse_endpoint(value),
                    _ => None,
                };
                match endpoint {
                    Some((host, port)) => {
                        cfg.settings.port = Some(port);
                        if host.is_some() {
                            cfg.settings.host = host;
                        }
                    }
                    None => error(
                        &mut cfg,
                        "endpoint_add_tcp expects \"host:port\" with a port between 0 and 65535"
                            .to_string(),
                    ),
                }
            }
            // Accepted for familiarity; SixM only listens on TCP (WebSocket).
            "endpoint_add_udp" => {}
            // Bare form: `sv_hostname "My Server"`.
            _ if is_setting_convar(name) => {
                if args.is_empty() {
                    error(&mut cfg, format!("{name} expects a value"));
                } else {
                    set_convar(&mut cfg, line, name, args.join(" "), ConvarKind::Private);
                }
            }
            _ => cfg.unknown.push(cmd.clone()),
        }
    }
    cfg
}

/// Read, parse and interpret a config file. Fails only when the file cannot be
/// read; syntax and validation problems land in `applied.errors`.
pub fn load_config(path: impl AsRef<Path>) -> std::io::Result<LoadedConfig> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    let parsed = parse_config(&String::from_utf8_lossy(&bytes));
    let mut applied = apply_config(&parsed.commands);
    applied.errors.extend(parsed.errors);
    applied.errors.sort_by_key(|e| e.line);
    Ok(LoadedConfig {
        path: path.to_path_buf(),
        commands: parsed.commands,
        applied,
    })
}
