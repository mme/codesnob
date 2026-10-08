//! A minimal Model Context Protocol server over stdio: JSON-RPC 2.0, one
//! message per line. It exposes one tool that shows a repository in the
//! viewer nearest to the agent's own workspace.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use crate::link;

const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                reply(
                    &mut out,
                    Value::Null,
                    Err((-32700, format!("parse error: {e}"))),
                )?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => Ok(call(&params)),
            m if m.starts_with("notifications/") => continue,
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        // Notifications carry no id and get no reply.
        let Some(id) = id else { continue };
        reply(&mut out, id, result)?;
    }
    Ok(())
}

fn reply(out: &mut impl Write, id: Value, result: Result<Value, (i64, String)>) -> Result<()> {
    let msg = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    };
    serde_json::to_writer(&mut *out, &msg)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn initialize(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = if SUPPORTED.contains(&requested) {
        requested
    } else {
        SUPPORTED[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "codesnob", "version": crate::proto::VERSION },
        "instructions": "Use show_diff to display a repository's changes in the user's codesnob viewer. The diff shows everything since the branch point, including uncommitted work, and refreshes automatically as files change."
    })
}

fn tools() -> Value {
    json!([{
        "name": "show_diff",
        "description": "Show the diff of a git repository (changes since its branch point, plus uncommitted work) in the user's codesnob terminal viewer. Picks the viewer on the agent's own workspace when there is one. Fails if no viewer is running.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory inside the repository or worktree to show." },
                "base": { "type": "string", "description": "Optional revision; the diff is taken against its merge-base with HEAD instead of the detected branch point." }
            },
            "required": ["path"]
        }
    }])
}

fn call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "show_diff" => tool_result(show_diff(&args)),
        _ => tool_result(Err(format!("unknown tool: {name}"))),
    }
}

fn tool_result(r: Result<String, String>) -> Value {
    match r {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
    }
}

fn show_diff(args: &Value) -> Result<String, String> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("missing required argument: path")?;
    let base = args.get("base").and_then(Value::as_str).map(str::to_owned);
    let dir = std::fs::canonicalize(path).map_err(|e| format!("{path}: {e}"))?;
    let mut client = link::connect()?;
    let r = link::request_show(&mut client, PathBuf::from(&dir), base).map_err(|e| {
        match e.as_str() {
            "no codesnob viewer is connected" => {
                "no codesnob viewer is running; ask the user to open one with `codesnob`"
                    .to_string()
            }
            _ => e,
        }
    })?;
    let mut text = link::describe(&r);
    if !r.repo {
        text.push_str(&format!(
            "; note: {} is not inside a git repository, the viewer shows a notice instead",
            dir.display()
        ));
    }
    Ok(text)
}
