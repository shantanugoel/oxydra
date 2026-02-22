//! WASM guest module for oxydra file tool operations.
//!
//! This binary is compiled to `wasm32-wasip1` and executed inside a wasmtime
//! sandbox with WASI preopened directories as the security boundary. File
//! access is physically constrained to the preopened mounts — the guest cannot
//! escape those boundaries regardless of the paths it constructs.
//!
//! ## Protocol
//!
//! stdin:  `{"op": "<tool_name>", "args": {<tool_arguments>}}`
//! stdout: `{"ok": "<result_string>"}` on success
//!         `{"err": "<error_message>"}` on failure
//! exit 0 on success, exit 1 on operation error

use std::{
    fs,
    io::{self, Read},
    path::Path,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_SEARCH_MATCHES: usize = 200;

#[derive(Deserialize)]
struct GuestInvocation {
    op: String,
    args: Value,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GuestResult {
    Ok { ok: String },
    Err { err: String },
}

impl GuestResult {
    fn ok(s: impl Into<String>) -> Self {
        Self::Ok { ok: s.into() }
    }

    fn err(s: impl Into<String>) -> Self {
        Self::Err { err: s.into() }
    }

    fn is_err(&self) -> bool {
        matches!(self, Self::Err { .. })
    }
}

fn main() {
    let mut input = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut input) {
        let result = GuestResult::err(format!("failed to read stdin: {e}"));
        println!("{}", serde_json::to_string(&result).unwrap_or_default());
        std::process::exit(1);
    }

    let invocation: GuestInvocation = match serde_json::from_str(&input) {
        Ok(inv) => inv,
        Err(e) => {
            let result = GuestResult::err(format!("failed to parse invocation: {e}"));
            println!("{}", serde_json::to_string(&result).unwrap_or_default());
            std::process::exit(1);
        }
    };

    let result = dispatch(&invocation.op, &invocation.args);
    let is_err = result.is_err();
    let output = serde_json::to_string(&result)
        .unwrap_or_else(|e| format!("{{\"err\":\"failed to serialize result: {e}\"}}"));
    println!("{output}");

    if is_err {
        std::process::exit(1);
    }
}

fn dispatch(op: &str, args: &Value) -> GuestResult {
    match op {
        "file_read" => file_read(args),
        "file_write" => file_write(args),
        "file_edit" => file_edit(args),
        "file_delete" => file_delete(args),
        "file_list" => file_list(args),
        "file_search" => file_search(args),
        "vault_copyto_read" => vault_copyto_read(args),
        "vault_copyto_write" => vault_copyto_write(args),
        _ => GuestResult::err(format!("unknown operation `{op}`")),
    }
}

fn required_string<'a>(args: &'a Value, field: &str, op: &str) -> Result<&'a str, GuestResult> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        GuestResult::err(format!(
            "operation `{op}` requires string argument `{field}`"
        ))
    })
}

fn optional_string<'a>(args: &'a Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(Value::as_str)
}

fn file_read(args: &Value) -> GuestResult {
    let path = match required_string(args, "path", "file_read") {
        Ok(p) => p,
        Err(e) => return e,
    };
    match fs::read_to_string(path) {
        Ok(content) => GuestResult::ok(content),
        Err(e) => GuestResult::err(format!("failed to read `{path}`: {e}")),
    }
}

fn file_write(args: &Value) -> GuestResult {
    let path = match required_string(args, "path", "file_write") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match required_string(args, "content", "file_write") {
        Ok(c) => c,
        Err(e) => return e,
    };
    // Create parent directories if needed
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return GuestResult::err(format!("failed to create directories for `{path}`: {e}"));
    }
    match fs::write(path, content.as_bytes()) {
        Ok(()) => GuestResult::ok(format!("wrote {} bytes to {path}", content.len())),
        Err(e) => GuestResult::err(format!("failed to write `{path}`: {e}")),
    }
}

fn file_edit(args: &Value) -> GuestResult {
    let path = match required_string(args, "path", "file_edit") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old_text = match required_string(args, "old_text", "file_edit") {
        Ok(t) => t,
        Err(e) => return e,
    };
    let new_text = match required_string(args, "new_text", "file_edit") {
        Ok(t) => t,
        Err(e) => return e,
    };

    if old_text.is_empty() {
        return GuestResult::err("old_text must not be empty");
    }

    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return GuestResult::err(format!("failed to read `{path}`: {e}")),
    };

    let occurrences = content.matches(old_text).count();
    if occurrences == 0 {
        return GuestResult::err("old_text was not found in target file");
    }
    if occurrences > 1 {
        return GuestResult::err(format!(
            "old_text matched {occurrences} locations; provide a more specific snippet"
        ));
    }

    let updated = content.replacen(old_text, new_text, 1);
    match fs::write(path, updated.as_bytes()) {
        Ok(()) => GuestResult::ok(format!("updated {path}")),
        Err(e) => GuestResult::err(format!("failed to write `{path}`: {e}")),
    }
}

fn file_delete(args: &Value) -> GuestResult {
    let path = match required_string(args, "path", "file_delete") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return GuestResult::err(format!("failed to stat `{path}`: {e}")),
    };
    if metadata.is_dir() {
        match fs::remove_dir_all(path) {
            Ok(()) => GuestResult::ok(format!("deleted directory {path}")),
            Err(e) => GuestResult::err(format!("failed to remove `{path}`: {e}")),
        }
    } else {
        match fs::remove_file(path) {
            Ok(()) => GuestResult::ok(format!("deleted file {path}")),
            Err(e) => GuestResult::err(format!("failed to remove `{path}`: {e}")),
        }
    }
}

fn file_list(args: &Value) -> GuestResult {
    let path = optional_string(args, "path").unwrap_or(".");
    let dir = match fs::read_dir(path) {
        Ok(d) => d,
        Err(e) => return GuestResult::err(format!("failed to list `{path}`: {e}")),
    };

    let mut entries = Vec::new();
    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                return GuestResult::err(format!("failed to read entry in `{path}`: {e}"));
            }
        };
        let mut label = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_dir() {
            label.push('/');
        }
        entries.push(label);
    }
    entries.sort();

    if entries.is_empty() {
        GuestResult::ok("no entries found")
    } else {
        GuestResult::ok(entries.join("\n"))
    }
}

fn file_search(args: &Value) -> GuestResult {
    let path = match required_string(args, "path", "file_search") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let query = match required_string(args, "query", "file_search") {
        Ok(q) => q,
        Err(e) => return e,
    };

    if query.is_empty() {
        return GuestResult::err("query must not be empty");
    }

    let mut matches = Vec::new();
    if let Err(e) = collect_search_matches(Path::new(path), query, &mut matches) {
        return GuestResult::err(e);
    }

    if matches.is_empty() {
        GuestResult::ok("no matches found")
    } else {
        GuestResult::ok(matches.join("\n"))
    }
}

fn collect_search_matches(
    path: &Path,
    query: &str,
    matches: &mut Vec<String>,
) -> Result<(), String> {
    if matches.len() >= MAX_SEARCH_MATCHES {
        return Ok(());
    }

    let metadata =
        fs::metadata(path).map_err(|e| format!("failed to inspect `{}`: {e}", path.display()))?;

    if metadata.is_dir() {
        let entries =
            fs::read_dir(path).map_err(|e| format!("failed to list `{}`: {e}", path.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("failed to read entry in `{}`: {e}", path.display()))?;
            collect_search_matches(&entry.path(), query, matches)?;
            if matches.len() >= MAX_SEARCH_MATCHES {
                break;
            }
        }
        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    // Skip binary files silently
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    for (line_index, line) in content.lines().enumerate() {
        if line.contains(query) {
            matches.push(format!("{}:{}:{line}", path.display(), line_index + 1));
            if matches.len() >= MAX_SEARCH_MATCHES {
                break;
            }
        }
    }

    Ok(())
}

fn vault_copyto_read(args: &Value) -> GuestResult {
    let source_path = match required_string(args, "source_path", "vault_copyto_read") {
        Ok(p) => p,
        Err(e) => return e,
    };
    match fs::read_to_string(source_path) {
        Ok(content) => GuestResult::ok(content),
        Err(e) => GuestResult::err(format!("failed to read vault source `{source_path}`: {e}")),
    }
}

fn vault_copyto_write(args: &Value) -> GuestResult {
    let destination_path = match required_string(args, "destination_path", "vault_copyto_write") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match required_string(args, "content", "vault_copyto_write") {
        Ok(c) => c,
        Err(e) => return e,
    };
    match fs::write(destination_path, content.as_bytes()) {
        Ok(()) => GuestResult::ok(format!(
            "copied {} bytes to {destination_path}",
            content.len()
        )),
        Err(e) => GuestResult::err(format!(
            "failed to write destination `{destination_path}`: {e}"
        )),
    }
}
