//! `server.cfg` parsing and the logger.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use sixm_server::config::{apply_config, load_config, parse_config, Command, Settings};
use sixm_server::logger::{Level, Logger, Stream};

fn cmd(name: &str, args: &[&str], line: usize) -> Command {
    Command {
        name: name.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        line,
    }
}

fn settings_of(text: &str) -> Settings {
    apply_config(&parse_config(text).commands).settings
}

#[test]
fn parse_config_commands_blank_lines_and_line_numbers() {
    let parsed = parse_config("ensure chat\n\n   \nSET  foo   bar\r\n\tstart spawn");
    assert_eq!(parsed.errors, []);
    assert_eq!(
        parsed.commands,
        [
            cmd("ensure", &["chat"], 1),
            cmd("set", &["foo", "bar"], 4),
            cmd("start", &["spawn"], 5)
        ]
    );
}

#[test]
fn parse_config_hash_and_slash_comments_full_line_and_trailing() {
    let text = [
        "# a comment",
        "// another",
        "ensure chat # trailing",
        "ensure map // trailing",
        "sets url https://example.com/a#frag",
        "set motd \"has # and // inside\"",
        "   # indented comment",
    ]
    .join("\n");
    let parsed = parse_config(&text);
    assert_eq!(parsed.errors, []);
    assert_eq!(
        parsed.commands,
        [
            cmd("ensure", &["chat"], 3),
            cmd("ensure", &["map"], 4),
            cmd("sets", &["url", "https://example.com/a#frag"], 5),
            cmd("set", &["motd", "has # and // inside"], 6),
        ]
    );
}

#[test]
fn parse_config_quoted_strings_and_escapes() {
    let parsed = parse_config(
        "sv_hostname \"My \\\"Cool\\\" Server\"\nset path \"C:\\\\games\\\\sixm\"\nset empty \"\"\nset glued pre\"mid dle\"post\n",
    );
    assert_eq!(parsed.errors, []);
    let args: Vec<Vec<String>> = parsed.commands.into_iter().map(|c| c.args).collect();
    assert_eq!(
        args,
        [
            vec!["My \"Cool\" Server"],
            vec!["path", "C:\\games\\sixm"],
            vec!["empty", ""],
            vec!["glued", "premid dlepost"],
        ]
    );
}

#[test]
fn parse_config_unterminated_quote_is_reported_with_its_line_and_skipped() {
    let parsed = parse_config("ensure a\nset motd \"oops\nensure b\n");
    let firsts: Vec<&str> = parsed.commands.iter().map(|c| c.args[0].as_str()).collect();
    assert_eq!(firsts, ["a", "b"]);
    assert_eq!(parsed.errors.len(), 1);
    assert_eq!(parsed.errors[0].line, 2);
    assert!(parsed.errors[0].message.contains("unterminated"));
}

#[test]
fn parse_config_bom_and_empty_input() {
    assert_eq!(
        parse_config("\u{feff}ensure a").commands,
        [cmd("ensure", &["a"], 1)]
    );
    assert_eq!(parse_config("").commands, []);
    assert_eq!(parse_config("").errors, []);
}

#[test]
fn apply_config_a_typical_server_cfg() {
    let parsed = parse_config(
        r#"
endpoint_add_tcp "0.0.0.0:30125"
endpoint_add_udp "0.0.0.0:30125"
sv_hostname "Leonida Nights"
sv_maxclients 48
set sv_tickrate 30
set sv_streamdistance 350.5
sets tags "roleplay, eu ,, drift"
sets locale en-GB
setr voice_enabled true
set mysql_string "server=localhost;db=x"
ensure chat
start spawn
ensure chat
ensure hud
stop hud
frobnicate 1 2
"#,
    );
    assert_eq!(parsed.errors, []);
    let cfg = apply_config(&parsed.commands);
    assert_eq!(cfg.errors, []);
    assert_eq!(
        cfg.settings,
        Settings {
            port: Some(30125),
            host: Some("0.0.0.0".to_string()),
            name: Some("Leonida Nights".to_string()),
            max_players: Some(48),
            tick_rate: Some(30),
            stream_distance: Some(350.5),
            tags: Some(vec![
                "roleplay".to_string(),
                "eu".to_string(),
                "drift".to_string()
            ]),
        }
    );
    assert_eq!(cfg.resources, ["chat", "spawn"]);
    let info_vars: BTreeMap<String, String> =
        [("tags", "roleplay, eu ,, drift"), ("locale", "en-GB")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into();
    assert_eq!(cfg.info_vars, info_vars);
    assert_eq!(cfg.convars["mysql_string"], "server=localhost;db=x");
    assert_eq!(cfg.convars["voice_enabled"], "true");
    assert_eq!(cfg.convars["sv_hostname"], "Leonida Nights");
    assert_eq!(cfg.convars["locale"], "en-GB");
    assert_eq!(cfg.replicated, ["voice_enabled"]);
    assert_eq!(cfg.unknown, [cmd("frobnicate", &["1", "2"], 17)]);
    assert!(
        !cfg.info_vars.contains_key("mysql_string"),
        "private convars are not public"
    );
}

#[test]
fn apply_config_settings_stay_unset_unless_configured_sv_port_and_ipv6_endpoint() {
    assert_eq!(apply_config(&[]).settings, Settings::default());
    assert_eq!(
        settings_of("sv_port 4000"),
        Settings {
            port: Some(4000),
            ..Settings::default()
        }
    );
    assert_eq!(
        settings_of("endpoint_add_tcp \"[::1]:4001\""),
        Settings {
            host: Some("::1".to_string()),
            port: Some(4001),
            ..Settings::default()
        }
    );
    assert_eq!(
        settings_of("endpoint_add_tcp 4002"),
        Settings {
            port: Some(4002),
            ..Settings::default()
        }
    );
    assert_eq!(
        apply_config(&parse_config("set a one two three").commands).convars["a"],
        "one two three"
    );
}

#[test]
fn apply_config_invalid_values_are_reported_with_line_numbers() {
    let text = [
        "sv_maxclients lots",
        "sv_maxclients 0",
        "set sv_tickrate 12.5",
        "sv_port 70000",
        "endpoint_add_tcp \"0.0.0.0:http\"",
        "sv_streamdistance far",
        "set lonely",
        "ensure",
        "ensure a b",
        "sv_hostname",
        "sv_maxclients 16",
    ]
    .join("\n");
    let cfg = apply_config(&parse_config(&text).commands);
    let lines: Vec<usize> = cfg.errors.iter().map(|e| e.line).collect();
    assert_eq!(lines, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert!(cfg.errors[0].message.contains("sv_maxclients"));
    assert!(cfg.errors[2].message.contains("sv_tickrate"));
    assert!(cfg.errors[4].message.contains("endpoint_add_tcp"));
    assert_eq!(
        cfg.settings,
        Settings {
            max_players: Some(16),
            ..Settings::default()
        },
        "only the valid value was applied"
    );
    assert!(cfg.resources.is_empty());
}

#[test]
fn load_config_reads_a_file_and_merges_parse_and_validation_errors_in_line_order() {
    let dir = std::env::temp_dir().join(format!("sixm-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("server.cfg");
    std::fs::write(
        &file,
        "sv_hostname \"From Disk\"\nsv_maxclients nope\nset x \"broken\nensure core\n",
    )
    .unwrap();

    let loaded = load_config(&file);
    let missing = load_config(dir.join("missing.cfg"));
    std::fs::remove_dir_all(&dir).unwrap();

    let cfg = loaded.unwrap();
    assert_eq!(cfg.path, file);
    assert_eq!(cfg.applied.settings.name.as_deref(), Some("From Disk"));
    assert_eq!(cfg.applied.resources, ["core"]);
    let lines: Vec<usize> = cfg.applied.errors.iter().map(|e| e.line).collect();
    assert_eq!(lines, [2, 3]);
    assert_eq!(cfg.commands.len(), 3);
    assert_eq!(missing.unwrap_err().kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn logger_plain_output_level_filtering_children_and_streams() {
    let lines: Rc<RefCell<Vec<(Stream, String)>>> = Rc::default();
    let sink = lines.clone();
    let plain = Logger::to_sink("srv", false, move |stream, line| {
        sink.borrow_mut().push((stream, line.to_string()))
    })
    .with_level(Level::Info);
    plain.debug("hidden");
    plain.info("hello {\"a\":1}");
    plain.error(format!("bad {}", "Error: boom"));
    plain.child("net").warn("careful");
    {
        let lines = lines.borrow();
        assert_eq!(lines.len(), 3);
        let (stream, line) = &lines[0];
        assert_eq!(*stream, Stream::Stdout);
        // HH:MM:SS, then the padded level, the scope and the text.
        let (time, rest) = line.split_at(8);
        assert!(time.bytes().enumerate().all(|(i, b)| if i % 3 == 2 {
            b == b':'
        } else {
            b.is_ascii_digit()
        }));
        assert_eq!(rest, " INFO  [srv] hello {\"a\":1}\n");
        assert!(!line.contains("\x1b["));
        assert_eq!(lines[1].0, Stream::Stderr);
        assert!(lines[1].1.ends_with(" ERROR [srv] bad Error: boom\n"));
        assert_eq!(lines[2].0, Stream::Stderr);
        assert!(lines[2].1.ends_with(" WARN  [srv:net] careful\n"));
    }

    assert_eq!(plain.level(), Level::Info);
    plain.set_level(Level::Silent);
    plain.clone().error("nothing");
    assert_eq!(lines.borrow().len(), 3);

    let (captured, seen) = Logger::capture();
    captured.debug("kept");
    assert_eq!(seen.borrow().len(), 1);
    assert!(seen.borrow()[0].ends_with(" DEBUG kept\n"));

    Logger::silent().child("x").info("ignored");
    assert_eq!(Logger::silent().level(), Level::Silent);
}
