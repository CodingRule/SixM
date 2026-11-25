//! Resource manifests. A manifest is a small Lua file of directives:
//!
//! ```lua
//! name 'hello'
//! server_script 'server.lua'
//! client_scripts { 'a.lua', 'b.lua' }
//! ```
//!
//! It is evaluated in a throwaway Lua state that has no standard library, only
//! these directives, so a manifest cannot do anything but describe its resource.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use sixm_lua::mlua::{self, Lua, LuaOptions, StdLib, Value};

pub const MANIFEST_FILE: &str = "manifest.lua";

/// A manifest that loops forever is stopped after this many VM instructions.
const INSTRUCTION_LIMIT: u32 = 1_000_000;

const INFO_KEYS: [&str; 6] = [
    "name",
    "description",
    "author",
    "version",
    "url",
    "sixm_version",
];

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Manifest {
    /// `name`, `description`, `author`, `version`, `url`, `sixm_version`.
    pub info: BTreeMap<String, String>,
    pub server_scripts: Vec<String>,
    pub client_scripts: Vec<String>,
    pub shared_scripts: Vec<String>,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Copy)]
enum List {
    Server,
    Client,
    Shared,
    Dependencies,
}

/// A script path must stay inside its resource: relative, and without `..`.
fn is_inside_resource(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let rooted = path.starts_with('/') || path.starts_with('\\');
    let climbs = path.split(['/', '\\']).any(|part| part == "..");
    !path.is_empty() && !drive && !rooted && !climbs
}

fn to_list(value: Value, directive: &str, check_paths: bool) -> Result<Vec<String>, String> {
    let wrong = || format!("{directive} expects a file name or a list of file names");
    let items: Vec<Value> = match value {
        Value::Table(t) => t
            .sequence_values::<Value>()
            .collect::<mlua::Result<_>>()
            .map_err(|_| wrong())?,
        other => vec![other],
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(s) = item else {
            return Err(wrong());
        };
        let s = s.to_str().map_err(|_| wrong())?.to_string();
        if s.is_empty() {
            return Err(wrong());
        }
        if check_paths && !is_inside_resource(&s) {
            return Err(format!(
                "{directive}: \"{s}\" must be a path inside the resource"
            ));
        }
        out.push(s);
    }
    Ok(out)
}

/// Parse the text of a `manifest.lua`.
pub fn parse_manifest(source: &str, chunk_name: &str) -> Result<Manifest, String> {
    let fail = |e: mlua::Error| format!("invalid manifest: {}", first_line(&e.to_string()));
    let lua = Lua::new_with(StdLib::NONE, LuaOptions::default()).map_err(fail)?;
    let manifest = Rc::new(RefCell::new(Manifest::default()));
    let globals = lua.globals();

    for key in INFO_KEYS {
        let m = manifest.clone();
        let f = lua
            .create_function(move |_, value: Value| {
                let text = match value {
                    Value::String(s) => s.to_string_lossy().to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Number(n) => n.to_string(),
                    _ => return Err(mlua::Error::runtime(format!("{key} expects a string"))),
                };
                m.borrow_mut().info.insert(key.to_string(), text);
                Ok(())
            })
            .map_err(fail)?;
        globals.set(key, f).map_err(fail)?;
    }

    let lists = [
        ("server_script", List::Server),
        ("server_scripts", List::Server),
        ("client_script", List::Client),
        ("client_scripts", List::Client),
        ("shared_script", List::Shared),
        ("shared_scripts", List::Shared),
        ("dependency", List::Dependencies),
        ("dependencies", List::Dependencies),
    ];
    for (directive, list) in lists {
        let m = manifest.clone();
        let f = lua
            .create_function(move |_, value: Value| {
                let check_paths = !matches!(list, List::Dependencies);
                let items = to_list(value, directive, check_paths).map_err(mlua::Error::runtime)?;
                let mut m = m.borrow_mut();
                match list {
                    List::Server => m.server_scripts.extend(items),
                    List::Client => m.client_scripts.extend(items),
                    List::Shared => m.shared_scripts.extend(items),
                    List::Dependencies => m.dependencies.extend(items),
                }
                Ok(())
            })
            .map_err(fail)?;
        globals.set(directive, f).map_err(fail)?;
    }

    let budget = mlua::HookTriggers::new().every_nth_instruction(INSTRUCTION_LIMIT);
    lua.set_hook(budget, |_, _| {
        Err(mlua::Error::runtime("the manifest runs for too long"))
    })
    .map_err(fail)?;

    lua.load(source).set_name(chunk_name).exec().map_err(fail)?;
    drop(globals);
    drop(lua);
    Ok(Rc::try_unwrap(manifest)
        .map(RefCell::into_inner)
        .unwrap_or_else(|rc| rc.borrow().clone()))
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_directives_in_both_forms() {
        let m = parse_manifest(
            "name 'hello'\nversion '1.0.0'\ndescription 'x'\nshared_script 'cfg.lua'\n\
             server_scripts { 'a.lua', 'sub/b.lua' }\nclient_script 'c.lua'\ndependency 'chat'",
            "@hello/manifest.lua",
        )
        .unwrap();
        assert_eq!(m.info["name"], "hello");
        assert_eq!(m.info["version"], "1.0.0");
        assert_eq!(m.shared_scripts, ["cfg.lua"]);
        assert_eq!(m.server_scripts, ["a.lua", "sub/b.lua"]);
        assert_eq!(m.client_scripts, ["c.lua"]);
        assert_eq!(m.dependencies, ["chat"]);
    }

    #[test]
    fn refuses_paths_outside_the_resource() {
        for bad in [
            "../x.lua",
            "a/../../x.lua",
            "/etc/passwd",
            "C:\\x.lua",
            "\\\\host\\x.lua",
        ] {
            let err = parse_manifest(&format!("server_script [[{bad}]]"), "@m").unwrap_err();
            assert!(err.contains("inside the resource"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_manifest_can_only_describe() {
        assert!(parse_manifest("os.execute('calc')", "@m").is_err());
        assert!(parse_manifest("io.open('x', 'w')", "@m").is_err());
        assert!(parse_manifest("require('x')", "@m").is_err());
        assert!(parse_manifest("server_script(42)", "@m")
            .unwrap_err()
            .contains("expects a file name"));
        assert!(parse_manifest("while true do end", "@m")
            .unwrap_err()
            .contains("too long"));
        assert!(parse_manifest("name 'x' (", "@m")
            .unwrap_err()
            .starts_with("invalid manifest"));
    }
}
