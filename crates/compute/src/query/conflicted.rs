//! Commits `GitButler` holds conflicted, read as `GitButler` reads them.
//!
//! `GitButler` marks one with a `GitButler-Conflict` trailer, or with a
//! `gitbutler-conflicted` header in commits from before the trailer, and adds
//! the conflict's trees to the commit's root tree under fixed names. A marked
//! commit whose root tree holds none of those names is ordinary, so a marker
//! copied onto another commit changes nothing.

use gix_object::commit::MessageRef;

use crate::git::object::Commit;
use crate::git::oid::Oid;

use super::Child;

const HEADER: &[u8] = b"gitbutler-conflicted";
const TRAILER: &[u8] = b"GitButler-Conflict";
const AUTO_RESOLUTION: &str = ".auto-resolution";
const CONFLICT_PREFIX: &[u8] = b".conflict-";

/// A tree `GitButler` keeps in a conflicted commit, as its `TreeKind` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictTree {
    /// The conflict resolved in favor of ours, which `GitButler` shows as the
    /// commit's change.
    AutoResolution,
    Ours,
    Theirs,
    Base,
}

impl ConflictTree {
    pub(super) fn entry_name(self) -> &'static str {
        match self {
            Self::AutoResolution => AUTO_RESOLUTION,
            Self::Ours => ".conflict-side-0",
            Self::Theirs => ".conflict-side-1",
            Self::Base => ".conflict-base-0",
        }
    }
}

/// Whether `commit` carries `GitButler`'s conflict marker.
pub(super) fn is_marked(commit: &Commit<'_>) -> bool {
    let header = commit
        .header(HEADER)
        .and_then(|value| std::str::from_utf8(value).ok()?.parse::<u64>().ok())
        .is_some();
    header
        || MessageRef::from_bytes(commit.message)
            .body()
            .is_some_and(|body| {
                body.trailers()
                    .any(|t| t.token.eq_ignore_ascii_case(TRAILER))
            })
}

/// What a marked commit's root tree, of `entries`, holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Layout {
    /// No conflict trees: the commit is ordinary.
    Ordinary,
    /// The conflict, with the asked-for tree when the commit has it.
    Conflicted(Option<Oid>),
}

pub(super) fn layout(entries: &[Child], tree: ConflictTree) -> Layout {
    let conflicted = entries
        .iter()
        .any(|e| e.name == AUTO_RESOLUTION.as_bytes() || e.name.starts_with(CONFLICT_PREFIX));
    if !conflicted {
        return Layout::Ordinary;
    }
    Layout::Conflicted(
        entries
            .iter()
            .find(|e| e.is_tree && e.name == tree.entry_name().as_bytes())
            .map(|e| e.oid),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::object::parse_commit;
    use crate::git::oid::ObjectFormat;

    fn commit(headers: &str, message: &str) -> Vec<u8> {
        format!(
            "tree {tree}\nauthor A <a@a> 0 +0000\ncommitter A <a@a> 0 +0000\n{headers}\n{message}",
            tree = "1".repeat(40)
        )
        .into_bytes()
    }

    fn marked(data: &[u8]) -> bool {
        is_marked(&parse_commit(data).unwrap())
    }

    #[test]
    fn either_marker_marks_a_commit() {
        assert!(
            marked(&commit("gitbutler-conflicted 1\n", "subject\n")),
            "the header GitButler wrote first"
        );
        assert!(
            marked(&commit(
                "",
                "[conflict] subject\n\ngitbutler-conflict: yes\n"
            )),
            "the trailer GitButler writes now, in any case"
        );
        assert!(
            !marked(&commit("gitbutler-conflicted many\n", "subject\n")),
            "GitButler reads the header as a count"
        );
        assert!(
            !marked(&commit("", "GitButler-Conflict: yes\n")),
            "a subject is not a trailer"
        );
        assert!(
            !marked(&commit("change-id abc\n", "subject\n\nbody\n")),
            "an ordinary GitButler commit"
        );
    }

    fn entry(name: &str, is_tree: bool, byte: u8) -> Child {
        Child {
            oid: Oid::from_slice(ObjectFormat::Sha1, &[byte; 20]),
            is_tree,
            is_commit: false,
            name: name.into(),
        }
    }

    #[test]
    fn the_root_tree_decides_whether_a_marked_commit_is_conflicted() {
        let resolved = entry(".auto-resolution", true, 1);
        let ours = entry(".conflict-side-0", true, 2);
        let file = entry("a.txt", false, 3);
        assert_eq!(
            layout(std::slice::from_ref(&file), ConflictTree::AutoResolution),
            Layout::Ordinary,
            "a copied marker"
        );
        assert_eq!(
            layout(&[resolved.clone(), ours.clone(), file], ConflictTree::Ours),
            Layout::Conflicted(Some(ours.oid))
        );
        assert_eq!(
            layout(&[ours], ConflictTree::AutoResolution),
            Layout::Conflicted(None),
            "a conflict without the tree asked for"
        );
        assert_eq!(
            layout(
                &[entry(".auto-resolution", false, 4)],
                ConflictTree::AutoResolution
            ),
            Layout::Conflicted(None),
            "a blob is no tree to diff"
        );
    }
}
