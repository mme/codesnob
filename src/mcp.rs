//! A minimal Model Context Protocol server over stdio: JSON-RPC 2.0, one
//! message per line. It exposes one tool that shows a repository in the
//! viewer nearest to the agent's own workspace.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

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
        "instructions": INSTRUCTIONS
    })
}

/// Always in the agent's context, so this is what makes it reach for codesnob.
const INSTRUCTIONS: &str = "codesnob is the user's code viewer, open in a terminal beside you. When the user wants to see changes, show them there instead of pasting diffs into the chat: `show_diff` shows what changed between any two states (the branch point, commits, branches, staged work, a pull request). Typical requests: \"what changed\", \"show me the diff\", \"compare main and this branch\", \"show me PR 123\". A call updates that repository's view in the viewer; the user gets back to where they were with Esc.";

fn tools() -> Value {
    json!([{
        "name": "show_diff",
        "description": "Show the user a diff in their codesnob viewer. range takes git syntax (\"main...feature\", \"HEAD~3..HEAD\", a single commit, \"stash@{0}\"), \"worktree\", \"staged\" or \"unstaged\", or a pull request (\"#123\", \"owner/repo#123\", a URL). Without a range it shows everything since the branch point, uncommitted work included. Use it when the user asks what changed, to see or review a diff, to compare branches or commits, or to look at a pull request. Optionally narrow to files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "range": { "type": "string", "description": "What to compare. Git syntax: \"a...b\" is b since it diverged from a, \"a..b\" is a against b, a branch or tag alone is everything since the branch point with it, any other revision is that commit against its parent. Also \"worktree\", \"staged\", \"unstaged\", or a pull request. Omit for everything since the branch point." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Show only these files. Paths are relative to your working directory." },
                "repo": { "type": "string", "description": "A directory in the repository. Defaults to your working directory." }
            }
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
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let (root, view, what) = prepare_diff(args, &cwd)?;
    let mut client = link::connect()?;
    let r = link::request_show(&mut client, root, Some(view)).map_err(no_viewer)?;
    Ok(format!(
        "{}: {what}; the user gets back to their previous view with Esc",
        link::describe(&r)
    ))
}

fn no_viewer(e: String) -> String {
    match e.as_str() {
        "no codesnob viewer is connected" => {
            "no codesnob viewer is running; ask the user to open one with `codesnob`".to_string()
        }
        _ => e,
    }
}

/// Check a show_diff call before anything reaches the viewer: the
/// repository, the range and the files. Returns the checkout root, what to
/// send, and words for what will be shown.
fn prepare_diff(
    args: &Value,
    cwd: &Path,
) -> Result<(PathBuf, crate::proto::ViewSpec, String), String> {
    let root = repo_root(args, cwd)?;
    let range = args
        .get("range")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    if let Some(r) = &range
        && crate::pr::parse(r).is_none()
    {
        crate::git::parse_spec(&root, r).map_err(|e| e.to_string())?;
    }
    let files = match args.get("files") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                let p = v.as_str().ok_or("files must be strings")?;
                relative_to(&root, cwd, p)
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("files must be a list of paths".to_string()),
    };
    let mut what = range
        .clone()
        .unwrap_or_else(|| "everything since the branch point".to_string());
    if !files.is_empty() {
        what.push_str(&format!(", narrowed to {}", files.join(", ")));
    }
    Ok((root, crate::proto::ViewSpec { range, files }, what))
}

/// The checkout root of `repo`, or of the working directory.
fn repo_root(args: &Value, cwd: &Path) -> Result<PathBuf, String> {
    let dir = match args.get("repo").and_then(Value::as_str) {
        Some(r) => cwd.join(r),
        None => cwd.to_path_buf(),
    };
    let repo = gix::discover(&dir)
        .map_err(|_| format!("{} is not inside a git repository", dir.display()))?;
    let root = repo
        .workdir()
        .ok_or("bare repositories have no working tree")?;
    std::fs::canonicalize(root).map_err(|e| e.to_string())
}

/// `path` as given by the agent (relative to its working directory, or
/// absolute) made relative to the checkout root, the way the viewer names
/// files. The file may no longer exist, as in a diff that deletes it.
fn relative_to(root: &Path, cwd: &Path, path: &str) -> Result<String, String> {
    let joined = cwd.join(path);
    let full = std::fs::canonicalize(&joined).unwrap_or_else(|_| lexical(&joined));
    let rel = full
        .strip_prefix(root)
        .map_err(|_| format!("{path} is outside the repository at {}", root.display()))?;
    Ok(rel.to_string_lossy().into_owned())
}

/// Resolve `.` and `..` without touching the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::branch_with_history;

    #[test]
    fn a_diff_call_is_checked_before_it_is_sent() {
        let dir = branch_with_history("mcp-prepare");
        let sub = dir.join("src");
        let (root, view, what) = prepare_diff(
            &json!({ "range": "main...feature", "files": ["deep/one.rs"] }),
            &sub,
        )
        .unwrap();
        assert_eq!(root, dir);
        assert_eq!(view.range.as_deref(), Some("main...feature"));
        assert_eq!(
            view.files,
            ["src/deep/one.rs"],
            "paths become repository-relative"
        );
        assert_eq!(what, "main...feature, narrowed to src/deep/one.rs");

        let (_, view, what) = prepare_diff(&json!({}), &dir).unwrap();
        assert_eq!(view, crate::proto::ViewSpec::default());
        assert_eq!(what, "everything since the branch point");

        let err = prepare_diff(&json!({ "range": "nope...feature" }), &dir).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        let err = prepare_diff(&json!({ "files": ["../../elsewhere.rs"] }), &dir).unwrap_err();
        assert!(err.contains("outside the repository"), "{err}");
        let (_, view, _) = prepare_diff(&json!({ "range": "#12" }), &dir).unwrap();
        assert_eq!(
            view.range.as_deref(),
            Some("#12"),
            "PRs are left to the viewer"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
