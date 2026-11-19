//! Checks the live Hardcover API for changes that matter to this app.
//!
//! 1. Introspects the GraphQL schema and renders it in the format of
//!    `schema/hardcover.graphql`, then lists the types and fields that were
//!    added, removed or changed since the committed copy.
//! 2. Validates every document the client sends against the live schema.
//! 3. Fetches the public OAuth scope map (`/capabilities.json`) and checks that
//!    the scopes the app requests still grant every top-level field it uses,
//!    and lists scope changes since `schema/hardcover-capabilities.json`.
//!
//! Then it writes both files, so `git diff schema/` shows the full change.
//! Exits non-zero when a document no longer validates or a scope no longer
//! covers it, which means the released app is broken against the live API.
//!
//! Uses `HARDCOVER_TOKEN` if set, otherwise the app's keyring session (sign
//! in with the app or the `oauth_login` example first). Two API requests.
//!
//! ```sh
//! cargo run -p hardcover-core --example api_check             # check and update schema/
//! cargo run -p hardcover-core --example api_check -- --check  # check only; exit 1 on any change
//! ```

#[path = "../src/api/sdl.rs"]
mod sdl;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use hardcover_core::auth::{DEFAULT_SCOPES, KeyringTokenStore, OAuthConfig, Session, StaticToken};
use hardcover_core::{TokenSource, USER_AGENT};
use serde_json::Value;

const ENDPOINT: &str = "https://api.hardcover.app/v1/graphql";
const CAPABILITIES: &str = "https://api.hardcover.app/capabilities.json";
const HEADER: &str = "# Generated from Hardcover GraphQL introspection (https://api.hardcover.app/v1/graphql). Do not edit by hand.\n";

const INTROSPECTION: &str = "query IntrospectionQuery { __schema { types { kind name \
    fields(includeDeprecated: true) { name args { name type { ...T } } type { ...T } isDeprecated deprecationReason } \
    inputFields { name type { ...T } } enumValues(includeDeprecated: true) { name } } } } \
    fragment T on __Type { kind name ofType { kind name ofType { kind name ofType { kind name \
    ofType { kind name ofType { kind name ofType { kind name } } } } } } }";

type Error = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool, Error> {
    let check_only = std::env::args().skip(1).any(|a| a == "--check");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema_path = root.join("schema/hardcover.graphql");
    let caps_path = root.join("schema/hardcover-capabilities.json");
    let mut ok = true;

    // ----- schema -----
    let (tokens, source) = token_source()?;
    println!("Introspecting {ENDPOINT} with {source}");
    let introspection = graphql(&*tokens, INTROSPECTION)?;
    let live_sdl = render_sdl(&introspection)?;
    let old_sdl = std::fs::read_to_string(&schema_path).unwrap_or_default();
    let schema_changed = live_sdl != old_sdl;
    if schema_changed {
        report_schema_diff(&old_sdl, &live_sdl);
    } else {
        println!("  schema unchanged");
    }

    let live = sdl::parse(&live_sdl).map_err(|e| format!("live schema does not parse:\n{e}"))?;
    let mut bad_documents = 0;
    for (name, doc) in hardcover_core::api::DOCUMENTS {
        if let Err(e) = sdl::validate(&live, name, doc) {
            println!("  BROKEN  {name}:\n{}", indent(&e, 10));
            bad_documents += 1;
        }
    }
    if bad_documents == 0 {
        println!(
            "  all {} documents validate against the live schema",
            hardcover_core::api::DOCUMENTS.len()
        );
    } else {
        ok = false;
    }

    // ----- OAuth scopes -----
    println!("\nFetching {CAPABILITIES}");
    let caps = get_json(CAPABILITIES)?;
    let granted = granted_operations(&caps, &DEFAULT_SCOPES)?;
    let mut uncovered = Vec::new();
    for (name, doc) in hardcover_core::api::DOCUMENTS {
        for field in sdl::top_level_fields(&live, name, doc) {
            if !granted.contains(field.as_str()) {
                uncovered.push(format!("{name} uses `{field}`"));
            }
        }
    }
    if uncovered.is_empty() {
        println!(
            "  the app's scopes ({}) cover every top-level field it uses",
            DEFAULT_SCOPES.join(" ")
        );
    } else {
        ok = false;
        for u in &uncovered {
            println!("  NOT GRANTED  {u}");
        }
    }
    let caps_json = serde_json::to_string_pretty(&caps)? + "\n";
    let old_caps = std::fs::read_to_string(&caps_path).unwrap_or_default();
    let caps_changed = caps_json != old_caps;
    if caps_changed {
        report_scope_diff(&old_caps, &caps);
    } else {
        println!("  scope map unchanged");
    }

    // ----- result -----
    println!();
    if schema_changed || caps_changed {
        if check_only {
            println!("The live API differs from schema/. Run without --check to update it.");
            ok = false;
        } else {
            write(&schema_path, &live_sdl, schema_changed)?;
            write(&caps_path, &caps_json, caps_changed)?;
            println!("Updated schema/. Review with `git diff schema/` and run `cargo test`.");
        }
    } else {
        println!("schema/ matches the live API.");
    }
    if !ok && (bad_documents > 0 || !uncovered.is_empty()) {
        println!("The app is BROKEN against the live API: see BROKEN / NOT GRANTED above.");
    }
    Ok(ok)
}

fn token_source() -> Result<(Arc<dyn TokenSource>, &'static str), Error> {
    if let Some(token) = StaticToken::from_env() {
        return Ok((Arc::new(token), "HARDCOVER_TOKEN"));
    }
    let session = Session::restore(OAuthConfig::default(), Arc::new(KeyringTokenStore::new()?))?
        .ok_or(
            "no token: set HARDCOVER_TOKEN or sign in with the app (or the oauth_login example)",
        )?;
    Ok((Arc::new(session), "the keyring session"))
}

fn graphql(tokens: &dyn TokenSource, query: &str) -> Result<Value, Error> {
    let token = tokens.access_token()?;
    let mut response = ureq::post(ENDPOINT)
        .header("user-agent", USER_AGENT)
        .header("authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "query": query }))?;
    let body: Value = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_json()?;
    if let Some(errors) = body.get("errors") {
        return Err(format!("GraphQL errors: {errors}").into());
    }
    Ok(body)
}

fn get_json(url: &str) -> Result<Value, Error> {
    Ok(ureq::get(url)
        .header("user-agent", USER_AGENT)
        .call()?
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_json()?)
}

// ----- SDL rendering (the format of schema/hardcover.graphql) -----

fn render_sdl(introspection: &Value) -> Result<String, Error> {
    let mut types: Vec<&Value> = introspection["data"]["__schema"]["types"]
        .as_array()
        .ok_or("introspection has no types")?
        .iter()
        .collect();
    types.sort_by_key(|t| t["name"].as_str().unwrap_or_default().to_owned());

    let mut out = vec![HEADER.to_owned()];
    for t in types {
        let name = t["name"].as_str().ok_or("type without a name")?;
        if name.starts_with("__") {
            continue;
        }
        match t["kind"].as_str() {
            Some("SCALAR") => out.push(format!("scalar {name}\n")),
            Some("ENUM") => {
                let values: Vec<&str> = list(&t["enumValues"])
                    .filter_map(|v| v["name"].as_str())
                    .collect();
                out.push(format!("enum {name} {{ {} }}\n", values.join(" ")));
            }
            Some(kind @ ("OBJECT" | "INPUT_OBJECT")) => {
                let keyword = if kind == "OBJECT" { "type" } else { "input" };
                out.push(format!("{keyword} {name} {{"));
                let fields = if kind == "OBJECT" {
                    &t["fields"]
                } else {
                    &t["inputFields"]
                };
                for f in list(fields) {
                    out.push(render_field(f)?);
                }
                out.push("}\n".to_owned());
            }
            other => return Err(format!("unsupported kind {other:?} for type {name}").into()),
        }
    }
    Ok(out.join("\n"))
}

fn render_field(f: &Value) -> Result<String, Error> {
    let name = f["name"].as_str().ok_or("field without a name")?;
    let args: Vec<String> = list(&f["args"])
        .map(|a| {
            Ok(format!(
                "{}: {}",
                a["name"].as_str().unwrap_or("?"),
                type_ref(&a["type"])?
            ))
        })
        .collect::<Result<_, Error>>()?;
    let args = if args.is_empty() {
        String::new()
    } else {
        format!("({})", args.join(", "))
    };
    let mut line = format!("  {name}{args}: {}", type_ref(&f["type"])?);
    if f["isDeprecated"].as_bool() == Some(true) {
        let reason = f["deprecationReason"]
            .as_str()
            .unwrap_or("No longer supported");
        line.push_str(&format!(
            " @deprecated(reason: {})",
            serde_json::to_string(reason)?
        ));
    }
    Ok(line)
}

fn type_ref(t: &Value) -> Result<String, Error> {
    match t["kind"].as_str() {
        Some("NON_NULL") => Ok(format!("{}!", type_ref(&t["ofType"])?)),
        Some("LIST") => Ok(format!("[{}]", type_ref(&t["ofType"])?)),
        _ => Ok(t["name"]
            .as_str()
            .ok_or("type reference without a name")?
            .to_owned()),
    }
}

fn list(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

// ----- diff reports -----

/// Type name -> (declaration line, member lines), parsed from rendered SDL.
fn blocks(sdl: &str) -> BTreeMap<String, (String, Vec<String>)> {
    let mut out = BTreeMap::new();
    let mut current: Option<(String, String, Vec<String>)> = None;
    for line in sdl.lines() {
        if let Some((name, decl, members)) = current.as_mut() {
            if line == "}" {
                out.insert(name.clone(), (decl.clone(), std::mem::take(members)));
                current = None;
            } else {
                members.push(line.trim().to_owned());
            }
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(keyword), Some(name)) = (words.next(), words.next()) else {
            continue;
        };
        match keyword {
            "type" | "input" => current = Some((name.to_owned(), keyword.to_owned(), Vec::new())),
            "enum" => {
                let values = line
                    .split_once('{')
                    .map(|(_, rest)| rest.trim_end_matches('}').split_whitespace())
                    .into_iter()
                    .flatten()
                    .map(str::to_owned)
                    .collect();
                out.insert(name.to_owned(), ("enum".to_owned(), values));
            }
            "scalar" => {
                out.insert(name.to_owned(), ("scalar".to_owned(), Vec::new()));
            }
            _ => {}
        }
    }
    out
}

fn member_name(line: &str) -> &str {
    line.split(['(', ':']).next().unwrap_or(line).trim()
}

fn report_schema_diff(old: &str, new: &str) {
    let (old, new) = (blocks(old), blocks(new));
    let mut breaking = 0;
    let mut lines = Vec::new();
    for (name, (kind, _)) in &new {
        if !old.contains_key(name) {
            lines.push(format!("  + {kind} {name}"));
        }
    }
    for (name, (kind, _)) in &old {
        if !new.contains_key(name) {
            lines.push(format!("  - {kind} {name}  (removed)"));
            breaking += 1;
        }
    }
    for (name, (kind, new_members)) in &new {
        let Some((_, old_members)) = old.get(name) else {
            continue;
        };
        let by_name = |members: &[String]| -> BTreeMap<String, String> {
            members
                .iter()
                .map(|m| (member_name(m).to_owned(), m.clone()))
                .collect()
        };
        let (o, n) = (by_name(old_members), by_name(new_members));
        for (member, line) in &n {
            match o.get(member) {
                None => lines.push(format!("  + {kind} {name}.{member}: {}", signature(line))),
                Some(before) if before != line => {
                    lines.push(format!(
                        "  ~ {kind} {name}.{member}: {} -> {}  (changed)",
                        signature(before),
                        signature(line)
                    ));
                    breaking += 1;
                }
                Some(_) => {}
            }
        }
        for member in o.keys().filter(|m| !n.contains_key(*m)) {
            lines.push(format!("  - {kind} {name}.{member}  (removed)"));
            breaking += 1;
        }
    }
    println!(
        "  schema changed: {} differences, {breaking} of them removals or changes",
        lines.len()
    );
    for line in lines {
        println!("{line}");
    }
}

fn signature(line: &str) -> &str {
    let name = member_name(line);
    line.strip_prefix(name)
        .map(|s| s.trim_start_matches(':').trim())
        .unwrap_or(line)
}

fn report_scope_diff(old: &str, new: &Value) {
    let old: Value = serde_json::from_str(old).unwrap_or(Value::Null);
    if old.is_null() {
        println!("  no previous scope map; saving the first snapshot");
        return;
    }
    let ops = |caps: &Value| -> BTreeMap<String, BTreeSet<String>> {
        list(&caps["scopes"])
            .filter_map(|s| {
                let scope = s["scope"].as_str()?.to_owned();
                let ops = list(&s["operations"])
                    .filter_map(|o| o.as_str().map(str::to_owned))
                    .collect();
                Some((scope, ops))
            })
            .collect()
    };
    let (old, new) = (ops(&old), ops(new));
    println!("  scope map changed:");
    let mut listed = false;
    for scope in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        let empty = BTreeSet::new();
        let (o, n) = (
            old.get(scope).unwrap_or(&empty),
            new.get(scope).unwrap_or(&empty),
        );
        if !old.contains_key(scope) {
            println!("  + scope {scope}");
            listed = true;
        } else if !new.contains_key(scope) {
            println!("  - scope {scope}  (removed)");
            listed = true;
        }
        for op in n.difference(o) {
            println!("  + {scope}: {op}");
            listed = true;
        }
        for op in o.difference(n) {
            println!("  - {scope}: {op}");
            listed = true;
        }
    }
    if !listed {
        println!("  no operations moved between scopes; see `git diff schema/` for the details");
    }
}

/// Top-level fields the given scopes grant, following `implies`.
fn granted_operations(caps: &Value, scopes: &[&str]) -> Result<BTreeSet<String>, Error> {
    let by_scope: BTreeMap<&str, &Value> = list(&caps["scopes"])
        .filter_map(|s| Some((s["scope"].as_str()?, s)))
        .collect();
    if by_scope.is_empty() {
        return Err("capabilities.json lists no scopes".into());
    }
    let mut pending: Vec<&str> = scopes.to_vec();
    let mut seen = BTreeSet::new();
    let mut granted = BTreeSet::new();
    while let Some(scope) = pending.pop() {
        if !seen.insert(scope) {
            continue;
        }
        let Some(s) = by_scope.get(scope) else {
            println!("  scope {scope} no longer exists");
            continue;
        };
        granted.extend(list(&s["operations"]).filter_map(|o| o.as_str().map(str::to_owned)));
        pending.extend(list(&s["implies"]).filter_map(|i| i.as_str()));
    }
    Ok(granted)
}

fn write(path: &PathBuf, contents: &str, changed: bool) -> Result<(), Error> {
    if changed {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

fn indent(text: &str, by: usize) -> String {
    let pad = " ".repeat(by);
    text.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
