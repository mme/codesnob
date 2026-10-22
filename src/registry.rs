//! The locations that agents and viewers share: a view of a checkout, or a
//! pull request, each with its own marks. The daemon keeps the registry;
//! viewers hold copies of the locations they have open and show them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::location::{Named, View};
use crate::marks::Marks;

/// What a location is, and how it is told from every other.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocationKey {
    /// A view of a checkout, by its root.
    Repo { dir: PathBuf, view: View },
    /// A pull request, by its repository's `owner/name` and number.
    Pr { repo: String, number: u64 },
}

impl LocationKey {
    /// How agents name it: `/dir`, `/dir#view`, or `owner/repo#142`.
    pub fn name(&self) -> String {
        match self {
            LocationKey::Repo { dir, view } => format!("{}{}", dir.display(), view.suffix()),
            LocationKey::Pr { repo, number } => format!("{repo}#{number}"),
        }
    }

    pub fn pr(repo: &crate::pr::RepoId, number: u64) -> Self {
        LocationKey::Pr {
            repo: repo.slug().to_ascii_lowercase(),
            number,
        }
    }

    /// The checkout's root, for a view of one.
    pub fn dir(&self) -> Option<&Path> {
        match self {
            LocationKey::Repo { dir, .. } => Some(dir),
            LocationKey::Pr { .. } => None,
        }
    }

    pub fn view(&self) -> Option<&View> {
        match self {
            LocationKey::Repo { view, .. } => Some(view),
            LocationKey::Pr { .. } => None,
        }
    }
}

/// A location as the daemon keeps it and viewers copy it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationState {
    pub key: LocationKey,
    /// The checkout shown: the repository's, or the PR's own; empty for a
    /// PR nobody has fetched yet.
    pub dir: PathBuf,
    /// "#142 Fix the parser" for a pull request, else the directory's name.
    pub name: String,
    pub marks: Marks,
}

impl LocationState {
    /// A location nobody has shown yet, as the daemon first records it.
    pub fn fresh(key: LocationKey, dir: PathBuf, name: String) -> Self {
        Self {
            key,
            dir,
            name,
            marks: Marks::default(),
        }
    }
}

/// The root of the checkout `dir` is in, so every way of naming a
/// repository lands on the same location.
pub fn checkout_root(dir: &Path) -> PathBuf {
    gix::discover(dir)
        .ok()
        .and_then(|r| r.workdir().map(|w| w.to_path_buf()))
        .and_then(|w| std::fs::canonicalize(w).ok())
        .unwrap_or_else(|| dir.to_path_buf())
}

/// What a name an agent wrote stands for: its key, the checkout (empty for
/// a PR not yet fetched), and its short name.
pub fn resolve(named: &Named) -> Result<(LocationKey, PathBuf, String), String> {
    match named {
        Named::Repo { dir, view } => {
            if !dir.is_dir() {
                return Err(format!("{} is not a directory", dir.display()));
            }
            if gix::discover(dir).is_err() {
                return Err(format!("{} is not inside a git repository", dir.display()));
            }
            let root = checkout_root(dir);
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string());
            Ok((
                LocationKey::Repo {
                    dir: root.clone(),
                    view: view.clone(),
                },
                root,
                name,
            ))
        }
        Named::Pr(spec) => {
            let repo = spec
                .repo
                .as_ref()
                .ok_or_else(|| format!("#{} names no repository", spec.number))?;
            Ok((
                LocationKey::pr(repo, spec.number),
                PathBuf::new(),
                format!("#{}", spec.number),
            ))
        }
    }
}

/// Every location anyone has open, or has marks on.
#[derive(Default)]
pub struct Registry {
    pub locations: Vec<LocationState>,
}

impl Registry {
    pub fn get(&self, key: &LocationKey) -> Option<&LocationState> {
        self.locations.iter().find(|l| l.key == *key)
    }

    pub fn get_mut(&mut self, key: &LocationKey) -> Option<&mut LocationState> {
        self.locations.iter_mut().find(|l| l.key == *key)
    }

    /// The location for `key`, recorded now if it was not yet.
    pub fn ensure(&mut self, key: LocationKey, dir: PathBuf, name: String) -> &mut LocationState {
        let pos = match self.locations.iter().position(|l| l.key == key) {
            Some(i) => i,
            None => {
                self.locations.push(LocationState::fresh(key, dir, name));
                self.locations.len() - 1
            }
        };
        &mut self.locations[pos]
    }

    /// A viewer opened `state`: it is adopted when the location is new
    /// here. The location as the registry has it, and whether it was new.
    pub fn adopt(&mut self, state: LocationState) -> (&LocationState, bool) {
        let pos = self.locations.iter().position(|l| l.key == state.key);
        match pos {
            Some(i) => {
                // Names can change: a PR's title, a renamed directory; and
                // a PR's checkout is known once a viewer fetched it.
                if !state.dir.as_os_str().is_empty() {
                    self.locations[i].dir = state.dir;
                }
                self.locations[i].name = state.name;
                (&self.locations[i], false)
            }
            None => {
                self.locations.push(state);
                (self.locations.last().expect("just pushed"), true)
            }
        }
    }

    /// Locations nobody has open go once they carry no marks; the ones
    /// dropped.
    pub fn drop_unused(&mut self, open: &dyn Fn(&LocationKey) -> bool) -> Vec<LocationKey> {
        let gone: Vec<LocationKey> = self
            .locations
            .iter()
            .filter(|l| l.marks.marks.is_empty() && !open(&l.key))
            .map(|l| l.key.clone())
            .collect();
        self.locations.retain(|l| !gone.contains(&l.key));
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(dir: &str) -> LocationState {
        LocationState::fresh(
            LocationKey::Repo {
                dir: PathBuf::from(dir),
                view: View::Files,
            },
            PathBuf::from(dir),
            dir.rsplit('/').next().unwrap().to_string(),
        )
    }

    #[test]
    fn locations_are_named_by_what_they_are() {
        let k = LocationKey::Repo {
            dir: PathBuf::from("/work/repo"),
            view: View::Files,
        };
        assert_eq!(k.name(), "/work/repo");
        let k = LocationKey::Repo {
            dir: PathBuf::from("/work/repo"),
            view: View::parse("main...HEAD"),
        };
        assert_eq!(k.name(), "/work/repo#main...HEAD");
        let k = LocationKey::Pr {
            repo: "acme/widget".to_string(),
            number: 7,
        };
        assert_eq!(k.name(), "acme/widget#7");
    }

    #[test]
    fn a_name_resolves_to_the_checkout_root_or_refuses() {
        let dir = crate::testutil::branch_with_history("registry-resolve");
        let sub = dir.join("src");
        let named = crate::location::parse(&format!("{}#branch", sub.display())).unwrap();
        let (key, root, name) = resolve(&named).unwrap();
        assert_eq!(root, dir);
        assert_eq!(name, dir.file_name().unwrap().to_string_lossy());
        assert_eq!(key.name(), format!("{}#branch", dir.display()));
        let scratch = crate::testutil::fresh_dir("registry-plain");
        let named = crate::location::parse(scratch.to_str().unwrap()).unwrap();
        let err = resolve(&named).unwrap_err();
        assert!(err.contains("not inside a git repository"), "{err}");
        let named = crate::location::parse("/nowhere/at/all").unwrap();
        let err = resolve(&named).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        let named = crate::location::parse("Acme/Widget#7").unwrap();
        let (key, root, name) = resolve(&named).unwrap();
        assert_eq!(key.name(), "acme/widget#7");
        assert!(root.as_os_str().is_empty());
        assert_eq!(name, "#7");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(scratch);
    }

    #[test]
    fn unused_locations_go_unless_they_carry_marks() {
        let mut r = Registry::default();
        r.adopt(repo("/a"));
        let (_, new) = r.adopt(repo("/b"));
        assert!(new);
        let (_, new) = r.adopt(repo("/b"));
        assert!(!new);
        r.get_mut(&LocationKey::Repo {
            dir: PathBuf::from("/b"),
            view: View::Files,
        })
        .unwrap()
        .marks
        .apply(&crate::marks::Op::Mark {
            author: crate::marks::Author::User,
            path: "x".to_string(),
            start: 1,
            end: 1,
            old_side: false,
            with_deletions: false,
            text: "keep me".to_string(),
            lines: vec![],
        });
        let gone = r.drop_unused(&|_| false);
        assert_eq!(gone.len(), 1);
        assert_eq!(r.locations.len(), 1);
        assert_eq!(r.locations[0].name, "b");
    }

    #[test]
    fn ensure_records_a_location_once_and_adopt_fills_in_a_prs_checkout() {
        let mut r = Registry::default();
        let key = LocationKey::Pr {
            repo: "acme/widget".to_string(),
            number: 7,
        };
        r.ensure(key.clone(), PathBuf::new(), "#7".to_string());
        r.ensure(key.clone(), PathBuf::new(), "#7".to_string());
        assert_eq!(r.locations.len(), 1);
        let mut fetched = LocationState::fresh(key.clone(), "/cache/pr/7".into(), "#7 Fix".into());
        fetched.marks.made = 9;
        let (got, new) = r.adopt(fetched);
        assert!(!new);
        assert_eq!(got.dir, PathBuf::from("/cache/pr/7"));
        assert_eq!(got.name, "#7 Fix");
        assert_eq!(got.marks.made, 0, "the registry's marks count");
    }
}
