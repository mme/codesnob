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
const INSTRUCTIONS: &str = "codesnob is the user's code viewer, open in a terminal beside you. When the user wants to see code, show it there instead of pasting it into the chat: `show_code` for files and lines, with highlights; `show_diff` for what changed between any two states (the branch point, commits, branches, staged work, a pull request). Typical requests: \"show me the code\", \"where is this handled\", \"open that file\", \"what changed\", \"show me the diff\", \"compare main and this branch\", \"show me PR 123\". A call updates that repository's view in the viewer; the user gets back to where they were with Esc. Read files with your own tools; codesnob is for showing. Add notes to highlights only when the user asks you to explain or comment on the code; otherwise show the code without them.";

fn tools() -> Value {
    json!([{
        "name": "show_code",
        "description": "Show source code to the user in their codesnob viewer: whole files, or excerpts around a line and highlighted line ranges. Use it when the user asks to see, open or look at code, or where something is defined or handled, and whenever they ask you to show code while explaining. Prefer a few precise highlights over large ranges. Add notes only when the user asks you to explain or comment on the code.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The files to show, in this order. Unless excerpts says otherwise, one file shows whole and several show as excerpts around their line and highlights.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Relative to your working directory, or absolute." },
                            "line": { "type": "integer", "minimum": 1, "description": "Open at this line, marked." },
                            "excerpts": { "type": "boolean", "description": "true shows only this file's line and highlights, each with a few lines around it, like a diff; false shows the whole file." },
                            "highlights": {
                                "type": "array",
                                "description": "Line ranges to highlight in this file. The viewer numbers all of a call's highlights by where they sit, not by the order you list them: by file in the order of files, then by line, so the first file's highlights come first. At most 20 per call, ① to ⑳. The user steps through them with < and >; the result lists each number with its file and lines, so refer to them by those numbers.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "lines": { "type": "string", "description": "1-based and inclusive: \"40-60\", or \"42\" for one line." },
                                        "note": { "type": "string", "description": "A short note shown above the lines. Only when the user asked you to explain or comment on the code." }
                                    },
                                    "required": ["lines"]
                                }
                            }
                        },
                        "required": ["path"]
                    }
                },
                "rev": { "type": "string", "description": "Show the files as of this revision instead of the working tree." },
                "repo": { "type": "string", "description": "A directory in the repository. Defaults to your working directory." }
            },
            "required": ["files"],
            // With highlights the view opens at ①, so no file takes a line.
            "if": { "properties": { "files": { "contains": { "required": ["highlights"] } } } },
            "then": { "properties": { "files": { "items": { "not": { "required": ["line"] } } } } }
        }
    }, {
        "name": "show_diff",
        "description": "Show the user a diff in their codesnob viewer. range takes git syntax (\"main...feature\", \"HEAD~3..HEAD\", a single commit, \"stash@{0}\"), \"worktree\", \"staged\" or \"unstaged\", or a pull request (\"#123\", \"owner/repo#123\", a URL). Without a range it shows everything since the branch point, uncommitted work included. Use it when the user asks what changed, to see or review a diff, to compare branches or commits, or to look at a pull request. Optionally narrow to files and highlight lines; add notes only when the user asks you to explain or comment on the code.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "range": { "type": "string", "description": "What to compare. Git syntax: \"a...b\" is b since it diverged from a, \"a..b\" is a against b, a branch or tag alone is everything since the branch point with it, any other revision is that commit against its parent. Also \"worktree\", \"staged\", \"unstaged\", or a pull request. Omit for everything since the branch point." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Show only these files. Paths are relative to your working directory." },
                "highlights": {
                    "type": "array",
                    "description": "Line ranges to highlight. Lines are the new side's, or the old side's in a deleted file; unchanged lines are shown too. The viewer numbers them by where they sit, not by the order you list them: by file in path order, as the diff lists its files, then by line. At most 20 per call, ① to ⑳. The user steps through them with < and >; the result lists each number with its file and lines, so refer to them by those numbers.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Relative to your working directory." },
                            "lines": { "type": "string", "description": "1-based and inclusive: \"40-60\", or \"42\"." },
                            "note": { "type": "string", "description": "A short note shown above the lines. Only when the user asked you to explain or comment on the code." }
                        },
                        "required": ["path", "lines"]
                    }
                },
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
        "show_code" => tool_result(show_code(&args)),
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

fn show_code(args: &Value) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let (root, view, what) = prepare_code(args, &cwd)?;
    let mut client = link::connect()?;
    let r = link::request_show(&mut client, root, Some(view)).map_err(no_viewer)?;
    Ok(format!(
        "{}: {what}; the user gets back to their previous view with Esc",
        link::describe(&r)
    ))
}

/// Check a show_code call: the repository, the revision, and that every file
/// exists there. Returns the checkout root, what to send, and words for it.
fn prepare_code(
    args: &Value,
    cwd: &Path,
) -> Result<(PathBuf, crate::proto::ViewSpec, String), String> {
    let root = repo_root(args, cwd)?;
    let rev = args
        .get("rev")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    let items = match args.get("files") {
        Some(Value::Array(items)) if !items.is_empty() => items,
        _ => return Err("files must list at least one file".to_string()),
    };
    let mut files = Vec::new();
    let mut marks: Vec<(usize, crate::proto::HighlightSpec)> = Vec::new();
    for item in items {
        if let Value::Object(_) = item {
            for h in list(item, "highlights")? {
                let (start, end) = parse_lines(h.get("lines"))?;
                marks.push((
                    files.len(),
                    crate::proto::HighlightSpec {
                        path: String::new(),
                        start,
                        end,
                        note: note(h),
                    },
                ));
            }
        }
        let (path, line, excerpts) = match item {
            Value::String(p) => (p.as_str(), None, None),
            Value::Object(o) => (
                o.get("path")
                    .and_then(Value::as_str)
                    .ok_or("each file needs a path")?,
                o.get("line").and_then(Value::as_u64),
                match o.get("excerpts") {
                    None | Some(Value::Null) => None,
                    Some(Value::Bool(b)) => Some(*b),
                    Some(_) => return Err("excerpts is true or false".to_string()),
                },
            ),
            _ => return Err("each file is an object with a path".to_string()),
        };
        if line == Some(0) {
            return Err(format!("{path}: lines start at 1"));
        }
        files.push(crate::proto::CodeFile {
            path: relative_to(&root, cwd, path)?,
            line: line.map(|l| l as usize),
            excerpts,
        });
    }
    check_count(marks.len())?;
    // Every file must be there to be shown.
    let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let snap = crate::git::read_code(&root, rev.as_deref(), &paths).map_err(|e| e.to_string())?;
    let lengths: Vec<usize> = snap
        .files
        .iter()
        .map(|(_, data)| data.split(|&b| b == b'\n').count() - usize::from(data.ends_with(b"\n")))
        .collect();
    for (f, &n) in files.iter().zip(&lengths) {
        if let Some(l) = f.line
            && l > n.max(1)
        {
            return Err(format!("{} has {n} lines; line {l} does not exist", f.path));
        }
    }
    let mut highlights: Vec<crate::proto::HighlightSpec> = marks
        .into_iter()
        .map(|(i, mut h)| {
            if h.end > lengths[i] {
                return Err(format!(
                    "{} has {} lines; lines {}-{} do not exist",
                    files[i].path, lengths[i], h.start, h.end
                ));
            }
            h.path = files[i].path.clone();
            Ok(h)
        })
        .collect::<Result<_, String>>()?;
    // Numbered as the viewer shows them: by file in the listed order, then line.
    let at = |path: &str| files.iter().position(|f| f.path == path);
    highlights.sort_by_key(|h| (at(&h.path), h.start, h.end));
    let mut what = files
        .iter()
        .map(|f| match f.line {
            Some(l) => format!("{} at line {l}", f.path),
            None => f.path.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(r) = &rev {
        what.push_str(&format!(" as of {r}"));
    }
    what.push_str(&list_highlights(&highlights));
    let view = crate::proto::ViewSpec {
        code: Some(crate::proto::CodeSpec { rev, files }),
        highlights,
        ..Default::default()
    };
    Ok((root, view, what))
}

/// An optional list argument.
fn list<'a>(args: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("{key} must be a list")),
    }
}

fn note(h: &Value) -> Option<String> {
    h.get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

/// "40-60" (or with an en dash), "42", or 42: 1-based and inclusive.
fn parse_lines(v: Option<&Value>) -> Result<(usize, usize), String> {
    let bad = || "lines must look like \"40-60\" or \"42\"".to_string();
    let (start, end) = match v {
        Some(Value::Number(n)) => {
            let n = n.as_u64().ok_or_else(bad)? as usize;
            (n, n)
        }
        Some(Value::String(s)) => {
            let s = s.trim();
            match s.split_once(['-', '–']) {
                Some((a, b)) => (
                    a.trim().parse().map_err(|_| bad())?,
                    b.trim().parse().map_err(|_| bad())?,
                ),
                None => {
                    let n = s.parse().map_err(|_| bad())?;
                    (n, n)
                }
            }
        }
        _ => return Err(bad()),
    };
    if start == 0 || end < start {
        return Err(format!(
            "lines {start}-{end}: lines start at 1 and run forward"
        ));
    }
    Ok((start, end))
}

fn check_count(n: usize) -> Result<(), String> {
    let max = crate::proto::MAX_HIGHLIGHTS;
    if n > max {
        return Err(format!(
            "at most {max} highlights per call (got {n}); keep the ones that matter most"
        ));
    }
    Ok(())
}

/// The highlights with the numbers the user sees, in order.
fn list_highlights(highlights: &[crate::proto::HighlightSpec]) -> String {
    if highlights.is_empty() {
        return String::new();
    }
    let each: Vec<String> = highlights
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let lines = if h.start == h.end {
                h.start.to_string()
            } else {
                format!("{}-{}", h.start, h.end)
            };
            format!("{} {} {lines}", crate::proto::circled(i + 1), h.path)
        })
        .collect();
    format!("; highlights {}", each.join(", "))
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
    let mut highlights = Vec::new();
    for h in list(args, "highlights")? {
        let path = h
            .get("path")
            .and_then(Value::as_str)
            .ok_or("each highlight needs a path")?;
        let (start, end) = parse_lines(h.get("lines"))?;
        highlights.push(crate::proto::HighlightSpec {
            path: relative_to(&root, cwd, path)?,
            start,
            end,
            note: note(h),
        });
    }
    check_count(highlights.len())?;
    // Numbered as the viewer shows them: by path, as the diff orders files,
    // then line.
    highlights.sort_by(|a, b| (&a.path, a.start, a.end).cmp(&(&b.path, b.start, b.end)));
    let mut what = range
        .clone()
        .unwrap_or_else(|| "everything since the branch point".to_string());
    if !files.is_empty() {
        what.push_str(&format!(", narrowed to {}", files.join(", ")));
    }
    what.push_str(&list_highlights(&highlights));
    let view = crate::proto::ViewSpec {
        range,
        files,
        highlights,
        ..Default::default()
    };
    Ok((root, view, what))
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

    #[test]
    fn a_code_call_is_checked_before_it_is_sent() {
        let dir = branch_with_history("mcp-code");
        let sub = dir.join("src");
        let (root, view, what) = prepare_code(
            &json!({ "files": [{ "path": "deep/one.rs", "line": 1 }, { "path": "../a.txt" }] }),
            &sub,
        )
        .unwrap();
        assert_eq!(root, dir);
        let code = view.code.unwrap();
        assert_eq!(code.files[0].path, "src/deep/one.rs");
        assert_eq!(code.files[0].line, Some(1));
        assert_eq!(code.files[1].path, "a.txt");
        assert_eq!(what, "src/deep/one.rs at line 1, a.txt");

        let (_, view, what) =
            prepare_code(&json!({ "files": ["gone.txt"], "rev": "main" }), &dir).unwrap();
        assert_eq!(view.code.unwrap().rev.as_deref(), Some("main"));
        assert_eq!(what, "gone.txt as of main");

        let err = prepare_code(&json!({ "files": ["gone.txt"] }), &dir).unwrap_err();
        assert!(err.contains("gone.txt does not exist"), "{err}");
        let err =
            prepare_code(&json!({ "files": [{ "path": "a.txt", "line": 99 }] }), &dir).unwrap_err();
        assert!(err.contains("has 1 lines"), "{err}");
        assert!(prepare_code(&json!({ "files": [] }), &dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn highlights_are_checked_and_carried() {
        let dir = branch_with_history("mcp-highlights");
        let (_, view, what) = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "highlights": [{ "lines": "1", "note": " here " }] }] }),
            &dir,
        )
        .unwrap();
        assert_eq!(
            view.highlights,
            [crate::proto::HighlightSpec {
                path: "a.txt".into(),
                start: 1,
                end: 1,
                note: Some("here".into()),
            }]
        );
        assert_eq!(what, "a.txt; highlights ① a.txt 1");
        let err = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "highlights": [{ "lines": "1-5" }] }] }),
            &dir,
        )
        .unwrap_err();
        assert!(err.contains("do not exist"), "{err}");

        let (_, view, what) = prepare_diff(
            &json!({ "highlights": [{ "path": "src/deep/one.rs", "lines": "2–3" }, { "path": "a.txt", "lines": 1 }] }),
            &dir,
        )
        .unwrap();
        assert_eq!(view.highlights.len(), 2);
        assert_eq!((view.highlights[1].start, view.highlights[1].end), (2, 3));
        assert!(
            what.ends_with("; highlights ① a.txt 1, ② src/deep/one.rs 2-3"),
            "{what}"
        );
        for bad in ["0-2", "5-3", "x"] {
            assert!(
                prepare_diff(
                    &json!({ "highlights": [{ "path": "a.txt", "lines": bad }] }),
                    &dir
                )
                .is_err(),
                "{bad}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn highlights_are_numbered_by_where_they_sit() {
        let dir = branch_with_history("mcp-numbering");
        // show_code: by file in the order listed, then by line.
        let (_, view, what) = prepare_code(
            &json!({ "files": [
                { "path": "b.txt", "highlights": [{ "lines": "1", "note": "b" }] },
                { "path": "a.txt", "highlights": [{ "lines": "1", "note": "a" }] },
            ], "rev": "HEAD~2" }),
            &dir,
        )
        .unwrap();
        assert_eq!(
            what,
            "b.txt, a.txt as of HEAD~2; highlights ① b.txt 1, ② a.txt 1"
        );
        let notes: Vec<_> = view.highlights.iter().map(|h| h.note.as_deref()).collect();
        assert_eq!(notes, [Some("b"), Some("a")]);

        // show_diff: by path, as the diff orders its files, then by line.
        let (_, view, what) = prepare_diff(
            &json!({ "highlights": [
                { "path": "src/deep/one.rs", "lines": "9" },
                { "path": "b.txt", "lines": "7-8" },
                { "path": "src/deep/one.rs", "lines": "3" },
                { "path": "a.txt", "lines": "5" },
            ] }),
            &dir,
        )
        .unwrap();
        assert!(
            what.ends_with(
                "; highlights ① a.txt 5, ② b.txt 7-8, ③ src/deep/one.rs 3, ④ src/deep/one.rs 9"
            ),
            "{what}"
        );
        let starts: Vec<_> = view.highlights.iter().map(|h| h.start).collect();
        assert_eq!(starts, [5, 7, 3, 9]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn at_most_twenty_highlights_per_call() {
        let dir = branch_with_history("mcp-cap");
        let many = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|_| json!({ "path": "a.txt", "lines": "1" }))
                .collect()
        };
        let (_, view, what) = prepare_diff(&json!({ "highlights": many(20) }), &dir).unwrap();
        assert_eq!(view.highlights.len(), 20);
        assert!(what.contains("⑳ a.txt 1"), "{what}");
        let err = prepare_diff(&json!({ "highlights": many(21) }), &dir).unwrap_err();
        assert_eq!(
            err,
            "at most 20 highlights per call (got 21); keep the ones that matter most"
        );
        let err = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "highlights": many(21) }] }),
            &dir,
        )
        .unwrap_err();
        assert!(
            err.starts_with("at most 20 highlights per call (got 21)"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn excerpts_are_carried_per_file() {
        let dir = branch_with_history("mcp-excerpts");
        let (_, view, _) = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "excerpts": true }, { "path": "b.txt" }] }),
            &dir,
        )
        .unwrap();
        let files = view.code.unwrap().files;
        assert_eq!(files[0].excerpts, Some(true));
        assert_eq!(files[1].excerpts, None);
        let err = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "excerpts": "yes" }] }),
            &dir,
        )
        .unwrap_err();
        assert_eq!(err, "excerpts is true or false");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn show_code_schema_keeps_line_and_highlights_apart() {
        let tools = tools();
        let schema = &tools[0]["inputSchema"];
        assert_eq!(tools[0]["name"], "show_code");
        assert_eq!(
            schema["if"]["properties"]["files"]["contains"]["required"],
            json!(["highlights"])
        );
        assert_eq!(
            schema["then"]["properties"]["files"]["items"]["not"]["required"],
            json!(["line"])
        );
    }
}
