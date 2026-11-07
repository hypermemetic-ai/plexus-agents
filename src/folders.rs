//! Where a thread can start: the host's roots, the projects right under them, and
//! the folders recent threads used.
//!
//! Read-only, and names only: a folder is listed by what's in it (`.git`, a
//! manifest), and nothing is read. Hidden entries are skipped, and so is anything
//! that resolves outside every root, a symlink's target included. A host's roots are
//! `--root` (repeatable) or `AGENTS_ROOTS`, else its user's home.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Most folders listed under one root.
pub const MAX_PER_ROOT: usize = 200;
/// Most recent folders listed.
pub const MAX_RECENT: usize = 20;

/// What marks a directory as a project, besides `.git`.
const MARKERS: [&str; 12] = [
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "Makefile",
    "flake.nix",
    "CLAUDE.md",
    ".hg",
];

/// A folder a thread may start in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Folder {
    /// Absolute, as the host resolves it: what `start`'s `cwd` takes.
    pub path: String,
    /// The last component, for a person.
    pub name: String,
    /// A recent thread started here.
    pub recent: bool,
    /// It's a git checkout.
    pub git: bool,
}

/// The canonical forms of `roots`, dropping any that don't resolve to a directory.
pub fn resolve_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .filter(|r| r.is_dir())
        .collect();
    out.dedup();
    out
}

/// The roots when none are configured: the user's home.
pub fn default_roots() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .into_iter()
        .collect()
}

/// `path`, resolved, if it's a directory inside one of `roots` (which are resolved).
pub fn inside(roots: &[PathBuf], path: &Path) -> Option<PathBuf> {
    let p = path.canonicalize().ok()?;
    (p.is_dir() && roots.iter().any(|r| p.starts_with(r))).then_some(p)
}

fn hidden(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_none_or(|n| n.starts_with('.'))
}

fn folder(path: &Path, recent: bool) -> Folder {
    Folder {
        path: path.to_string_lossy().into_owned(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned()),
        recent,
        git: path.join(".git").exists(),
    }
}

fn is_project(p: &Path) -> bool {
    p.join(".git").exists() || MARKERS.iter().any(|m| p.join(m).exists())
}

/// The projects right under `dir`, by name, at most [`MAX_PER_ROOT`].
fn projects_under(roots: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| !hidden(p))
        .filter_map(|p| inside(roots, &p))
        .filter(|p| is_project(p))
        .collect();
    found.sort();
    found.dedup();
    found.truncate(MAX_PER_ROOT);
    found
}

/// Lists folders: each root, the projects right under it, and `recent` (newest
/// first; any outside the roots, gone, or hidden are dropped). Under `under`, if
/// given, instead of the roots: one level of a folder inside them, or an error.
pub fn list(
    roots: &[PathBuf],
    recent: &[String],
    under: Option<&Path>,
) -> Result<Vec<Folder>, String> {
    let recent: Vec<PathBuf> = recent
        .iter()
        .filter_map(|r| inside(roots, Path::new(r)))
        .fold(Vec::new(), |mut acc, p| {
            if !acc.contains(&p) && acc.len() < MAX_RECENT {
                acc.push(p);
            }
            acc
        });
    let is_recent = |p: &Path| recent.iter().any(|r| r == p);
    let mut out: Vec<Folder> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |p: &Path, out: &mut Vec<Folder>| {
        if seen.insert(p.to_path_buf()) {
            out.push(folder(p, is_recent(p)));
        }
    };
    match under {
        Some(dir) => {
            let dir = inside(roots, dir).ok_or_else(|| {
                format!("{} is not a folder inside this host's roots", dir.display())
            })?;
            for p in projects_under(roots, &dir) {
                push(&p, &mut out);
            }
        }
        None => {
            for p in &recent {
                push(p, &mut out);
            }
            for root in roots {
                push(root, &mut out);
                for p in projects_under(roots, root) {
                    push(&p, &mut out);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for d in [
            "repo/.git",
            "crate",
            "plain",
            ".hidden/.git",
            "deep/inner/.git",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("crate/Cargo.toml"), "").unwrap();
        std::fs::write(root.join("file.txt"), "").unwrap();
        (dir, root)
    }

    #[test]
    fn a_root_lists_its_projects_and_nothing_hidden_or_plain() {
        let (_d, root) = tree();
        let got = list(std::slice::from_ref(&root), &[], None).unwrap();
        let names: Vec<_> = got.iter().map(|f| f.name.as_str()).collect();
        let root_name = root.file_name().unwrap().to_str().unwrap();
        assert_eq!(names, vec![root_name, "crate", "repo"]);
        assert!(got.iter().find(|f| f.name == "repo").unwrap().git);
        assert!(!got.iter().find(|f| f.name == "crate").unwrap().git);
        assert!(got.iter().all(|f| !f.recent));
    }

    #[test]
    fn recent_folders_come_first_and_only_from_inside_the_roots() {
        let (_d, root) = tree();
        let other = tempfile::tempdir().unwrap();
        let recent = [
            root.join("deep/inner").to_string_lossy().into_owned(),
            other.path().to_string_lossy().into_owned(),
            root.join("repo").to_string_lossy().into_owned(),
            "/no/such/place".to_string(),
        ];
        let got = list(std::slice::from_ref(&root), &recent, None).unwrap();
        assert_eq!(got[0].name, "inner");
        assert!(got[0].recent && got[0].git);
        assert_eq!(got[1].name, "repo");
        assert!(got[1].recent);
        assert_eq!(got.iter().filter(|f| f.name == "repo").count(), 1);
        let outside = other.path().canonicalize().unwrap();
        assert!(got
            .iter()
            .all(|f| !Path::new(&f.path).starts_with(&outside)));
    }

    #[test]
    fn under_lists_one_level_and_refuses_outside_the_roots() {
        let (_d, root) = tree();
        let got = list(std::slice::from_ref(&root), &[], Some(&root.join("deep"))).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "inner");
        let other = tempfile::tempdir().unwrap();
        assert!(list(std::slice::from_ref(&root), &[], Some(other.path())).is_err());
        assert!(list(std::slice::from_ref(&root), &[], Some(&root.join(".."))).is_err());
    }

    #[test]
    fn a_symlink_out_of_the_roots_is_not_listed() {
        let (_d, root) = tree();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other.path().join(".git")).unwrap();
        std::os::unix::fs::symlink(other.path(), root.join("escape")).unwrap();
        let got = list(std::slice::from_ref(&root), &[], None).unwrap();
        assert!(got.iter().all(|f| f.name != "escape"));
    }
}
