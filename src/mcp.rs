//! A minimal Model Context Protocol server over stdio: JSON-RPC 2.0, one
//! message per line. Showing goes to the viewer nearest the agent's own
//! workspace; questions about marks go to the daemon, which keeps them.

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
const INSTRUCTIONS: &str = "codesnob is the user's code viewer, open in a terminal beside you. When the user wants to see code, show it there instead of pasting it into the chat: `show` puts files, a diff or a pull request on their screen, with marks on the lines you mean. Typical requests: \"show me the code\", \"where is this handled\", \"open that file\", \"what changed\", \"show me the diff\", \"compare main and this branch\", \"show me PR 123\". Read files with your own tools; codesnob is for showing. Every call names its location: a checkout by absolute directory, with the view after # (`/dir` the files as they are, `/dir#main` as of a revision, `/dir#main...HEAD` or `/dir#abc~..abc` a comparison, `/dir#branch` everything since the branch point, `#worktree` `#staged` `#unstaged`), or a pull request as owner/repo#123. Every reply names the location it acted on; copy it into the next call. Marks are lettered [A], [B]… in the viewer and shared between you and the user, each with a thread under it; yours and theirs are the same thing, and a mark belongs to its location. A mark stays until deleted: a show adds marks and removes none unless told to, so pass replace when you point somewhere new, and delete yours once they have served. Put words on a mark only when the user asks you to explain, review or comment on code. A ```mermaid fence in a mark's words is drawn as a diagram, so a flow or a sequence can be shown next to the code it describes. The user marks lines in the viewer too; call `review` whenever they say they reviewed, commented or marked something, or a reply says they have unread words, and answer to the letters with `reply`. A mark the user made or wrote under is theirs: it goes only whole, and only when the user explicitly asks you to delete it. The user gets back to their previous view with Esc.";

fn tools() -> Value {
    let location = json!({
        "type": "string",
        "description": "A checkout by absolute directory, with the view after #: \"/home/u/repo\" the files as they are, \"/home/u/repo#main\" as of a revision, \"/home/u/repo#main...HEAD\" or \"#abc~..abc\" a comparison, \"#branch\" everything since the branch point with uncommitted work, \"#worktree\" \"#staged\" \"#unstaged\" as in git. Or a pull request: \"owner/repo#123\" or its URL. Never relative; copy it from the last reply."
    });
    json!([{
        "name": "show",
        "description": "Show code to the user in their codesnob viewer, and set the marks on it. location says which checkout and view, or which pull request; files what is on screen; marks what is pointed at. Whole files are shown unless a file has lines. With a diff view (#branch, #main...HEAD, a PR), no files means every changed file; with files as they are, no files means the marked lines with a few around them. A new mark is lettered in the order given and stays until deleted; pass replace when you point somewhere new, so your old marks go. Put text on a mark only when the user asks you to explain, review or comment on the code. A mark with a letter is an existing one: with path and lines it moves there, keeping its words and thread; with delete it goes. A show with only moves and deletions leaves the view as it is.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "location": location,
                "files": {
                    "type": "array",
                    "description": "The files to show, in this order, relative to the checkout root. A file may appear more than once with different lines.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "lines": { "type": "string", "description": "Only this region, 1-based and inclusive: \"40-60\", or \"42\". Omit for the whole file." }
                        },
                        "required": ["path"]
                    }
                },
                "marks": {
                    "type": "array",
                    "description": "At most 20 new marks per call. {path, lines, text?} adds one on lines that are shown; {letter, path, lines} moves the mark with that letter; {letter, delete: true} removes it. Lines are the new side's in a diff. The reply lists new marks by letter.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Relative to the checkout root." },
                            "lines": { "type": "string", "description": "1-based and inclusive: \"40-60\", or \"42\"." },
                            "text": { "type": "string", "description": "Words under the lines of a new mark, with ```mermaid fences drawn. Only when the user asked you to explain, review or comment." },
                            "letter": { "type": "string", "description": "An existing mark: \"C\"." },
                            "delete": { "type": "boolean", "description": "true removes the mark letter names, thread and all." }
                        }
                    }
                },
                "replace": { "type": "boolean", "description": "true removes your own marks on the location first: the ones the user has not written under. Default: false." },
                "user_explicitly_asked_to_delete": { "type": "boolean", "description": "true only when the user explicitly asked you to delete their marks; without it, deleting a mark the user made or wrote under is refused. Default: false." }
            },
            "required": ["location"]
        }
    }, {
        "name": "review",
        "description": "Read the marks on a location, yours and the user's: each with its letter, who made it, its file and lines, its text, the code under the user's marks, and the thread under it. Call it whenever the user says they reviewed, commented on or marked something, or a reply says they have unread words. The user's words count as read once returned; those written or edited since your last call are flagged new. A stale mark's lines no longer exist.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "location": location,
                "letters": { "type": "array", "items": { "type": "string" }, "description": "Only these marks: [\"C\", \"D\"]. Default: all." }
            },
            "required": ["location"]
        }
    }, {
        "name": "reply",
        "description": "Answer in the thread under a mark in the user's codesnob viewer: to say what you did about one of the user's marks, or to ask about it. The mark stays theirs to resolve, so reply instead of deleting it. You can also answer under one of your own marks once the user has written there.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "location": location,
                "letter": { "type": "string", "description": "The mark to answer under: \"C\"." },
                "text": { "type": "string", "description": "The reply, one or two sentences; ```mermaid fences are drawn." }
            },
            "required": ["location", "letter", "text"]
        }
    }])
}

fn call(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "show" => tool_result(show(&args)),
        "review" => tool_result(review(&args)),
        "reply" => tool_result(reply_in_thread(&args)),
        _ => tool_result(Err(format!("unknown tool: {name}"))),
    }
}

fn tool_result(r: Result<String, String>) -> Value {
    match r {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
    }
}

/// A show call, checked: the location as named, its checkout root (empty
/// for a pull request), and what to send.
#[derive(Debug)]
pub struct ShowArgs {
    pub location: String,
    pub files: Vec<crate::proto::FileSpec>,
    pub marks: Vec<crate::proto::MarkEntry>,
    pub replace: bool,
    pub user_asked: bool,
}

fn show(args: &Value) -> Result<String, String> {
    let a = show_args(args)?;
    let mut client = link::connect()?;
    let r = link::request_show(
        &mut client,
        a.location,
        a.files,
        a.marks,
        a.replace,
        a.user_asked,
    )
    .map_err(no_viewer)?;
    Ok(r.render())
}

/// The location a call names, checked: what it is, and its checkout root.
fn location_arg(args: &Value) -> Result<(String, crate::location::Named, PathBuf), String> {
    let s = args
        .get("location")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("location is required: an absolute directory, optionally with #view, or a pull request as owner/repo#142")?;
    let named = crate::location::parse(s)?;
    let (key, root, _) = crate::registry::resolve(&named)?;
    Ok((key.name(), named, root))
}

/// Check a show call before anything reaches the daemon: the location,
/// the files and regions, and every mark.
pub fn show_args(args: &Value) -> Result<ShowArgs, String> {
    use crate::location::{Named, View};
    use crate::proto::{FileSpec, MarkEntry, MarkSpec};
    let (location, named, root) = location_arg(args)?;
    let view = match &named {
        Named::Repo { view, .. } => Some(view.clone()),
        Named::Pr(_) => None,
    };
    let mut files = Vec::new();
    for item in list(args, "files")? {
        let (path, lines) = match item {
            Value::String(p) => (p.as_str(), None),
            Value::Object(o) => (
                o.get("path")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or("each file needs a path")?,
                match o.get("lines") {
                    None | Some(Value::Null) => None,
                    some => Some(parse_lines(some)?),
                },
            ),
            _ => return Err("each file is an object with a path".to_string()),
        };
        files.push(FileSpec {
            path: relative_to(&root, path)?,
            lines,
        });
    }
    let mut marks = Vec::new();
    let mut adds = 0;
    for item in list(args, "marks")? {
        let Value::Object(o) = item else {
            return Err("each mark is an object".to_string());
        };
        let letter = match o.get("letter") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(crate::marks::parse_letter)
                    .ok_or_else(|| not_a_letter(v))?,
            ),
        };
        let delete = matches!(o.get("delete"), Some(Value::Bool(true)));
        let path = o
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty());
        let words = text(item);
        match (letter, delete) {
            (Some(letter), true) => marks.push(MarkEntry::Delete { letter }),
            (Some(letter), false) => {
                if words.is_some() {
                    return Err(format!(
                        "[{letter}] with text: a move keeps the mark's words; to say something new, reply under it"
                    ));
                }
                let path = path
                    .ok_or_else(|| format!("[{letter}]: path is required: the file it goes to"))?;
                let (start, end) =
                    parse_lines(o.get("lines")).map_err(|e| format!("[{letter}]: {e}"))?;
                marks.push(MarkEntry::Move {
                    letter,
                    path: relative_to(&root, path)?,
                    start,
                    end,
                });
            }
            (None, true) => return Err("delete needs the letter of the mark to remove".to_string()),
            (None, false) => {
                let path = path.ok_or("each new mark needs a path: the file it goes on")?;
                if o.get("lines").is_none() {
                    return Err(format!(
                        "{path}: lines is required: where the mark goes, like \"40-42\""
                    ));
                }
                let (start, end) = parse_lines(o.get("lines"))?;
                adds += 1;
                marks.push(MarkEntry::Add(MarkSpec {
                    path: relative_to(&root, path)?,
                    start,
                    end,
                    text: words,
                }));
            }
        }
    }
    check_count(adds)?;
    let replace = matches!(args.get("replace"), Some(Value::Bool(true)));
    let user_asked = matches!(
        args.get("user_explicitly_asked_to_delete"),
        Some(Value::Bool(true))
    );
    let goes = !files.is_empty() || adds > 0 || marks.is_empty() && !replace;
    // Files as they are must be there, and the marks on shown lines.
    if let Some(view) = &view {
        match view {
            View::Files | View::Rev { .. } if goes => {
                if files.is_empty() && adds == 0 {
                    return Err(
                        "files is required to show files as they are; a diff view (#branch, #main...HEAD) shows every changed file"
                            .to_string(),
                    );
                }
                let rev = match view {
                    View::Rev { rev } => Some(rev.as_str()),
                    _ => None,
                };
                check_shown(&root, rev, &files, &marks)?;
            }
            View::Files | View::Rev { .. } => {}
            diff => {
                if let Some(spec) = diff.git_spec() {
                    crate::git::parse_spec(&root, &spec).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    Ok(ShowArgs {
        location,
        files,
        marks,
        replace,
        user_asked,
    })
}

/// Every file to show is there at `rev`, every region is within it, and
/// every new mark lies on lines that will be shown.
fn check_shown(
    root: &Path,
    rev: Option<&str>,
    files: &[crate::proto::FileSpec],
    marks: &[crate::proto::MarkEntry],
) -> Result<(), String> {
    use crate::proto::MarkEntry;
    let mut paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    for m in marks {
        if let MarkEntry::Add(spec) = m
            && !paths.contains(&spec.path)
        {
            paths.push(spec.path.clone());
        }
    }
    let snap = crate::git::read_code(root, rev, &paths).map_err(|e| e.to_string())?;
    let length = |path: &str| -> usize {
        snap.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, data)| {
                data.split(|&b| b == b'\n').count() - usize::from(data.ends_with(b"\n"))
            })
            .unwrap_or(0)
    };
    for f in files {
        if let Some((a, b)) = f.lines {
            let n = length(&f.path);
            if b > n.max(1) {
                return Err(format!(
                    "{} has {n} lines; lines {a}-{b} do not exist",
                    f.path
                ));
            }
        }
    }
    for m in marks {
        let MarkEntry::Add(spec) = m else { continue };
        let shown = files
            .iter()
            .filter(|f| f.path == spec.path)
            .collect::<Vec<_>>();
        let inside = if shown.is_empty() {
            // Shown for the mark's sake, as excerpts: only the file's length limits it.
            spec.end <= length(&spec.path).max(1)
        } else {
            shown.iter().any(|f| match f.lines {
                None => spec.end <= length(&spec.path).max(1),
                Some((a, b)) => spec.start >= a && spec.end <= b,
            })
        };
        if !inside {
            let n = length(&spec.path);
            return Err(if shown.iter().any(|f| f.lines.is_some()) {
                format!(
                    "{} {}-{}: a mark must lie on lines that are shown; show that region too",
                    spec.path, spec.start, spec.end
                )
            } else {
                format!(
                    "{} has {n} lines; lines {}-{} do not exist",
                    spec.path, spec.start, spec.end
                )
            });
        }
    }
    Ok(())
}

fn review(args: &Value) -> Result<String, String> {
    let (location, ..) = location_arg(args)?;
    let letters: Vec<String> = match args.get("letters") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .and_then(crate::marks::parse_letter)
                    .ok_or_else(|| not_a_letter(v))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("letters must be a list".to_string()),
    };
    let mut client = link::connect()?;
    let v = link::request_query(
        &mut client,
        crate::proto::AgentQuery::Review { location, letters },
    )
    .map_err(no_viewer)?;
    let r: crate::proto::ReviewResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(r.render())
}

/// Why a value is no mark's letter: replies used to have labels of their
/// own, C2, and no longer do.
fn not_a_letter(v: &Value) -> String {
    let has_digit = v
        .as_str()
        .is_some_and(|s| s.chars().any(|c| c.is_ascii_digit()));
    if has_digit {
        format!(
            "{v} is not a mark's letter; replies have no letters of their own, name the mark they are under"
        )
    } else {
        format!("{v} is not a mark's letter")
    }
}

/// The mark a call names, by its `letter` argument.
fn letter_arg(args: &Value) -> Result<String, String> {
    let v = args
        .get("letter")
        .ok_or("letter is required: the mark's, like \"C\"")?;
    v.as_str()
        .and_then(crate::marks::parse_letter)
        .ok_or_else(|| not_a_letter(v))
}

fn reply_in_thread(args: &Value) -> Result<String, String> {
    let (location, ..) = location_arg(args)?;
    let letter = letter_arg(args)?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("text is required: the reply")?
        .to_string();
    let mut client = link::connect()?;
    let v = link::request_query(
        &mut client,
        crate::proto::AgentQuery::Reply {
            location,
            letter,
            text,
        },
    )
    .map_err(no_viewer)?;
    let r: crate::proto::ReplyResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(r.render())
}

/// An optional list argument.
fn list<'a>(args: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("{key} must be a list")),
    }
}

fn text(h: &Value) -> Option<String> {
    h.get("text")
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
    let max = crate::proto::MAX_MARKS;
    if n > max {
        return Err(format!(
            "at most {max} new marks per call (got {n}); keep the ones that matter most"
        ));
    }
    Ok(())
}

fn no_viewer(e: String) -> String {
    match e.as_str() {
        "no codesnob viewer is connected" => {
            "no codesnob viewer is running; ask the user to open one with `codesnob`".to_string()
        }
        _ => e,
    }
}

/// `path` as the agent gave it, relative to the checkout root or absolute
/// under it, made relative to the root the way the viewer names files.
/// With no root known, as for a pull request, it is taken as relative.
fn relative_to(root: &Path, path: &str) -> Result<String, String> {
    let p = Path::new(path.trim());
    let rel = if p.is_absolute() {
        if root.as_os_str().is_empty() {
            return Err(format!(
                "{path}: paths in a pull request are relative to its checkout"
            ));
        }
        let full = std::fs::canonicalize(p).unwrap_or_else(|_| lexical(p));
        full.strip_prefix(root)
            .map(Path::to_path_buf)
            .map_err(|_| format!("{path} is outside the checkout at {}", root.display()))?
    } else {
        let clean = lexical(p);
        if clean.components().next().is_none()
            || clean.starts_with("..")
            || clean.as_os_str().to_string_lossy().starts_with("..")
        {
            return Err(format!(
                "{path} leads outside the checkout; paths are relative to its root"
            ));
        }
        clean
    };
    Ok(rel.to_string_lossy().into_owned())
}

/// Resolve `.` and `..` without touching the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
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
    use crate::proto::{FileSpec, MarkEntry};
    use crate::testutil::branch_with_history;

    fn at(dir: &Path, view: &str) -> String {
        format!("{}{view}", dir.display())
    }

    #[test]
    fn a_show_is_checked_before_it_is_sent() {
        let dir = branch_with_history("mcp-show");
        let a = show_args(&json!({
            "location": at(&dir.join("src"), "#main...feature"),
            "files": [{ "path": "src/deep/one.rs" }, "a.txt"],
        }))
        .unwrap();
        assert_eq!(
            a.location,
            at(&dir, "#main...feature"),
            "the root, with the view"
        );
        assert_eq!(
            a.files,
            [
                FileSpec {
                    path: "src/deep/one.rs".to_string(),
                    lines: None
                },
                FileSpec {
                    path: "a.txt".to_string(),
                    lines: None
                }
            ]
        );
        assert!(a.marks.is_empty() && !a.replace && !a.user_asked);

        let err = show_args(&json!({ "location": at(&dir, "#nope...feature") })).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        let err = show_args(&json!({ "location": "src", "files": ["a.txt"] })).unwrap_err();
        assert!(err.contains("absolute directory"), "{err}");
        let err = show_args(&json!({ "files": ["a.txt"] })).unwrap_err();
        assert!(err.contains("location is required"), "{err}");
        let err = show_args(&json!({
            "location": at(&dir, ""),
            "files": ["../elsewhere.rs"]
        }))
        .unwrap_err();
        assert!(err.contains("outside the checkout"), "{err}");
        let err = show_args(&json!({ "location": at(&dir, "") })).unwrap_err();
        assert!(err.contains("files is required"), "{err}");
        let a = show_args(&json!({ "location": at(&dir, "#branch") })).unwrap();
        assert!(a.files.is_empty(), "a diff shows every changed file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn files_as_they_are_must_exist_and_regions_and_marks_fit() {
        let dir = branch_with_history("mcp-files");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let a = show_args(&json!({
            "location": at(&dir, ""),
            "files": [{ "path": dir.join("a.txt").display().to_string(), "lines": "1-2" }],
            "marks": [{ "path": "a.txt", "lines": "2", "text": " here " }],
        }))
        .unwrap();
        assert_eq!(
            a.files[0].lines,
            Some((1, 2)),
            "absolute paths under the root are fine"
        );
        assert_eq!(a.files[0].path, "a.txt");
        assert_eq!(
            a.marks,
            [MarkEntry::Add(crate::proto::MarkSpec {
                path: "a.txt".to_string(),
                start: 2,
                end: 2,
                text: Some("here".to_string())
            })]
        );
        let err =
            show_args(&json!({ "location": at(&dir, ""), "files": ["nope.rs"] })).unwrap_err();
        assert!(err.contains("nope.rs"), "{err}");
        let err = show_args(&json!({
            "location": at(&dir, ""),
            "files": [{ "path": "a.txt", "lines": "1-400" }]
        }))
        .unwrap_err();
        assert!(err.contains("do not exist"), "{err}");
        let err = show_args(&json!({
            "location": at(&dir, ""),
            "files": [{ "path": "a.txt", "lines": "1-2" }],
            "marks": [{ "path": "a.txt", "lines": "3" }]
        }))
        .unwrap_err();
        assert!(err.contains("show that region too"), "{err}");
        let err = show_args(&json!({
            "location": at(&dir, ""),
            "marks": [{ "path": "a.txt", "lines": "900" }]
        }))
        .unwrap_err();
        assert!(err.contains("do not exist"), "{err}");
        let a = show_args(&json!({
            "location": at(&dir, ""),
            "marks": [{ "path": "a.txt", "lines": "1" }]
        }))
        .unwrap();
        assert!(
            a.files.is_empty(),
            "the marked files are shown for the marks' sake"
        );
        let err = show_args(&json!({
            "location": at(&dir, "#nope"),
            "files": ["a.txt"]
        }))
        .unwrap_err();
        assert!(err.contains("nope"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mark_entries_add_move_or_delete() {
        let dir = branch_with_history("mcp-entries");
        let a = show_args(&json!({
            "location": at(&dir, "#branch"),
            "marks": [
                { "letter": "c", "delete": true },
                { "letter": "[D]", "path": "./src/deep/one.rs", "lines": "4-5" },
                { "path": "a.txt", "lines": 1 }
            ],
            "replace": true,
            "user_explicitly_asked_to_delete": true
        }))
        .unwrap();
        assert_eq!(
            a.marks,
            [
                MarkEntry::Delete {
                    letter: "C".to_string()
                },
                MarkEntry::Move {
                    letter: "D".to_string(),
                    path: "src/deep/one.rs".to_string(),
                    start: 4,
                    end: 5
                },
                MarkEntry::Add(crate::proto::MarkSpec {
                    path: "a.txt".to_string(),
                    start: 1,
                    end: 1,
                    text: None
                })
            ]
        );
        assert!(a.replace && a.user_asked);
        let err = show_args(&json!({
            "location": at(&dir, "#branch"),
            "marks": [{ "letter": "C", "path": "a.txt", "lines": "1", "text": "new words" }]
        }))
        .unwrap_err();
        assert!(err.contains("a move keeps the mark's words"), "{err}");
        let err = show_args(&json!({
            "location": at(&dir, "#branch"),
            "marks": [{ "delete": true }]
        }))
        .unwrap_err();
        assert!(err.contains("delete needs the letter"), "{err}");
        let err = show_args(&json!({
            "location": at(&dir, "#branch"),
            "marks": [{ "letter": "C2", "delete": true }]
        }))
        .unwrap_err();
        assert!(err.contains("replies have no letters"), "{err}");
        let err = show_args(&json!({
            "location": at(&dir, "#branch"),
            "marks": [{ "path": "a.txt" }]
        }))
        .unwrap_err();
        assert!(err.contains("lines is required"), "{err}");
        let many: Vec<Value> = (1..=21)
            .map(|i| json!({ "path": "a.txt", "lines": i.to_string() }))
            .collect();
        let err =
            show_args(&json!({ "location": at(&dir, "#branch"), "marks": many })).unwrap_err();
        assert!(err.contains("at most 20"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pull_request_takes_relative_paths_and_no_view() {
        let a = show_args(&json!({
            "location": "Acme/Widget#7",
            "files": ["src/lib.rs"],
            "marks": [{ "path": "src/lib.rs", "lines": "3-4" }]
        }))
        .unwrap();
        assert_eq!(a.location, "acme/widget#7");
        let err = show_args(&json!({ "location": "acme/widget#7", "files": ["/abs/lib.rs"] }))
            .unwrap_err();
        assert!(err.contains("relative to its checkout"), "{err}");
        let err = show_args(&json!({ "location": "#7" })).unwrap_err();
        assert!(err.contains("owner/repo#7"), "{err}");
    }

    #[test]
    fn the_three_tools_are_listed_with_a_location_each() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["show", "review", "reply"]);
        for t in tools.as_array().unwrap() {
            let required = t["inputSchema"]["required"].as_array().unwrap();
            assert!(required.contains(&json!("location")), "{}", t["name"]);
            assert!(
                t["inputSchema"]["properties"]["location"]["description"]
                    .as_str()
                    .unwrap()
                    .contains("#main...HEAD")
            );
        }
        let marks = &tools[0]["inputSchema"]["properties"]["marks"]["items"]["properties"];
        for key in ["path", "lines", "text", "letter", "delete"] {
            assert!(marks.get(key).is_some(), "{key}");
        }
        assert!(INSTRUCTIONS.contains("`review`") && INSTRUCTIONS.contains("replace"));
        assert!(!INSTRUCTIONS.contains("show_code") && !INSTRUCTIONS.contains("list_locations"));
    }

    #[test]
    fn paths_are_cleaned_without_escaping_the_checkout() {
        let root = Path::new("/work/repo");
        assert_eq!(relative_to(root, "./src/../src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(
            relative_to(root, "/work/repo/src/a.rs").unwrap(),
            "src/a.rs"
        );
        assert!(
            relative_to(root, "../x.rs")
                .unwrap_err()
                .contains("outside")
        );
        assert!(
            relative_to(root, "/elsewhere/x.rs")
                .unwrap_err()
                .contains("outside")
        );
        assert!(relative_to(root, "").is_err());
    }
}
