//! Everything the viewer can do, in one table. The palette lists it, with
//! each command's shortcut beside it, so the table is also the help screen.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Action {
    Commits,
    OlderCommit,
    NewerCommit,
    ShowCommit(gix::ObjectId),
    ToggleDescription,
    Files,
    /// Narrow to the current file, or show all files again.
    Narrow,
    /// Move to file `n`: jump in the full view, switch file while narrowed.
    GoToFile(usize),
    NextHunk,
    PrevHunk,
    NextFile,
    PrevFile,
    Top,
    Bottom,
    RangeEverything,
    RangeUncommitted,
    ExplainBase,
    Reload,
    Quit,
}

pub struct Command {
    pub action: Action,
    pub name: &'static str,
    /// The shortcut shown beside the name; empty when there is none.
    pub keys: &'static str,
}

/// In palette order: what the palette is for first, movement after.
pub const COMMANDS: &[Command] = &[
    Command {
        action: Action::Commits,
        name: "Commits…",
        keys: "c",
    },
    Command {
        action: Action::Files,
        name: "Files…",
        keys: "f",
    },
    Command {
        action: Action::Narrow,
        name: "Narrow to this file",
        keys: "z",
    },
    Command {
        action: Action::RangeEverything,
        name: "Everything since branch point",
        keys: "Esc",
    },
    Command {
        action: Action::RangeUncommitted,
        name: "Uncommitted changes only",
        keys: "",
    },
    Command {
        action: Action::OlderCommit,
        name: "Older commit",
        keys: "<",
    },
    Command {
        action: Action::NewerCommit,
        name: "Newer commit",
        keys: ">",
    },
    Command {
        action: Action::ToggleDescription,
        name: "Toggle description",
        keys: "d",
    },
    Command {
        action: Action::ExplainBase,
        name: "How the base was chosen",
        keys: "",
    },
    Command {
        action: Action::Reload,
        name: "Reload",
        keys: "r",
    },
    Command {
        action: Action::NextHunk,
        name: "Next hunk",
        keys: "n",
    },
    Command {
        action: Action::PrevHunk,
        name: "Previous hunk",
        keys: "p",
    },
    Command {
        action: Action::NextFile,
        name: "Next file",
        keys: "N",
    },
    Command {
        action: Action::PrevFile,
        name: "Previous file",
        keys: "P",
    },
    Command {
        action: Action::Top,
        name: "Top",
        keys: "g",
    },
    Command {
        action: Action::Bottom,
        name: "Bottom",
        keys: "G",
    },
    Command {
        action: Action::Quit,
        name: "Quit",
        keys: "q",
    },
];
