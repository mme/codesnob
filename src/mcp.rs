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
const INSTRUCTIONS: &str = "codesnob is the user's code viewer, open in a terminal beside you. When the user wants to see code, show it there instead of pasting it into the chat: `show_code` for files and lines, with marks on the lines you mean; `show_diff` for what changed between any two states (the branch point, commits, branches, staged work, a pull request). Typical requests: \"show me the code\", \"where is this handled\", \"open that file\", \"what changed\", \"show me the diff\", \"compare main and this branch\", \"show me PR 123\". A call updates that repository's view in the viewer; the user gets back to where they were with Esc. Read files with your own tools; codesnob is for showing. Marks are lettered [A], [B]… in the viewer and shared between you and the user, each with a thread under it; yours and theirs are the same thing. A mark stays until someone removes it: a show adds marks and removes none, so clear your own with `clear_marks` once they have served, and before pointing somewhere new. Put words on a mark only when the user asks you to explain, review or comment on code: in the show call, or with `mark` for lines they are already looking at. The user marks lines in the viewer too; call `get_review` whenever they say they reviewed, commented or marked something, or a reply tells you they have unread comments, and answer to the letters. A location is a repository or a pull request, named by the repository's path (repo) or as owner/repo#123 (pr); the review tools default to the location on screen, so name one only for marks elsewhere. `list_locations` lists the locations open, with their unread comments. Answer in the thread under a mark with `reply`, and when you change the code under a mark, keep it on the right lines with `move_mark`. A mark the user made or wrote under is theirs: `clear_marks` removes it only whole and only when the user explicitly asks you to delete it.";

fn tools() -> Value {
    json!([{
        "name": "show_code",
        "description": "Show source code to the user in their codesnob viewer: whole files, or excerpts around a line and marked line ranges. Use it when the user asks to see, open or look at code, or where something is defined or handled, and whenever they ask you to show code while explaining. Prefer a few precise marks over large ranges. Put text on a mark only when the user asks you to explain, review or comment on the code. Marks stay until cleared, so clear yours when they have served.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The files to show, in this order. Unless excerpts says otherwise, one file shows whole and several show as excerpts around their line and marks.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Relative to your working directory, or absolute." },
                            "line": { "type": "integer", "minimum": 1, "description": "Open at this line, marked." },
                            "excerpts": { "type": "boolean", "description": "true shows only this file's line and marks, each with a few lines around it, like a diff; false shows the whole file." },
                            "marks": {
                                "type": "array",
                                "description": "Line ranges to mark in this file. The viewer letters marks as [A], [B] and so on, in the order they are made, by file in the order of files, then by line; the letters are fixed and never reused, and every mark stays until removed. At most 20 per call. The user steps through them with < and >; the result lists them in that order, so refer to them by their letters.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "lines": { "type": "string", "description": "1-based and inclusive: \"40-60\", or \"42\" for one line." },
                                        "text": { "type": "string", "description": "Words shown under the lines; a newline starts a new row. Only when the user asked you to explain, review or comment on the code." }
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
            // With marks the view opens at the first, so no file takes a line.
            "if": { "properties": { "files": { "contains": { "required": ["marks"] } } } },
            "then": { "properties": { "files": { "items": { "not": { "required": ["line"] } } } } }
        }
    }, {
        "name": "show_diff",
        "description": "Show the user a diff in their codesnob viewer. range takes git syntax (\"main...feature\", \"HEAD~3..HEAD\", a single commit, \"stash@{0}\"), \"worktree\", \"staged\" or \"unstaged\", or a pull request (\"#123\", \"owner/repo#123\", a URL). Without a range it shows everything since the branch point, uncommitted work included. Use it when the user asks what changed, to see or review a diff, to compare branches or commits, or to look at a pull request. Optionally narrow to files and mark lines; put text on a mark only when the user asks you to explain, review or comment on the code. Marks stay until cleared, so clear yours when they have served.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "range": { "type": "string", "description": "What to compare; see the description. Default: everything since the branch point." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Only these files, relative to your working directory." },
                "marks": {
                    "type": "array",
                    "description": "Line ranges to mark. Lines are the new side's, or the old side's in a deleted file; unchanged lines are shown too. The viewer letters marks as [A], [B] and so on, in the order they are made, by file in path order, as the diff lists its files, then by line; the letters are fixed and never reused, and every mark stays until removed. At most 20 per call. The user steps through them with < and >; the result lists them in that order, so refer to them by their letters.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Relative to your working directory, or absolute." },
                            "lines": { "type": "string", "description": "1-based and inclusive: \"40-60\", or \"42\" for one line." },
                            "text": { "type": "string", "description": "Words shown under the lines; a newline starts a new row. Only when the user asked you to explain, review or comment on the code." }
                        },
                        "required": ["path", "lines"]
                    }
                },
                "repo": { "type": "string", "description": "A directory in the repository. Defaults to your working directory." }
            }
        }
    }, {
        "name": "list_locations",
        "description": "List the locations open in the user's codesnob viewers: each repository or pull request, how to name it (repo or pr), what it shows, which is on screen, and how many marks it has, how many of the user's words, and how many of those are unread. get_review reads the location on screen unless given one, so use this when the user says they commented and the location on screen has nothing new, or to find a location they mean.",
        "inputSchema": { "type": "object", "properties": {} }
    }, {
        "name": "get_review",
        "description": "Read every mark on a location, yours and the user's: each with its letter, who made it, its file and lines, its text, the code under the user's marks, and the thread under it. Call it whenever the user says they reviewed, commented on or marked something, or a reply says they have unread comments. The user's words count as read once returned; those written or edited since your last call are flagged new. Marks on deleted lines give old-side line numbers; a stale mark's lines no longer exist.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "A directory in the repository, to name a location that is not on screen. Defaults to the location on screen." },
                "pr": { "type": "string", "description": "A pull request, to name its location: \"owner/repo#123\", \"#123\" or a URL." },
            }
        }
    }, {
        "name": "mark",
        "description": "Mark lines with words in the user's codesnob viewer, without changing what it shows: for comments on code the user is already looking at, as when they ask you to review it. Put everything you have to say in one call: the marks land together and the viewer takes the user to the first. They are marks like any other, lettered and kept until cleared; the user sees them when their file is on screen, so show the file if they are not looking at it. Lines are the new side's, as in show_diff.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "marks": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The marks, lettered top to bottom as the viewer shows them: by file, then by line. At most 20 per call.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "The file, relative to your working directory." },
                            "lines": { "type": "string", "description": "The lines, 1-based and inclusive: \"40-60\", or \"42\"." },
                            "text": { "type": "string", "description": "The words, a few sentences at most." }
                        },
                        "required": ["path", "lines", "text"]
                    }
                },
                "repo": { "type": "string", "description": "A directory in the repository, to resolve paths in and to name the location. Defaults to your working directory and the location on screen." },
                "pr": { "type": "string", "description": "A pull request, to name its location: \"owner/repo#123\", \"#123\" or a URL." }
            },
            "required": ["marks"]
        }
    }, {
        "name": "reply",
        "description": "Answer in the thread under a mark in the user's codesnob viewer. Use it to say what you did about one of the user's marks, or to ask about it; the mark stays the user's to resolve, so reply instead of clearing it. You can also answer under one of your own marks once the user has written there.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "A directory in the repository, to name a location that is not on screen. Defaults to the location on screen." },
                "pr": { "type": "string", "description": "A pull request, to name its location: \"owner/repo#123\", \"#123\" or a URL." },
                "letter": { "type": "string", "description": "The mark to answer under: \"C\"." },
                "text": { "type": "string", "description": "The reply, one or two sentences." }
            },
            "required": ["letter", "text"]
        }
    }, {
        "name": "move_mark",
        "description": "Move a mark in the user's codesnob viewer to other lines, when you changed the code it was on and it no longer sits on the code it is about: the user's, or one of yours. It keeps its letter, its text and its thread. Lines are the new side's, as in show_diff.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "letter": { "type": "string", "description": "The mark to move: \"C\"." },
                "lines": { "type": "string", "description": "Where it goes, 1-based and inclusive: \"40-60\", or \"42\"." },
                "path": { "type": "string", "description": "The file it goes to, relative to your working directory. Defaults to the file it is on." },
                "repo": { "type": "string", "description": "A directory in the repository, to resolve path in and to name the location. Defaults to your working directory and the location on screen." },
                "pr": { "type": "string", "description": "A pull request, to name its location: \"owner/repo#123\", \"#123\" or a URL." }
            },
            "required": ["letter", "lines"]
        }
    }, {
        "name": "clear_marks",
        "description": "Remove marks from a location in the user's codesnob viewer: the ones named by letter, else all of yours (mine: marks you made that the user has not written under), the user's (user) or every mark (all). Clear yours once they have served and before pointing somewhere new. A mark the user made, or wrote in the thread of, is theirs: it goes whole, thread and all, and only when the user explicitly asked you to delete it, which you say with user_explicitly_asked_to_delete_comments. To say you dealt with one of the user's marks, reply to it instead.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "A directory in the repository, to name a location that is not on screen. Defaults to the location on screen." },
                "pr": { "type": "string", "description": "A pull request, to name its location: \"owner/repo#123\", \"#123\" or a URL." },
                "which": { "type": "string", "enum": ["mine", "user", "all"], "description": "Which marks to remove when no letters are given. Default: mine." },
                "letters": { "type": "array", "items": { "type": "string" }, "description": "The marks to remove, by letter: [\"C\", \"D\"]." },
                "user_explicitly_asked_to_delete_comments": { "type": "boolean", "description": "true only when the user explicitly asked you to delete their marks; without it, a call that would remove a mark of the user's is refused. Default: false." }
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
        "list_locations" => tool_result(list_locations()),
        "get_review" => tool_result(get_review(&args)),
        "clear_marks" => tool_result(clear_marks(&args)),
        "mark" => tool_result(mark(&args)),
        "reply" => tool_result(reply_in_thread(&args)),
        "move_mark" => tool_result(move_mark(&args)),
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
    Ok(shown(&r, &what))
}

fn show_code(args: &Value) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let (root, view, what) = prepare_code(args, &cwd)?;
    let mut client = link::connect()?;
    let r = link::request_show(&mut client, root, Some(view)).map_err(no_viewer)?;
    Ok(shown(&r, &what))
}

/// Words for a show reply: where it went, what it was, and what the viewer
/// made of it when it answered.
fn shown(r: &crate::proto::ShowResult, what: &str) -> String {
    // With the viewer's answer, the letters say it all; without, the
    // order the marks were sent in is the best there is.
    let what = match &r.ack {
        Some(ack) if !ack.marks.is_empty() => {
            what.split("; marks, lettered").next().unwrap_or(what)
        }
        _ => what,
    };
    let mut text = format!("{}: {what}", link::describe(r));
    match &r.ack {
        Some(ack) => {
            if !ack.marks.is_empty() {
                let each: Vec<String> = ack
                    .marks
                    .iter()
                    .map(|m| format!("[{}] {} {}", m.label, m.path, m.lines_word()))
                    .collect();
                text.push_str(&format!("; marks {}", each.join(", ")));
            }
            if !ack.location.is_empty() {
                text.push_str(&format!("; location {}", ack.location));
            }
            if !ack.applied {
                text.push_str("; the user is reviewing, so the viewer holds this until they accept it or you read their comments");
            }
            let location = if ack.location.is_empty() {
                "the location on screen"
            } else {
                ack.location.as_str()
            };
            text.push_str(&crate::proto::unread_note(location, ack.unread));
            text.push_str(&crate::proto::mine_note(location, &ack.mine));
        }
        None => text.push_str(
            "; the viewer did not say which letters it gave, call get_review to see them",
        ),
    }
    text.push_str("; the user gets back to their previous view with Esc");
    text
}

/// The location a call names with `repo` or `pr`, if either.
fn location_arg(args: &Value, cwd: &Path) -> Option<crate::proto::LocationRef> {
    let word = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let repo = word("repo").map(|r| {
        let joined = cwd.join(r);
        std::fs::canonicalize(&joined).unwrap_or_else(|_| lexical(&joined))
    });
    let pr = word("pr");
    (repo.is_some() || pr.is_some()).then_some(crate::proto::LocationRef { repo, pr })
}

fn list_locations() -> Result<String, String> {
    let mut client = link::connect()?;
    link::request_locations(&mut client).map_err(no_viewer)
}

fn get_review(args: &Value) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut client = link::connect()?;
    let v = link::request_query(
        &mut client,
        crate::proto::AgentQuery::Review {
            location: location_arg(args, &cwd),
        },
    )
    .map_err(no_viewer)?;
    let r: crate::proto::ReviewResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(r.render())
}

/// Check a clear_marks call: which marks, and the letters, upper-cased and
/// without brackets.
fn clear_args(args: &Value) -> Result<(String, Vec<String>), String> {
    let which = args
        .get("which")
        .and_then(Value::as_str)
        .unwrap_or("mine")
        .to_string();
    if !["mine", "user", "all"].contains(&which.as_str()) {
        return Err(format!("which must be mine, user or all, not {which}"));
    }
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
    Ok((which, letters))
}

fn clear_marks(args: &Value) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let (which, letters) = clear_args(args)?;
    let user_asked = args
        .get("user_explicitly_asked_to_delete_comments")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut client = link::connect()?;
    let v = link::request_query(
        &mut client,
        crate::proto::AgentQuery::ClearMarks {
            location: location_arg(args, &cwd),
            which,
            letters,
            user_asked,
        },
    )
    .map_err(no_viewer)?;
    let r: crate::proto::ClearResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(format!(
        "{}{}",
        r.render(),
        crate::proto::unread_note(&r.location, r.unread)
    ))
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

/// Check a mark call: every mark's file made relative to the checkout
/// root, as the viewer names files, its lines, and its text.
pub fn mark_args(args: &Value, cwd: &Path) -> Result<crate::proto::AgentQuery, String> {
    let items = list(args, "marks")?;
    if items.is_empty() {
        return Err("marks must list at least one mark".to_string());
    }
    check_count(items.len())?;
    let root = repo_root(args, cwd)?;
    let mut marks = Vec::with_capacity(items.len());
    for item in items {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or("each mark needs a path: the file it goes on")?;
        if item.get("lines").is_none() {
            return Err(format!(
                "{path}: lines is required: where the mark goes, like \"40-42\""
            ));
        }
        let (start, end) = parse_lines(item.get("lines"))?;
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("{path}: text is required: the words on the mark"))?
            .to_string();
        marks.push(crate::proto::MarkSpec {
            path: relative_to(&root, cwd, path)?,
            start,
            end,
            text: Some(text),
        });
    }
    Ok(crate::proto::AgentQuery::Mark {
        location: location_arg(args, cwd),
        marks,
    })
}

fn mark(args: &Value) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let query = mark_args(args, &cwd)?;
    let mut client = link::connect()?;
    let v = link::request_query(&mut client, query).map_err(no_viewer)?;
    let r: crate::proto::MarkResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(format!(
        "{}{}{}",
        r.render(),
        crate::proto::unread_note(&r.location, r.unread),
        crate::proto::mine_note(&r.location, &r.mine)
    ))
}

fn reply_in_thread(args: &Value) -> Result<String, String> {
    let letter = letter_arg(args)?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("text is required: the reply")?
        .to_string();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut client = link::connect()?;
    let v = link::request_query(
        &mut client,
        crate::proto::AgentQuery::Reply {
            location: location_arg(args, &cwd),
            letter,
            text,
        },
    )
    .map_err(no_viewer)?;
    let r: crate::proto::ReplyResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(format!(
        "{}{}",
        r.render(),
        crate::proto::unread_note(&r.location, r.unread)
    ))
}

/// Check a move_mark call: the letter, the lines, and the file made
/// relative to the checkout root, as the viewer names files.
pub fn move_args(args: &Value, cwd: &Path) -> Result<crate::proto::AgentQuery, String> {
    let letter = letter_arg(args)?;
    if args.get("lines").is_none() {
        return Err("lines is required: where the mark goes, like \"40-42\"".to_string());
    }
    let (start, end) = parse_lines(args.get("lines"))?;
    let path = match args.get("path").and_then(Value::as_str).map(str::trim) {
        Some(p) if !p.is_empty() => Some(relative_to(&repo_root(args, cwd)?, cwd, p)?),
        _ => None,
    };
    Ok(crate::proto::AgentQuery::MoveMark {
        location: location_arg(args, cwd),
        letter,
        path,
        start,
        end,
    })
}

fn move_mark(args: &Value) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let query = move_args(args, &cwd)?;
    let mut client = link::connect()?;
    let v = link::request_query(&mut client, query).map_err(no_viewer)?;
    let r: crate::proto::MoveResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(format!(
        "{}{}",
        r.render(),
        crate::proto::unread_note(&r.location, r.unread)
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
    let mut marks: Vec<(usize, crate::proto::MarkSpec)> = Vec::new();
    for item in items {
        if let Value::Object(_) = item {
            for h in list(item, "marks")? {
                let (start, end) = parse_lines(h.get("lines"))?;
                marks.push((
                    files.len(),
                    crate::proto::MarkSpec {
                        path: String::new(),
                        start,
                        end,
                        text: text(h),
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
    let mut placed: Vec<crate::proto::MarkSpec> = marks
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
    placed.sort_by_key(|h| (at(&h.path), h.start, h.end));
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
    what.push_str(&list_marks(&placed));
    let view = crate::proto::ViewSpec {
        code: Some(crate::proto::CodeSpec { rev, files }),
        marks: placed,
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
            "at most {max} marks per call (got {n}); keep the ones that matter most"
        ));
    }
    Ok(())
}

/// The marks in the order the viewer letters them.
fn list_marks(marks: &[crate::proto::MarkSpec]) -> String {
    if marks.is_empty() {
        return String::new();
    }
    let each: Vec<String> = marks
        .iter()
        .map(|h| {
            let lines = if h.start == h.end {
                h.start.to_string()
            } else {
                format!("{}-{}", h.start, h.end)
            };
            format!("{} {lines}", h.path)
        })
        .collect();
    format!(
        "; marks, lettered in this order in the viewer: {}",
        each.join(", ")
    )
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
    let mut marks = Vec::new();
    for h in list(args, "marks")? {
        let path = h
            .get("path")
            .and_then(Value::as_str)
            .ok_or("each mark needs a path")?;
        let (start, end) = parse_lines(h.get("lines"))?;
        marks.push(crate::proto::MarkSpec {
            path: relative_to(&root, cwd, path)?,
            start,
            end,
            text: text(h),
        });
    }
    check_count(marks.len())?;
    // Numbered as the viewer shows them: by path, as the diff orders files,
    // then line.
    marks.sort_by(|a, b| (&a.path, a.start, a.end).cmp(&(&b.path, b.start, b.end)));
    let mut what = range
        .clone()
        .unwrap_or_else(|| "everything since the branch point".to_string());
    if !files.is_empty() {
        what.push_str(&format!(", narrowed to {}", files.join(", ")));
    }
    what.push_str(&list_marks(&marks));
    let view = crate::proto::ViewSpec {
        range,
        files,
        marks,
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
    fn marks_are_checked_and_carried() {
        let dir = branch_with_history("mcp-marks");
        let (_, view, what) = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "marks": [{ "lines": "1", "text": " here " }] }] }),
            &dir,
        )
        .unwrap();
        assert_eq!(
            view.marks,
            [crate::proto::MarkSpec {
                path: "a.txt".into(),
                start: 1,
                end: 1,
                text: Some("here".into()),
            }]
        );
        assert_eq!(
            what,
            "a.txt; marks, lettered in this order in the viewer: a.txt 1"
        );
        let err = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "marks": [{ "lines": "1-5" }] }] }),
            &dir,
        )
        .unwrap_err();
        assert!(err.contains("do not exist"), "{err}");

        let (_, view, what) = prepare_diff(
            &json!({ "marks": [{ "path": "src/deep/one.rs", "lines": "2–3" }, { "path": "a.txt", "lines": 1 }] }),
            &dir,
        )
        .unwrap();
        assert_eq!(view.marks.len(), 2);
        assert_eq!((view.marks[1].start, view.marks[1].end), (2, 3));
        assert!(
            what.ends_with(
                "; marks, lettered in this order in the viewer: a.txt 1, src/deep/one.rs 2-3"
            ),
            "{what}"
        );
        for bad in ["0-2", "5-3", "x"] {
            assert!(
                prepare_diff(
                    &json!({ "marks": [{ "path": "a.txt", "lines": bad }] }),
                    &dir
                )
                .is_err(),
                "{bad}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn marks_are_numbered_by_where_they_sit() {
        let dir = branch_with_history("mcp-numbering");
        // show_code: by file in the order listed, then by line.
        let (_, view, what) = prepare_code(
            &json!({ "files": [
                { "path": "b.txt", "marks": [{ "lines": "1", "text": "b" }] },
                { "path": "a.txt", "marks": [{ "lines": "1", "text": "a" }] },
            ], "rev": "HEAD~2" }),
            &dir,
        )
        .unwrap();
        assert_eq!(
            what,
            "b.txt, a.txt as of HEAD~2; marks, lettered in this order in the viewer: b.txt 1, a.txt 1"
        );
        let notes: Vec<_> = view.marks.iter().map(|h| h.text.as_deref()).collect();
        assert_eq!(notes, [Some("b"), Some("a")]);

        // show_diff: by path, as the diff orders its files, then by line.
        let (_, view, what) = prepare_diff(
            &json!({ "marks": [
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
                "; marks, lettered in this order in the viewer: a.txt 5, b.txt 7-8, src/deep/one.rs 3, src/deep/one.rs 9"
            ),
            "{what}"
        );
        let starts: Vec<_> = view.marks.iter().map(|h| h.start).collect();
        assert_eq!(starts, [5, 7, 3, 9]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn at_most_twenty_marks_per_call() {
        let dir = branch_with_history("mcp-cap");
        let many = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|_| json!({ "path": "a.txt", "lines": "1" }))
                .collect()
        };
        let (_, view, what) = prepare_diff(&json!({ "marks": many(20) }), &dir).unwrap();
        assert_eq!(view.marks.len(), 20);
        assert_eq!(what.matches("a.txt 1").count(), 20, "{what}");
        let err = prepare_diff(&json!({ "marks": many(21) }), &dir).unwrap_err();
        assert_eq!(
            err,
            "at most 20 marks per call (got 21); keep the ones that matter most"
        );
        let err = prepare_code(
            &json!({ "files": [{ "path": "a.txt", "marks": many(21) }] }),
            &dir,
        )
        .unwrap_err();
        assert!(
            err.starts_with("at most 20 marks per call (got 21)"),
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
    fn a_show_reply_names_the_letters_the_location_and_unread_comments() {
        let mut r = crate::proto::ShowResult {
            repo: true,
            viewer: "v1".to_string(),
            workspace: None,
            picked_by: "first".to_string(),
            ack: Some(crate::proto::ShowAck {
                location: "/r".to_string(),
                dir: PathBuf::from("/r"),
                marks: vec![
                    crate::proto::MarkInfo {
                        label: "C".to_string(),
                        path: "a.txt".to_string(),
                        start: 5,
                        end: 5,
                        ..Default::default()
                    },
                    crate::proto::MarkInfo {
                        label: "D".to_string(),
                        path: "b.txt".to_string(),
                        start: 7,
                        end: 8,
                        old_side: true,
                        ..Default::default()
                    },
                ],
                mine: vec![
                    crate::proto::MarkInfo {
                        label: "A".to_string(),
                        path: "a.txt".to_string(),
                        ..Default::default()
                    },
                    crate::proto::MarkInfo {
                        label: "C".to_string(),
                        path: "a.txt".to_string(),
                        ..Default::default()
                    },
                    crate::proto::MarkInfo {
                        label: "D".to_string(),
                        path: "b.txt".to_string(),
                        ..Default::default()
                    },
                ],
                unread: 2,
                applied: true,
            }),
        };
        assert_eq!(
            shown(
                &r,
                "a.txt, b.txt; marks, lettered in this order in the viewer: a.txt 5, b.txt 7-8"
            ),
            "shown in viewer v1: a.txt, b.txt; marks [C] a.txt 5, [D] b.txt old 7-8; location /r; the user has 2 unread comments on /r, call get_review; your marks on /r: [A] [C] a.txt, [D] b.txt; remember to remove any marks that are not needed any more; the user gets back to their previous view with Esc"
        );
        r.ack.as_mut().unwrap().applied = false;
        assert!(
            shown(&r, "x").contains("holds this until"),
            "{}",
            shown(&r, "x")
        );
        r.ack = None;
        assert!(
            shown(&r, "x").contains("did not say which letters"),
            "{}",
            shown(&r, "x")
        );
    }

    #[test]
    fn clear_arguments_are_checked() {
        let (which, letters) = clear_args(&json!({})).unwrap();
        assert_eq!((which.as_str(), letters.as_slice()), ("mine", &[][..]));
        let (which, letters) =
            clear_args(&json!({ "which": "all", "letters": [" c", "[D]", "aa"] })).unwrap();
        assert_eq!(which, "all");
        assert_eq!(letters, ["C", "D", "AA"]);
        let err = clear_args(&json!({ "which": "theirs" })).unwrap_err();
        assert_eq!(err, "which must be mine, user or all, not theirs");
        let err = clear_args(&json!({ "letters": ["3"] })).unwrap_err();
        assert_eq!(
            err,
            "\"3\" is not a mark's letter; replies have no letters of their own, name the mark they are under"
        );
        let err = clear_args(&json!({ "letters": ["c12"] })).unwrap_err();
        assert!(
            err.starts_with("\"c12\" is not a mark's letter; replies"),
            "{err}"
        );
        let err = clear_args(&json!({ "letters": "C" })).unwrap_err();
        assert_eq!(err, "letters must be a list");
    }

    #[test]
    fn marks_are_checked_and_their_paths_made_relative_to_the_checkout() {
        let dir = branch_with_history("mcp-mark");
        let sub = dir.join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        let q = mark_args(
            &json!({ "marks": [
                { "path": "one.rs", "lines": "4-6", "text": " unused " },
                { "path": "../a.txt", "lines": 1, "text": "and this" }
            ], "repo": ".." }),
            &sub,
        )
        .unwrap();
        assert_eq!(
            q,
            crate::proto::AgentQuery::Mark {
                location: Some(crate::proto::LocationRef {
                    repo: Some(dir.clone()),
                    pr: None,
                }),
                marks: vec![
                    crate::proto::MarkSpec {
                        path: "deep/one.rs".to_string(),
                        start: 4,
                        end: 6,
                        text: Some("unused".to_string()),
                    },
                    crate::proto::MarkSpec {
                        path: "a.txt".to_string(),
                        start: 1,
                        end: 1,
                        text: Some("and this".to_string()),
                    },
                ],
            }
        );
        let err = mark_args(&json!({}), &dir).unwrap_err();
        assert_eq!(err, "marks must list at least one mark");
        let err = mark_args(&json!({ "marks": [{ "lines": 9, "text": "x" }] }), &dir).unwrap_err();
        assert_eq!(err, "each mark needs a path: the file it goes on");
        let err = mark_args(
            &json!({ "marks": [{ "path": "a.txt", "text": "x" }] }),
            &dir,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "a.txt: lines is required: where the mark goes, like \"40-42\""
        );
        let err = mark_args(
            &json!({ "marks": [{ "path": "a.txt", "lines": 9, "text": " " }] }),
            &dir,
        )
        .unwrap_err();
        assert_eq!(err, "a.txt: text is required: the words on the mark");
        let many: Vec<Value> = (1..=21)
            .map(|i| json!({ "path": "a.txt", "lines": i, "text": "x" }))
            .collect();
        let err = mark_args(&json!({ "marks": many }), &dir).unwrap_err();
        assert!(
            err.starts_with("at most 20 marks per call (got 21)"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_move_is_checked_and_its_path_made_relative_to_the_checkout() {
        let dir = branch_with_history("mcp-move");
        let sub = dir.join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        let q = move_args(
            &json!({ "letter": "[b]", "lines": "4-6", "path": "one.rs", "repo": ".." }),
            &sub,
        )
        .unwrap();
        assert_eq!(
            q,
            crate::proto::AgentQuery::MoveMark {
                location: Some(crate::proto::LocationRef {
                    repo: Some(dir.clone()),
                    pr: None,
                }),
                letter: "B".to_string(),
                path: Some("deep/one.rs".to_string()),
                start: 4,
                end: 6,
            }
        );
        let q = move_args(&json!({ "letter": "B", "lines": 9 }), &dir).unwrap();
        assert!(
            matches!(
                q,
                crate::proto::AgentQuery::MoveMark {
                    location: None,
                    path: None,
                    start: 9,
                    end: 9,
                    ..
                }
            ),
            "{q:?}"
        );
        let q = move_args(&json!({ "letter": "B", "lines": 9, "pr": "#3" }), &dir).unwrap();
        assert!(
            matches!(
                q,
                crate::proto::AgentQuery::MoveMark {
                    location: Some(crate::proto::LocationRef {
                        repo: None,
                        pr: Some(_)
                    }),
                    ..
                }
            ),
            "{q:?}"
        );
        let err = move_args(&json!({ "letter": "B2", "lines": 9 }), &dir).unwrap_err();
        assert!(err.contains("replies have no letters"), "{err}");
        let err = move_args(&json!({ "lines": "4" }), &dir).unwrap_err();
        assert!(err.starts_with("letter is required"), "{err}");
        let err = move_args(&json!({ "letter": "B" }), &dir).unwrap_err();
        assert!(err.starts_with("lines is required"), "{err}");
        let err = move_args(&json!({ "letter": "B", "lines": "6-4" }), &dir).unwrap_err();
        assert!(err.contains("run forward"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_review_tools_are_listed() {
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "show_code",
                "show_diff",
                "list_locations",
                "get_review",
                "mark",
                "reply",
                "move_mark",
                "clear_marks"
            ]
        );
        let tool = |name: &str| {
            tools
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .unwrap()
                .clone()
        };
        assert!(tool("show_code")["inputSchema"]["properties"]["append"].is_null());
        assert_eq!(
            tool("list_locations")["inputSchema"]["properties"],
            json!({})
        );
        assert_eq!(
            tool("reply")["inputSchema"]["required"],
            json!(["letter", "text"])
        );
        assert_eq!(tool("mark")["inputSchema"]["required"], json!(["marks"]));
        assert_eq!(
            tool("mark")["inputSchema"]["properties"]["marks"]["items"]["required"],
            json!(["path", "lines", "text"])
        );
        for name in ["get_review", "mark", "reply", "move_mark", "clear_marks"] {
            let props = &tool(name)["inputSchema"]["properties"];
            assert!(
                props["repo"].is_object() && props["pr"].is_object() && props["location"].is_null(),
                "{name} names locations by repo or pr"
            );
        }
        assert_eq!(
            tool("move_mark")["inputSchema"]["required"],
            json!(["letter", "lines"])
        );
        let clear = &tool("clear_marks")["inputSchema"]["properties"];
        assert_eq!(clear["which"]["enum"], json!(["mine", "user", "all"]));
        assert_eq!(
            clear["user_explicitly_asked_to_delete_comments"]["type"],
            "boolean"
        );
    }

    #[test]
    fn show_code_schema_keeps_line_and_marks_apart() {
        let tools = tools();
        let schema = &tools[0]["inputSchema"];
        assert_eq!(tools[0]["name"], "show_code");
        assert_eq!(
            schema["if"]["properties"]["files"]["contains"]["required"],
            json!(["marks"])
        );
        assert_eq!(
            schema["then"]["properties"]["files"]["items"]["not"]["required"],
            json!(["line"])
        );
    }
}
