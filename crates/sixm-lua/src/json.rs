//! The JSON bridge: how Lua values cross between runtimes and to the network.
//!
//! Lua to JSON
//! - `nil` becomes `null`; booleans, strings and numbers map directly. Integers
//!   stay integers (the full 64-bit range), floats stay floats, and NaN and
//!   the infinities become `null`.
//! - A table whose keys are exactly `1..n` (n > 0) becomes an array. Any other
//!   table becomes an object, with number and boolean keys stringified. The
//!   empty table becomes `{}`: it cannot be told apart from an empty array.
//! - Functions, userdata and threads become `null` (`json.encode` refuses them
//!   instead).
//! - Cycles and nesting deeper than [`MAX_DEPTH`] levels are errors.
//! - Strings that are not valid UTF-8 are converted lossily (U+FFFD).
//!
//! JSON to Lua
//! - Integers that fit in an `i64` become Lua integers, all other numbers
//!   floats. Arrays become sequences, objects tables, `null` becomes `nil`.
//! - Object keys that are canonical integers (`"7"`, `"-3"`) become integer
//!   keys, so sparse and id-keyed tables survive a Lua → JSON → Lua round
//!   trip. `json.decode` keeps every key a string.

use std::ffi::c_void;

use mlua::{Lua, Value};
use serde_json::{Map, Number};

use crate::Json;

/// Deepest table / array nesting the bridge accepts.
pub const MAX_DEPTH: usize = 64;

/// Converts a Lua value to JSON using the runtime's rules (see the module docs
/// of the crate): sequence → array, other table → object, functions, userdata
/// and threads → `null`, cycles and nesting deeper than 64 levels → error.
pub fn lua_to_json(value: &Value) -> Result<Json, String> {
    to_json(value, false, 0, &mut Vec::new())
}

/// Converts JSON to a Lua value using the runtime's rules: integers stay
/// integers, arrays become sequences, objects become tables (canonical integer
/// keys become integer keys), `null` becomes `nil`.
pub fn json_to_lua(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    to_lua(lua, value, true)
}

/// `json.encode`: like [`lua_to_json`], but values with no JSON form are errors.
pub(crate) fn encode(value: &Value) -> Result<String, String> {
    let json = to_json(value, true, 0, &mut Vec::new())?;
    serde_json::to_string(&json).map_err(|err| err.to_string())
}

/// `json.decode`: like [`json_to_lua`], but object keys always stay strings.
pub(crate) fn decode(lua: &Lua, text: &str) -> Result<Value, String> {
    let json: Json = serde_json::from_str(text).map_err(|err| err.to_string())?;
    to_lua(lua, &json, false).map_err(|err| err.to_string())
}

fn to_json(
    value: &Value,
    strict: bool,
    depth: usize,
    path: &mut Vec<*const c_void>,
) -> Result<Json, String> {
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(flag) => Ok(Json::Bool(*flag)),
        Value::Integer(n) => Ok(Json::Number(Number::from(*n))),
        Value::Number(n) => Ok(Number::from_f64(*n).map_or(Json::Null, Json::Number)),
        Value::String(text) => Ok(Json::String(text.to_string_lossy())),
        Value::Table(table) => table_to_json(table, strict, depth, path),
        other if strict => Err(format!("cannot encode a {}", other.type_name())),
        _ => Ok(Json::Null),
    }
}

enum Key {
    Index(i64),
    Name(String),
}

fn table_to_json(
    table: &mlua::Table,
    strict: bool,
    depth: usize,
    path: &mut Vec<*const c_void>,
) -> Result<Json, String> {
    if depth >= MAX_DEPTH {
        return Err(format!(
            "cannot convert table: nested deeper than {MAX_DEPTH} levels"
        ));
    }
    let identity = table.to_pointer();
    if path.contains(&identity) {
        return Err("cannot convert table: it contains a cycle".to_owned());
    }
    path.push(identity);

    let mut entries: Vec<(Key, Json)> = Vec::with_capacity(table.raw_len());
    let mut sequence = true;
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|err| err.to_string())?;
        let key = match key {
            Value::Integer(n) => {
                sequence &= n >= 1;
                Key::Index(n)
            }
            Value::Number(n) => {
                sequence = false;
                Key::Name(n.to_string())
            }
            Value::String(text) => {
                sequence = false;
                Key::Name(text.to_string_lossy())
            }
            Value::Boolean(flag) => {
                sequence = false;
                Key::Name(flag.to_string())
            }
            other => {
                return Err(format!(
                    "cannot convert table: unsupported {} key",
                    other.type_name()
                ))
            }
        };
        entries.push((key, to_json(&value, strict, depth + 1, path)?));
    }
    path.pop();

    // Distinct integer keys >= 1 with none above `count` are exactly 1..count.
    let count = entries.len();
    let in_range =
        |key: &Key| matches!(key, Key::Index(n) if usize::try_from(*n).is_ok_and(|n| n <= count));
    if sequence && count > 0 && entries.iter().all(|(key, _)| in_range(key)) {
        let mut items = vec![Json::Null; count];
        for (key, value) in entries {
            if let Key::Index(n) = key {
                items[n as usize - 1] = value;
            }
        }
        return Ok(Json::Array(items));
    }
    // Sorted, so the output does not depend on Lua's table iteration order
    // (nor on whether serde_json's `preserve_order` feature is enabled).
    let mut named: Vec<(String, Json)> = entries
        .into_iter()
        .map(|(key, value)| match key {
            Key::Index(n) => (n.to_string(), value),
            Key::Name(name) => (name, value),
        })
        .collect();
    named.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Json::Object(Map::from_iter(named)))
}

fn to_lua(lua: &Lua, value: &Json, integer_keys: bool) -> mlua::Result<Value> {
    Ok(match value {
        Json::Null => Value::Nil,
        Json::Bool(flag) => Value::Boolean(*flag),
        Json::Number(n) => match n.as_i64() {
            Some(int) => Value::Integer(int),
            None => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(text) => Value::String(lua.create_string(text)?),
        Json::Array(items) => {
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (index, item) in items.iter().enumerate() {
                table.raw_seti(index + 1, to_lua(lua, item, integer_keys)?)?;
            }
            Value::Table(table)
        }
        Json::Object(object) => {
            let table = lua.create_table_with_capacity(0, object.len())?;
            for (key, item) in object {
                let item = to_lua(lua, item, integer_keys)?;
                match canonical_integer(key).filter(|_| integer_keys) {
                    Some(int) => table.raw_set(int, item)?,
                    None => table.raw_set(key.as_str(), item)?,
                }
            }
            Value::Table(table)
        }
    })
}

/// `"7"` → 7, `"-3"` → -3; `"07"`, `"+7"`, `"-0"` and `"7.0"` are not canonical.
fn canonical_integer(key: &str) -> Option<i64> {
    let int: i64 = key.parse().ok()?;
    (int.to_string() == key).then_some(int)
}
