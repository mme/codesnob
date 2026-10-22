//! How agents name a location: a checkout and a view of it in one string,
//! or a pull request. `/dir` is the files as they are, `/dir#main` the files
//! at a revision, `/dir#main...HEAD` a comparison, `/dir#branch` everything
//! since the branch point; `owner/repo#142` and a PR URL are a pull request.
//! Nothing here is resolved against a working directory: a location names
//! itself, wherever it is read.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What state of a checkout's files a location shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum View {
    /// The files as they are in the working tree.
    Files,
    /// The files as of a revision: `#main`.
    Rev { rev: String },
    /// Everything since the branch point, uncommitted work included.
    Branch,
    /// HEAD against the working tree: uncommitted changes.
    Worktree,
    /// HEAD against the index.
    Staged,
    /// The index against the working tree.
    Unstaged,
    /// Two states, as git spells them: `a..b`, `a...b`.
    Compare { spec: String },
}

impl View {
    /// The words after `#`; read loosely, written back the one way.
    pub fn parse(s: &str) -> View {
        let s = s.trim();
        match s.to_ascii_lowercase().as_str() {
            "" => View::Files,
            "branch" => View::Branch,
            "worktree" => View::Worktree,
            "staged" => View::Staged,
            "unstaged" => View::Unstaged,
            _ if s.contains("..") => View::Compare {
                spec: s.split_whitespace().collect(),
            },
            _ => View::Rev { rev: s.to_string() },
        }
    }

    /// What follows the checkout in the location's name.
    pub fn suffix(&self) -> String {
        match self {
            View::Files => String::new(),
            View::Rev { rev } => format!("#{rev}"),
            View::Branch => "#branch".to_string(),
            View::Worktree => "#worktree".to_string(),
            View::Staged => "#staged".to_string(),
            View::Unstaged => "#unstaged".to_string(),
            View::Compare { spec } => format!("#{spec}"),
        }
    }

    /// A comparison, as opposed to files as they are at one state.
    pub fn is_diff(&self) -> bool {
        !matches!(self, View::Files | View::Rev { .. })
    }

    /// What it shows, in words.
    pub fn words(&self) -> String {
        match self {
            View::Files => "the working tree".to_string(),
            View::Rev { rev } => format!("as of {rev}"),
            View::Branch => "everything since the branch point".to_string(),
            View::Worktree => "uncommitted changes".to_string(),
            View::Staged => "staged changes".to_string(),
            View::Unstaged => "unstaged changes".to_string(),
            View::Compare { spec } => spec.clone(),
        }
    }

    /// The comparison as `git::parse_spec` reads it, for the views that
    /// are one.
    pub fn git_spec(&self) -> Option<String> {
        match self {
            View::Files | View::Rev { .. } | View::Branch => None,
            View::Worktree => Some("worktree".to_string()),
            View::Staged => Some("staged".to_string()),
            View::Unstaged => Some("unstaged".to_string()),
            View::Compare { spec } => Some(spec.clone()),
        }
    }
}

/// A location as an agent wrote it, before anything is looked up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Named {
    Repo { dir: PathBuf, view: View },
    Pr(crate::pr::Spec),
}

/// Read a location: an absolute directory with an optional `#view`, a pull
/// request as `owner/repo#142`, or a PR URL.
pub fn parse(s: &str) -> Result<Named, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(
            "location is required: an absolute directory, optionally with #view, or owner/repo#142"
                .to_string(),
        );
    }
    if s.starts_with('/') {
        // A directory that exists, `#` and all, is the location itself.
        if Path::new(s).is_dir() {
            return Ok(Named::Repo {
                dir: PathBuf::from(s),
                view: View::Files,
            });
        }
        let (dir, view) = match s.rsplit_once('#') {
            Some((d, v)) => (d, View::parse(v)),
            None => (s, View::Files),
        };
        let dir = dir.trim_end_matches('/');
        if dir.is_empty() {
            return Err(format!("{s}: no directory before #"));
        }
        return Ok(Named::Repo {
            dir: PathBuf::from(dir),
            view,
        });
    }
    if s.starts_with('#') {
        return Err(format!(
            "{s} names no repository; a pull request is owner/repo{s}"
        ));
    }
    if let Some(spec) = crate::pr::parse(s)
        && spec.repo.is_some()
    {
        return Ok(Named::Pr(spec));
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return Err(format!("{s} is not a pull request URL"));
    }
    Err(format!(
        "{s} is not a location: give an absolute directory, optionally with #view, or a pull request as owner/repo#142"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_are_read_loosely_and_written_one_way() {
        assert_eq!(View::parse(""), View::Files);
        assert_eq!(View::parse(" Branch "), View::Branch);
        assert_eq!(View::parse("staged"), View::Staged);
        assert_eq!(
            View::parse("main ... HEAD"),
            View::Compare {
                spec: "main...HEAD".to_string()
            }
        );
        assert_eq!(
            View::parse("abc~..abc"),
            View::Compare {
                spec: "abc~..abc".to_string()
            }
        );
        assert_eq!(
            View::parse("v1.2"),
            View::Rev {
                rev: "v1.2".to_string()
            }
        );
        assert_eq!(View::Files.suffix(), "");
        assert_eq!(View::Branch.suffix(), "#branch");
        assert_eq!(View::parse("main...HEAD").suffix(), "#main...HEAD");
        assert_eq!(View::parse("main").suffix(), "#main");
        assert!(View::Branch.is_diff() && !View::parse("main").is_diff());
    }

    #[test]
    fn a_location_is_a_directory_with_a_view_or_a_pull_request() {
        assert_eq!(
            parse("/work/repo").unwrap(),
            Named::Repo {
                dir: PathBuf::from("/work/repo"),
                view: View::Files
            }
        );
        assert_eq!(
            parse("/work/repo/#main...HEAD").unwrap(),
            Named::Repo {
                dir: PathBuf::from("/work/repo"),
                view: View::Compare {
                    spec: "main...HEAD".to_string()
                }
            }
        );
        assert_eq!(
            parse("/work/repo#branch").unwrap(),
            Named::Repo {
                dir: PathBuf::from("/work/repo"),
                view: View::Branch
            }
        );
        let Named::Pr(spec) = parse("Acme/Widget#7").unwrap() else {
            panic!("a pull request");
        };
        assert_eq!(spec.number, 7);
        assert_eq!(spec.repo.unwrap().slug(), "Acme/Widget");
        let Named::Pr(spec) = parse("https://github.com/acme/widget/pull/9").unwrap() else {
            panic!("a pull request");
        };
        assert_eq!(spec.number, 9);
    }

    #[test]
    fn relative_directories_and_bare_numbers_are_refused() {
        assert!(parse("src/ui").unwrap_err().contains("absolute directory"));
        assert!(parse(".").unwrap_err().contains("absolute directory"));
        assert!(parse("#7").unwrap_err().contains("owner/repo#7"));
        assert!(parse("").unwrap_err().contains("required"));
        assert!(parse("/#main").unwrap_err().contains("no directory"));
    }

    #[test]
    fn a_directory_that_exists_with_a_hash_in_its_name_is_itself() {
        let dir = crate::testutil::fresh_dir("location#hash");
        let got = parse(dir.to_str().unwrap()).unwrap();
        assert_eq!(
            got,
            Named::Repo {
                dir: dir.clone(),
                view: View::Files
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
