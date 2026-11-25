//! SixM wire protocol, shared by the server and every client.
//!
//! Transport: one WebSocket per player.
//!   - Text frames carry JSON control messages: `{ "t": <type>, ... }`.
//!   - Binary frames carry entity state, which is sent many times per second
//!     and is therefore packed tightly (little-endian).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod command;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 30120;

pub const MAX_NAME_LENGTH: usize = 32;
pub const MAX_EVENT_NAME_LENGTH: usize = 128;
pub const MAX_JSON_FRAME: usize = 256 * 1024;

/// Event a client sends to run a command line it has no local handler for.
pub const COMMAND_EVENT: &str = "__sixm:command";

/// Binary frame types (first byte).
pub mod bin {
    /// client -> server: this player's own state.
    pub const STATE: u8 = 1;
    /// server -> client: states of the entities relevant to that client.
    pub const SNAPSHOT: u8 = 2;
}

/// Bit flags in [`EntityState::flags`].
pub mod flags {
    pub const IN_VEHICLE: u16 = 1 << 0;
    pub const AIMING: u16 = 1 << 1;
    pub const SHOOTING: u16 = 1 << 2;
    pub const DEAD: u16 = 1 << 3;
    pub const SPRINTING: u16 = 1 << 4;
}

/// x y z | heading | vx vy vz | model | flags
pub const STATE_SIZE: usize = 12 + 4 + 12 + 4 + 2;
const STATE_HEADER: usize = 1 + 4; // type + seq
const SNAPSHOT_HEADER: usize = 1 + 4 + 2; // type + tick + count
const SNAPSHOT_ENTRY: usize = 2 + STATE_SIZE; // id + state

/// What is synchronised for one player.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EntityState {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Degrees.
    pub heading: f32,
    pub vx: f32,
    pub vy: f32,
    pub vz: f32,
    /// Model hash.
    pub model: u32,
    /// [`flags`] bitmask.
    pub flags: u16,
}

impl EntityState {
    /// A state is usable only if every float is finite.
    pub fn is_sane(&self) -> bool {
        [
            self.x,
            self.y,
            self.z,
            self.heading,
            self.vx,
            self.vy,
            self.vz,
        ]
        .iter()
        .all(|f| f.is_finite())
    }

    fn write(&self, out: &mut Vec<u8>) {
        for f in [
            self.x,
            self.y,
            self.z,
            self.heading,
            self.vx,
            self.vy,
            self.vz,
        ] {
            out.extend_from_slice(&f.to_le_bytes());
        }
        out.extend_from_slice(&self.model.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
    }

    fn read(b: &[u8]) -> Self {
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        EntityState {
            x: f(0),
            y: f(4),
            z: f(8),
            heading: f(12),
            vx: f(16),
            vy: f(20),
            vz: f(24),
            model: u32::from_le_bytes([b[28], b[29], b[30], b[31]]),
            flags: u16::from_le_bytes([b[32], b[33]]),
        }
    }
}

/// client -> server.
pub fn encode_state(seq: u32, state: &EntityState) -> Vec<u8> {
    let mut out = Vec::with_capacity(STATE_HEADER + STATE_SIZE);
    out.push(bin::STATE);
    out.extend_from_slice(&seq.to_le_bytes());
    state.write(&mut out);
    out
}

/// `None` when the frame is malformed or holds a non-finite number.
pub fn decode_state(buf: &[u8]) -> Option<(u32, EntityState)> {
    if buf.len() != STATE_HEADER + STATE_SIZE || buf[0] != bin::STATE {
        return None;
    }
    let seq = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
    let state = EntityState::read(&buf[STATE_HEADER..]);
    state.is_sane().then_some((seq, state))
}

/// server -> client.
pub fn encode_snapshot<'a, I>(tick: u32, entities: I) -> Vec<u8>
where
    I: IntoIterator<Item = (u16, &'a EntityState)>,
{
    let mut out = Vec::with_capacity(SNAPSHOT_HEADER + 8 * SNAPSHOT_ENTRY);
    out.push(bin::SNAPSHOT);
    out.extend_from_slice(&tick.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    let mut count: u16 = 0;
    for (id, state) in entities {
        out.extend_from_slice(&id.to_le_bytes());
        state.write(&mut out);
        count += 1;
    }
    out[5..7].copy_from_slice(&count.to_le_bytes());
    out
}

/// `None` when the frame is malformed.
pub fn decode_snapshot(buf: &[u8]) -> Option<(u32, Vec<(u16, EntityState)>)> {
    if buf.len() < SNAPSHOT_HEADER || buf[0] != bin::SNAPSHOT {
        return None;
    }
    let tick = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
    let count = u16::from_le_bytes([buf[5], buf[6]]) as usize;
    if buf.len() != SNAPSHOT_HEADER + count * SNAPSHOT_ENTRY {
        return None;
    }
    let entities = buf[SNAPSHOT_HEADER..]
        .chunks_exact(SNAPSHOT_ENTRY)
        .map(|e| (u16::from_le_bytes([e[0], e[1]]), EntityState::read(&e[2..])))
        .collect();
    Some((tick, entities))
}

/// One entry of the player list in a `welcome`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerInfo {
    pub id: u16,
    pub name: String,
}

/// A client-side script handed out in a `welcome`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientScript {
    pub file: String,
    pub code: String,
}

/// The client side of one resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientResource {
    pub name: String,
    pub scripts: Vec<ClientScript>,
}

/// JSON control messages. The `t` field selects the variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Msg {
    /// client -> server, must be the first frame.
    Hello {
        proto: u32,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Both directions. `src` is set by the server (0 = the server itself).
    Event {
        name: String,
        #[serde(default)]
        args: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src: Option<u16>,
    },
    /// client -> server.
    Ping {
        ts: f64,
    },
    /// server -> client, after an accepted `hello`.
    #[serde(rename_all = "camelCase")]
    Welcome {
        id: u16,
        /// The name the server accepted, after sanitising.
        name: String,
        server_name: String,
        tick_rate: u32,
        max_players: u32,
        /// Everyone already connected, not including the receiver.
        players: Vec<PlayerInfo>,
        resources: Vec<ClientResource>,
    },
    /// server -> client, instead of `welcome`; the connection is closed after it.
    Reject {
        reason: String,
    },
    Join {
        id: u16,
        name: String,
    },
    Leave {
        id: u16,
        #[serde(default)]
        reason: String,
    },
    Pong {
        ts: f64,
    },
    /// server -> client, right before the server closes the connection.
    Kick {
        reason: String,
    },
}

impl Msg {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("control messages always serialise")
    }

    /// `None` when the text is too long or is not a valid control message.
    pub fn decode(text: &str) -> Option<Msg> {
        if text.len() > MAX_JSON_FRAME {
            return None;
        }
        serde_json::from_str(text).ok()
    }
}

/// Trim and bound a player-supplied display name; empty when nothing usable is left.
pub fn sanitize_name(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    cleaned
        .trim()
        .chars()
        .take(MAX_NAME_LENGTH)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EntityState {
        EntityState {
            x: 1.5,
            y: -2.25,
            z: 30.0,
            heading: 90.0,
            vx: 0.5,
            vy: 0.0,
            vz: -1.0,
            model: 0xDEAD_BEEF,
            flags: flags::AIMING | flags::SPRINTING,
        }
    }

    #[test]
    fn state_round_trips_and_has_the_documented_size() {
        let buf = encode_state(7, &sample());
        assert_eq!(buf.len(), 39);
        assert_eq!(decode_state(&buf), Some((7, sample())));
    }

    #[test]
    fn state_bytes_match_the_0_1_implementation() {
        // Bytes produced by the 0.1 (Node.js) implementation for seq 258 and the state below: protocol v1 must not drift.
        let expected: [u8; 39] = [
            1, 2, 1, 0, 0, 0, 0, 128, 63, 0, 0, 0, 64, 0, 0, 64, 64, 0, 0, 180, 66, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 4, 3, 2, 1, 5, 0,
        ];
        let state = EntityState {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            heading: 90.0,
            model: 0x0102_0304,
            flags: 5,
            ..Default::default()
        };
        assert_eq!(encode_state(258, &state), expected);
    }

    #[test]
    fn malformed_and_non_finite_states_are_refused() {
        let mut buf = encode_state(1, &sample());
        assert!(decode_state(&buf[..38]).is_none());
        buf[0] = bin::SNAPSHOT;
        assert!(decode_state(&buf).is_none());
        let nan = EntityState {
            x: f32::NAN,
            ..sample()
        };
        assert!(decode_state(&encode_state(1, &nan)).is_none());
        let inf = EntityState {
            vz: f32::INFINITY,
            ..sample()
        };
        assert!(decode_state(&encode_state(1, &inf)).is_none());
    }

    #[test]
    fn snapshot_round_trips() {
        let a = sample();
        let b = EntityState {
            x: 9.0,
            ..Default::default()
        };
        let buf = encode_snapshot(42, [(1u16, &a), (65535u16, &b)]);
        assert_eq!(buf.len(), 7 + 2 * 36);
        assert_eq!(decode_snapshot(&buf), Some((42, vec![(1, a), (65535, b)])));
        assert_eq!(decode_snapshot(&encode_snapshot(3, [])), Some((3, vec![])));
        assert!(decode_snapshot(&buf[..buf.len() - 1]).is_none());
    }

    #[test]
    fn control_messages_use_the_wire_field_names() {
        let hello = Msg::Hello {
            proto: 1,
            name: "Alice".into(),
            build: Some("x".into()),
            token: None,
        };
        assert_eq!(
            hello.encode(),
            r#"{"t":"hello","proto":1,"name":"Alice","build":"x"}"#
        );
        let welcome = Msg::Welcome {
            id: 2,
            name: "A".into(),
            server_name: "S".into(),
            tick_rate: 20,
            max_players: 32,
            players: vec![],
            resources: vec![],
        };
        let json: Value = serde_json::from_str(&welcome.encode()).unwrap();
        assert_eq!(json["t"], "welcome");
        assert_eq!(json["serverName"], "S");
        assert_eq!(json["tickRate"], 20);
        assert_eq!(json["maxPlayers"], 32);
    }

    #[test]
    fn decode_accepts_what_0_1_clients_send() {
        assert_eq!(
            Msg::decode(r#"{"t":"event","name":"chat:send","args":["hi",1,null]}"#),
            Some(Msg::Event {
                name: "chat:send".into(),
                args: vec!["hi".into(), 1.into(), Value::Null],
                src: None
            })
        );
        assert_eq!(
            Msg::decode(r#"{"t":"event","name":"x"}"#),
            Some(Msg::Event {
                name: "x".into(),
                args: vec![],
                src: None
            })
        );
        assert_eq!(
            Msg::decode(r#"{"t":"ping","ts":1795597672181}"#),
            Some(Msg::Ping {
                ts: 1795597672181.0
            })
        );
        assert_eq!(
            Msg::decode(r#"{"t":"leave","id":3}"#),
            Some(Msg::Leave {
                id: 3,
                reason: String::new()
            })
        );
        assert!(Msg::decode("not json").is_none());
        assert!(Msg::decode(r#"{"t":"nope"}"#).is_none());
        assert!(Msg::decode(r#"{"name":"x"}"#).is_none());
        assert!(Msg::decode(&format!(
            r#"{{"t":"ping","ts":1,"pad":"{}"}}"#,
            "x".repeat(MAX_JSON_FRAME)
        ))
        .is_none());
    }

    #[test]
    fn names_are_cleaned_and_bounded() {
        assert_eq!(sanitize_name("  Alice\u{0}\n "), "Alice");
        assert_eq!(sanitize_name("\t\u{7f}"), "");
        assert_eq!(
            sanitize_name(&"x".repeat(100)).chars().count(),
            MAX_NAME_LENGTH
        );
    }
}
