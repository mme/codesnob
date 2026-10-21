//! The locations that agents and viewers share: one per checkout and one
//! per pull request, each with its marks. The daemon keeps the registry;
//! viewers hold copies of the locations they have open and show them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::marks::Marks;

/// What a location is, and how it is told from every other.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocationKey {
    /// A checkout, by its root.
    Repo { dir: PathBuf },
    /// A pull request, by its repository's `owner/name` and number.
    Pr { repo: String, number: u64 },
}

impl LocationKey {
    /// How agents name it: the checkout's path, or `owner/repo#142`.
    pub fn name(&self) -> String {
        match self {
            LocationKey::Repo { dir } => dir.display().to_string(),
            LocationKey::Pr { repo, number } => format!("{repo}#{number}"),
        }
    }

    pub fn pr(repo: &crate::pr::RepoId, number: u64) -> Self {
        LocationKey::Pr {
            repo: repo.slug().to_ascii_lowercase(),
            number,
        }
    }
}

/// How an agent names a location: a directory in the checkout, or a pull
/// request. Neither means the one on screen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
}

/// A location as the daemon keeps it and viewers copy it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationState {
    pub key: LocationKey,
    /// The checkout shown: the repository's, or the PR's own.
    pub dir: PathBuf,
    /// "#142 Fix the parser" for a pull request, else the directory's name.
    pub name: String,
    pub marks: Marks,
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

    /// A viewer opened `state`: it is adopted when the location is new
    /// here. The location as the registry has it, and whether it was new.
    pub fn adopt(&mut self, state: LocationState) -> (&LocationState, bool) {
        let pos = self.locations.iter().position(|l| l.key == state.key);
        match pos {
            Some(i) => {
                // Names can change: a PR's title, a renamed directory.
                self.locations[i].dir = state.dir;
                self.locations[i].name = state.name;
                (&self.locations[i], false)
            }
            None => {
                self.locations.push(state);
                (self.locations.last().expect("just pushed"), true)
            }
        }
    }

    /// The location an agent names, else the one on screen, else the one
    /// for the agent's own directory.
    pub fn resolve(
        &self,
        r: &LocationRef,
        on_screen: Option<&LocationKey>,
        cwd: Option<&Path>,
    ) -> Result<LocationKey, String> {
        if let Some(pr) = &r.pr {
            if r.repo.is_some() {
                return Err("name the location by repo or by pr, not both".to_string());
            }
            let spec = crate::pr::parse(pr).ok_or_else(|| {
                format!("{pr} is not a pull request; they look like owner/repo#142")
            })?;
            let want = spec.repo.as_ref().map(|r| r.slug().to_ascii_lowercase());
            let found: Vec<&LocationKey> = self
                .locations
                .iter()
                .filter_map(|l| match &l.key {
                    LocationKey::Pr { repo, number }
                        if *number == spec.number && want.as_ref().is_none_or(|w| w == repo) =>
                    {
                        Some(&l.key)
                    }
                    _ => None,
                })
                .collect();
            return match found.as_slice() {
                [one] => Ok((*one).clone()),
                [] => Err(format!("no pull request {pr} is open; show it first")),
                many => Err(format!(
                    "several pull requests are numbered {}; name one: {}",
                    spec.number,
                    many.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ")
                )),
            };
        }
        if let Some(dir) = &r.repo {
            let root = checkout_root(dir);
            return self
                .locations
                .iter()
                .find(|l| l.dir == root)
                .map(|l| l.key.clone())
                .ok_or_else(|| format!("nothing is open for {}; show it first", root.display()));
        }
        if let Some(key) = on_screen
            && self.get(key).is_some()
        {
            return Ok(key.clone());
        }
        if let Some(cwd) = cwd {
            let root = checkout_root(cwd);
            if let Some(l) = self.locations.iter().find(|l| l.dir == root) {
                return Ok(l.key.clone());
            }
        }
        Err("no location is on screen; name one with repo or pr".to_string())
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
        LocationState {
            key: LocationKey::Repo {
                dir: PathBuf::from(dir),
            },
            dir: PathBuf::from(dir),
            name: dir.rsplit('/').next().unwrap().to_string(),
            marks: Marks::default(),
        }
    }

    fn pr(slug: &str, number: u64, dir: &str) -> LocationState {
        LocationState {
            key: LocationKey::Pr {
                repo: slug.to_string(),
                number,
            },
            dir: PathBuf::from(dir),
            name: format!("#{number} Fix it"),
            marks: Marks::default(),
        }
    }

    #[test]
    fn locations_are_named_by_what_they_are() {
        let k = LocationKey::Repo {
            dir: PathBuf::from("/work/repo"),
        };
        assert_eq!(k.name(), "/work/repo");
        let k = LocationKey::Pr {
            repo: "acme/widget".to_string(),
            number: 7,
        };
        assert_eq!(k.name(), "acme/widget#7");
    }

    #[test]
    fn a_reference_finds_a_location_by_pr_or_directory() {
        let mut r = Registry::default();
        // Scratch directories that are not repositories resolve to themselves.
        let a = crate::testutil::fresh_dir("registry-a");
        let b = crate::testutil::fresh_dir("registry-b");
        r.adopt(repo(a.to_str().unwrap()));
        r.adopt(pr("acme/widget", 7, b.to_str().unwrap()));
        r.adopt(pr("other/widget", 7, "/c/pr/other/7"));
        let by = |rf: LocationRef| r.resolve(&rf, None, None);
        assert_eq!(
            by(LocationRef {
                repo: Some(a.clone()),
                pr: None
            })
            .unwrap(),
            LocationKey::Repo { dir: a.clone() }
        );
        assert_eq!(
            by(LocationRef {
                repo: Some(b.clone()),
                pr: None
            })
            .unwrap()
            .name(),
            "acme/widget#7",
            "a PR's checkout names the PR"
        );
        assert_eq!(
            by(LocationRef {
                repo: None,
                pr: Some("Acme/Widget#7".to_string())
            })
            .unwrap()
            .name(),
            "acme/widget#7"
        );
        let err = by(LocationRef {
            repo: None,
            pr: Some("#7".to_string()),
        })
        .unwrap_err();
        assert!(
            err.starts_with(
                "several pull requests are numbered 7; name one: acme/widget#7, other/widget#7"
            ),
            "{err}"
        );
        let err = by(LocationRef {
            repo: None,
            pr: Some("#9".to_string()),
        })
        .unwrap_err();
        assert_eq!(err, "no pull request #9 is open; show it first");
        let err = by(LocationRef {
            repo: Some(PathBuf::from("/nowhere")),
            pr: None,
        })
        .unwrap_err();
        assert_eq!(err, "nothing is open for /nowhere; show it first");
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
    }

    #[test]
    fn without_a_name_the_one_on_screen_comes_first_then_the_callers_own() {
        let mut r = Registry::default();
        let a = crate::testutil::fresh_dir("registry-screen-a");
        let b = crate::testutil::fresh_dir("registry-screen-b");
        r.adopt(repo(a.to_str().unwrap()));
        r.adopt(repo(b.to_str().unwrap()));
        let none = LocationRef::default();
        let on_screen = LocationKey::Repo { dir: b.clone() };
        assert_eq!(
            r.resolve(&none, Some(&on_screen), Some(&a)).unwrap(),
            on_screen
        );
        assert_eq!(
            r.resolve(&none, None, Some(&a)).unwrap(),
            LocationKey::Repo { dir: a.clone() }
        );
        let gone = LocationKey::Repo {
            dir: PathBuf::from("/gone"),
        };
        assert_eq!(
            r.resolve(&none, Some(&gone), Some(&a)).unwrap().name(),
            a.display().to_string(),
            "a location no longer open is skipped"
        );
        assert_eq!(
            r.resolve(&none, None, None).unwrap_err(),
            "no location is on screen; name one with repo or pr"
        );
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
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
}
