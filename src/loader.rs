//! Background loading: snapshots are computed off the UI thread, newest
//! request first, so a burst of requests (stepping through commits, a reload
//! during typing) costs one load, not one per request.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};

use crate::git::{self, Range};
use crate::watch::Msg;

pub struct Request {
    pub loc: u64,
    pub seq: u64,
    pub dir: PathBuf,
    pub base: Option<String>,
    pub range: Range,
    /// Files to show as they are, instead of the range's changes.
    pub code: Option<Code>,
}

pub struct Code {
    pub rev: Option<String>,
    pub paths: Vec<String>,
}

/// What a load produced.
pub enum Load {
    Diff(Box<git::Snapshot>),
    Code(git::CodeSnapshot),
}

/// Start the loader thread; send it requests through the returned sender.
pub fn spawn(tx: Sender<Msg>) -> Sender<Request> {
    let (req_tx, req_rx) = channel::<Request>();
    std::thread::spawn(move || {
        while let Ok(mut req) = req_rx.recv() {
            // Only the newest request matters; skip anything it superseded.
            while let Ok(newer) = req_rx.try_recv() {
                req = newer;
            }
            let result = Box::new(match &req.code {
                Some(code) => git::read_code(&req.dir, code.rev.as_deref(), &code.paths)
                    .map(Load::Code)
                    .map_err(|e| e.to_string()),
                None => git::snapshot(&req.dir, req.base.as_deref(), &req.range)
                    .map(|s| Load::Diff(Box::new(s)))
                    .map_err(|e| format!("{e:#}")),
            });
            let msg = Msg::Loaded {
                loc: req.loc,
                seq: req.seq,
                result,
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
    req_tx
}
