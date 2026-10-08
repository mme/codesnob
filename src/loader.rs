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
            let result = Box::new(
                git::snapshot(&req.dir, req.base.as_deref(), &req.range)
                    .map_err(|e| format!("{e:#}")),
            );
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
