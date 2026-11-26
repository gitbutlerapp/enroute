//! What a commit changed, and what landed before it.

use enroute_api::api::v1alpha1::{ConflictedTree, DiffCommitRequest, FileChange, RenameDetection};

use crate::contract::{Client, hex, hexes, oid, repo_key};
use crate::support::{Scratch, Stack, git};

use super::support::{
    PATH, absent_oid, hook_backed_repo, pushed_repo, pushed_repo_serving, status_of,
};

/// Where a merge base is measured against, as the other tests here use it.
const TRUNK: &str = "refs/heads/main";

/// What a commit view is drawn from: the paths a commit touched, and the
/// blob on each side of every one of them.
///
/// Add, change and delete together, because the three are one walk and a
/// deletion is the half the push path never has to report.
#[tokio::test]
async fn a_commit_diffs_against_its_parent() {
    // The door is kept up, because this pushes more than once.
    let (client, id, _tmp, local, _servers) = pushed_repo_serving("first\n").await;

    // A second commit doing all three, in a subdirectory as well as at
    // the root: a path is only a path once something is above it.
    std::fs::create_dir_all(local.join("dir")).unwrap();
    std::fs::write(local.join("dir/kept.txt"), "kept\n").unwrap();
    std::fs::write(local.join("dir/gone.txt"), "gone\n").unwrap();
    git(&["add", "."], Some(&local)).await;
    git(&["commit", "-m", "second"], Some(&local)).await;
    git(&["push", "origin", "main"], Some(&local)).await;

    std::fs::write(local.join("a.txt"), "second\n").unwrap();
    std::fs::write(local.join("added.txt"), "new\n").unwrap();
    std::fs::remove_file(local.join("dir/gone.txt")).unwrap();
    git(&["add", "-A"], Some(&local)).await;
    git(&["commit", "-m", "third"], Some(&local)).await;
    git(&["push", "origin", "main"], Some(&local)).await;

    let head = git(&["rev-parse", "HEAD"], Some(&local)).await;
    let changes = client.diff_commit(&id, head.trim(), "").await.unwrap();

    let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    assert_eq!(paths, vec!["a.txt", "added.txt", "dir/gone.txt"]);
    // Which side each id is on is the whole answer: it says what the
    // change was without a word for it.
    assert!(changes[0].old_object_id.is_some() && changes[0].new_object_id.is_some());
    assert!(changes[1].old_object_id.is_none() && changes[1].new_object_id.is_some());
    assert!(changes[2].old_object_id.is_some() && changes[2].new_object_id.is_none());

    // Against git's own answer, so the two agree about what changed.
    let expected = git(
        &[
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            head.trim(),
        ],
        Some(&local),
    )
    .await;
    let mut named: Vec<&str> = expected.lines().filter(|line| !line.is_empty()).collect();
    named.sort_unstable();
    assert_eq!(paths, named);

    // The new side is the blob git has at that path, not merely some id.
    let blob = git(&["rev-parse", "HEAD:a.txt"], Some(&local)).await;
    assert_eq!(hex(changes[0].new_object_id.as_ref()), blob.trim());
}

/// A change as `git diff-tree --name-status` prints it, less the score.
fn name_status(change: &FileChange) -> String {
    let status = match (&change.old_object_id, &change.new_object_id) {
        _ if !change.old_path.is_empty() => format!("R\t{}", change.old_path),
        (None, _) => "A".into(),
        (_, None) => "D".into(),
        _ => "M".into(),
    };
    format!("{status}\t{}", change.path)
}

/// Renames pair as `git diff -M` pairs them, at each bar a caller can set.
///
/// One of each way a pair is found: an identical blob, an edited file that
/// keeps its base name, and an edited file that changes it.
#[tokio::test]
async fn renames_pair_as_git_pairs_them() {
    let (client, id, _tmp, local, _servers) = pushed_repo_serving("first\n").await;
    let body = |name: &str, line_five: &str| -> String {
        (0..20)
            .map(|i| match i {
                5 => format!("{name} {line_five}\n"),
                _ => format!("{name} line {i}\n"),
            })
            .collect()
    };
    let put = |path: &str, content: String| {
        let path = local.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    put("src/moved.txt", body("moved", "as it was"));
    put("docs/guide.md", body("guide", "as it was"));
    put("old-name.txt", body("named", "as it was"));
    put("gone.txt", "gone\n".into());
    git(&["add", "-A"], Some(&local)).await;
    git(&["commit", "-m", "before"], Some(&local)).await;

    for gone in ["src/moved.txt", "docs/guide.md", "old-name.txt", "gone.txt"] {
        std::fs::remove_file(local.join(gone)).unwrap();
    }
    put("lib/moved.txt", body("moved", "as it was"));
    put("manual/guide.md", body("guide", "edited"));
    put("new-name.txt", body("named", "edited"));
    put("fresh.txt", "fresh\n".into());
    git(&["add", "-A"], Some(&local)).await;
    git(&["commit", "-m", "after"], Some(&local)).await;
    git(&["push", "origin", "main"], Some(&local)).await;
    let head = git(&["rev-parse", "HEAD"], Some(&local)).await;

    for (renames, flag) in [
        (None, "--no-renames"),
        (Some(0), "-M"),
        (Some(100), "-M100%"),
    ] {
        let diff = client
            .diff(DiffCommitRequest {
                repo: Some(repo_key(&id)),
                commit_id: oid(head.trim()),
                renames: renames.map(|min_similarity_percent| RenameDetection {
                    min_similarity_percent,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !diff.truncated && !diff.renames_incomplete,
            "a diff this small is whole"
        );
        let mut ours: Vec<String> = diff.changes.iter().map(name_status).collect();
        ours.sort_unstable();

        let printed = git(
            &[
                "diff-tree",
                "-r",
                flag,
                "--name-status",
                "--no-commit-id",
                head.trim(),
            ],
            Some(&local),
        )
        .await;
        let mut theirs: Vec<String> = printed
            .lines()
            .filter_map(|line| {
                let (status, paths) = line.split_once('\t')?;
                Some(format!("{}\t{paths}", status.chars().next()?))
            })
            .collect();
        theirs.sort_unstable();
        assert_eq!(ours, theirs, "git diff-tree {flag} pairs the same paths");
    }
}

/// The commits [`a_conflicted_commit_diffs_as_the_tree_asked_for`] reads.
struct Conflicts {
    by_trailer: String,
    by_header: String,
    ordinary: String,
    copied_marker: String,
    lookalike: String,
    malformed: String,
}

/// Commit each of [`Conflicts`] onto `local`'s `main`, oldest first, and push
/// them.
async fn conflicted_history(local: &std::path::Path) -> Conflicts {
    let put = |path: &str, content: &str| {
        let path = local.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    let conflict_trees = [
        ".auto-resolution",
        ".conflict-side-0",
        ".conflict-side-1",
        ".conflict-base-0",
    ];
    let lay_out_conflict = |resolved: &str, ours: &str| {
        put("a.txt", resolved);
        for (tree, content) in conflict_trees
            .iter()
            .zip([resolved, ours, "theirs\n", "first\n"])
        {
            put(&format!("{tree}/a.txt"), content);
        }
        put(".conflict-files", "ourEntries = [\"a.txt\"]\n");
    };
    let commit = async |args: &[&str]| -> String {
        git(&["add", "-A"], Some(local)).await;
        git(&[&["commit"][..], args].concat(), Some(local)).await;
        git(&["rev-parse", "HEAD"], Some(local))
            .await
            .trim()
            .to_owned()
    };

    lay_out_conflict("resolved\n", "ours\n");
    let by_trailer = commit(&[
        "-m",
        "[conflict] by trailer",
        "-m",
        "GitButler-Conflict: yes",
    ])
    .await;

    lay_out_conflict("resolved again\n", "ours again\n");
    git(&["add", "-A"], Some(local)).await;
    let tree = git(&["write-tree"], Some(local)).await;
    let raw = local.join(".git/by-header");
    std::fs::write(
        &raw,
        format!(
            "tree {}\nparent {by_trailer}\n\
             author A <a@example.com> 1700000000 +0000\n\
             committer A <a@example.com> 1700000000 +0000\n\
             gitbutler-headers-version 2\ngitbutler-conflicted 1\n\nby header\n",
            tree.trim()
        ),
    )
    .unwrap();
    let written = git(
        &["hash-object", "-t", "commit", "-w", raw.to_str().unwrap()],
        Some(local),
    )
    .await;
    let by_header = written.trim().to_owned();
    git(&["update-ref", "HEAD", &by_header], Some(local)).await;

    for tree in conflict_trees {
        std::fs::remove_dir_all(local.join(tree)).unwrap();
    }
    std::fs::remove_file(local.join(".conflict-files")).unwrap();
    put("b.txt", "b\n");
    let ordinary = commit(&["-m", "ordinary"]).await;

    put("b.txt", "b, edited\n");
    let copied_marker = commit(&["-m", "copied", "-m", "GitButler-Conflict: yes"]).await;

    put(".auto-resolution/a.txt", "a lookalike\n");
    let lookalike = commit(&["-m", "a lookalike"]).await;

    std::fs::remove_dir_all(local.join(".auto-resolution")).unwrap();
    put(".conflict-side-0/a.txt", "ours\n");
    let malformed = commit(&["-m", "malformed", "-m", "GitButler-Conflict: yes"]).await;
    git(&["push", "origin", "main"], Some(local)).await;

    Conflicts {
        by_trailer,
        by_header,
        ordinary,
        copied_marker,
        lookalike,
        malformed,
    }
}

/// A commit `GitButler` holds conflicted diffs as the tree of its conflict
/// asked for, on each side, and every other commit as its root tree.
///
/// Marked both ways `GitButler` has marked one: the trailer it writes now, and
/// the header it wrote before.
#[tokio::test]
async fn a_conflicted_commit_diffs_as_the_tree_asked_for() {
    let (client, id, _tmp, local, _servers) = pushed_repo_serving("first\n").await;
    let Conflicts {
        by_trailer,
        by_header,
        ordinary,
        copied_marker,
        lookalike,
        malformed,
    } = conflicted_history(&local).await;

    let diff = async |commit: &str, tree: ConflictedTree| {
        client
            .diff(DiffCommitRequest {
                repo: Some(repo_key(&id)),
                commit_id: oid(commit),
                conflicted_tree: tree.into(),
                ..Default::default()
            })
            .await
            .map(|diff| diff.changes)
    };
    let blob = async |spec: String| {
        git(&["rev-parse", &spec], Some(&local))
            .await
            .trim()
            .to_owned()
    };
    let sides = |changes: &[FileChange]| -> Vec<(String, String, String)> {
        changes
            .iter()
            .map(|c| {
                let (old, new) = (c.old_object_id.as_ref(), c.new_object_id.as_ref());
                (c.path.clone(), hex(old), hex(new))
            })
            .collect()
    };
    let as_git_diffs = async |commit: &str| -> Vec<String> {
        let printed = git(
            &["diff-tree", "-r", "--name-only", "--no-commit-id", commit],
            Some(&local),
        )
        .await;
        let mut paths: Vec<String> = printed.lines().map(str::to_owned).collect();
        paths.sort_unstable();
        paths
    };
    let auto = ConflictedTree::AutoResolution;

    assert_eq!(
        sides(&diff(&by_trailer, auto).await.unwrap()),
        [(
            "a.txt".into(),
            blob(format!("{by_trailer}~1:a.txt")).await,
            blob(format!("{by_trailer}:.auto-resolution/a.txt")).await,
        )],
        "the resolution, against an ordinary parent"
    );
    for (tree, name) in [
        (auto, ".auto-resolution"),
        (ConflictedTree::Ours, ".conflict-side-0"),
    ] {
        assert_eq!(
            sides(&diff(&by_header, tree).await.unwrap()),
            [(
                "a.txt".into(),
                blob(format!("{by_trailer}:{name}/a.txt")).await,
                blob(format!("{by_header}:{name}/a.txt")).await,
            )],
            "{name} on both sides, each marked its own way"
        );
    }
    let paths =
        |changes: Vec<FileChange>| -> Vec<String> { changes.into_iter().map(|c| c.path).collect() };
    assert_eq!(
        paths(diff(&ordinary, auto).await.unwrap()),
        ["b.txt"],
        "a.txt is what the conflicted parent resolved to"
    );
    for (commit, why) in [
        (&copied_marker, "a marker with no conflict trees"),
        (&lookalike, "conflict trees with no marker"),
    ] {
        assert_eq!(
            paths(diff(commit, auto).await.unwrap()),
            as_git_diffs(commit).await,
            "{why} is an ordinary commit"
        );
    }
    assert_eq!(
        paths(diff(&by_header, ConflictedTree::Unspecified).await.unwrap()),
        as_git_diffs(&by_header).await,
        "no tree asked for diffs the root trees, as git does"
    );

    let error = diff(&malformed, auto).await.expect_err("no tree to diff");
    assert_eq!(
        status_of(&error).code(),
        tonic::Code::InvalidArgument,
        "a conflict without the tree asked for: {error:?}"
    );
}

/// A bar above identical, and a tree the contract does not name, are refused
/// rather than read as something nearby.
#[tokio::test]
async fn diff_options_with_no_meaning_are_refused() {
    let (client, id, _tmp, local) = pushed_repo("first\n").await;
    let head = git(&["rev-parse", "HEAD"], Some(&local)).await;
    let request = || DiffCommitRequest {
        repo: Some(repo_key(&id)),
        commit_id: oid(head.trim()),
        ..Default::default()
    };
    for (what, refused) in [
        (
            "more alike than identical",
            DiffCommitRequest {
                renames: Some(RenameDetection {
                    min_similarity_percent: 101,
                }),
                ..request()
            },
        ),
        (
            "a conflicted tree the contract does not name",
            DiffCommitRequest {
                conflicted_tree: 99,
                ..request()
            },
        ),
    ] {
        let error = client.diff(refused).await.expect_err(what);
        assert_eq!(
            status_of(&error).code(),
            tonic::Code::InvalidArgument,
            "{what}: {error:?}"
        );
    }
}

/// A commit with no parent has nothing to diff against, so everything in
/// it is an add — git's own answer for a root commit.
#[tokio::test]
async fn a_root_commit_adds_every_path_in_it() {
    let (client, id, _tmp, local) = pushed_repo("first\n").await;
    let head = git(&["rev-parse", "HEAD"], Some(&local)).await;

    let changes = client.diff_commit(&id, head.trim(), "").await.unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "a.txt");
    assert!(
        changes[0].old_object_id.is_none(),
        "a root commit has no side to have changed from"
    );
}

/// A diff is of a commit, and a tree is refused.
///
/// A tree has no parent, so asking what it changed is a question with no
/// answer rather than one whose answer is empty.
#[tokio::test]
async fn diffing_a_tree_is_refused() {
    let (client, id, _tmp, local) = pushed_repo("first\n").await;
    let tree = git(&["rev-parse", "HEAD^{tree}"], Some(&local)).await;

    let error = client
        .diff_commit(&id, tree.trim(), "")
        .await
        .expect_err("a tree is not a commit");
    assert_eq!(
        status_of(&error).code(),
        tonic::Code::InvalidArgument,
        "wrong status for diffing a tree: {error:?}"
    );
}

/// A repository whose `main` is `count` commits deep, newest last.
///
/// Returns the client, the repository id, the checkout, and every oid in
/// the order git made them.
async fn pushed_history(count: usize) -> (Client, String, tempfile::TempDir, Vec<String>) {
    let stack = Stack::start().await;
    let client = stack.client().await;
    let repo = client.create_repository("").await.unwrap();
    let id = repo.repo.clone().unwrap().key;
    stack.repos.name(PATH, &id);

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("work");
    std::fs::create_dir_all(&local).unwrap();
    let url = stack.url(PATH);
    git(&["init", "-b", "main"], Some(&local)).await;
    git(&["remote", "add", "origin", &url], Some(&local)).await;
    for n in 0..count {
        std::fs::write(local.join("a.txt"), format!("body {n}\n")).unwrap();
        git(&["add", "."], Some(&local)).await;
        git(&["commit", "-m", &format!("commit {n}")], Some(&local)).await;
    }
    git(&["push", "origin", "main"], Some(&local)).await;

    let listed = git(&["rev-list", "--reverse", "HEAD"], Some(&local)).await;
    let oids = listed.lines().map(str::to_owned).collect();
    (client.holding(stack), id, tmp, oids)
}

/// The history reads back newest first, and reads back what git says it
/// is — the ordering is the whole point of the call.
#[tokio::test]
async fn a_history_lists_newest_first() {
    let (client, id, _tmp, oids) = pushed_history(5).await;
    let head = oids.last().unwrap();

    let (commits, next) = client.list_commits(&id, head, 0, "").await.unwrap();
    let listed: Vec<String> = commits.iter().map(|c| hex(c.commit_id.as_ref())).collect();
    let expected: Vec<String> = oids.iter().rev().cloned().collect();
    assert_eq!(listed, expected, "a history came back out of order");
    assert!(
        next.is_empty(),
        "a whole history still named a page after it: {next}"
    );

    let newest = commits.first().expect("a page of five has a first commit");
    assert_eq!(newest.summary, "commit 4");
    assert_eq!(
        hexes(&newest.parent_commit_ids).first().copied(),
        Some(oids[3].as_str()),
        "a commit named the wrong parent"
    );
}

/// Paging resumes exactly where the page before it stopped, with nothing
/// repeated and nothing skipped across the seam.
#[tokio::test]
async fn paging_a_history_neither_repeats_nor_skips() {
    let (client, id, _tmp, oids) = pushed_history(5).await;
    let head = oids.last().unwrap();

    let (first, next) = client.list_commits(&id, head, 2, "").await.unwrap();
    assert_eq!(first.len(), 2);
    assert!(!next.is_empty(), "a partial history named no page after it");

    let (second, last) = client.list_commits(&id, head, 3, &next).await.unwrap();
    assert_eq!(second.len(), 3);
    assert!(
        last.is_empty(),
        "the last page named a page after it: {last}"
    );

    let walked: Vec<String> = first
        .iter()
        .chain(second.iter())
        .map(|c| hex(c.commit_id.as_ref()))
        .collect();
    let expected: Vec<String> = oids.iter().rev().cloned().collect();
    assert_eq!(walked, expected, "paging did not reassemble the history");

    // And an unset limit from the same token takes whatever is left, which is
    // what a caller reading the tail of a history asks for.
    let (rest, none) = client.list_commits(&id, head, 0, &next).await.unwrap();
    assert!(
        none.is_empty(),
        "the last page named a page after it: {none}"
    );
    assert_eq!(
        rest.iter()
            .map(|c| hex(c.commit_id.as_ref()))
            .collect::<Vec<_>>(),
        expected[2..],
        "an unset limit did not take the rest of the history"
    );
}

/// A history starts at a commit, and a tree is a well-formed question with
/// the answer no — not an internal error.
#[tokio::test]
async fn a_history_from_a_tree_is_refused() {
    let (client, id, _tmp, local) = pushed_repo("body\n").await;
    let tree = git(&["rev-parse", "HEAD^{tree}"], Some(&local)).await;

    let error = client
        .list_commits(&id, tree.trim(), 0, "")
        .await
        .expect_err("a tree has no history");
    assert_eq!(
        status_of(&error).code(),
        tonic::Code::InvalidArgument,
        "wrong status for a history from a tree: {error:?}"
    );
}

/// And one nobody pushed is still a `NotFound`, as reading it would be.
#[tokio::test]
async fn a_history_from_an_unknown_object_is_not_found() {
    let (client, id, _tmp, _local) = pushed_repo("body\n").await;
    let absent = absent_oid();

    let error = client
        .list_commits(&id, &absent, 0, "")
        .await
        .expect_err("an object that was never pushed has no history");
    assert_eq!(
        status_of(&error).code(),
        tonic::Code::NotFound,
        "wrong status for a history from a missing object: {error:?}"
    );
}

/// A merge is read against its first parent, in both of the calls a commit
/// view is drawn from: what it changed, and what landed before it.
///
/// One repository for both, since a merge is what each is about and building
/// one costs a checkout, a branch, and a push.
#[tokio::test]
async fn a_merge_reads_against_its_first_parent() {
    let (client, id, _tmp, local, _servers) = pushed_repo_serving("root\n").await;
    git(&["checkout", "-b", "side"], Some(&local)).await;
    std::fs::write(local.join("side.txt"), "side\n").unwrap();
    git(&["add", "."], Some(&local)).await;
    git(&["commit", "-m", "on the side"], Some(&local)).await;

    git(&["checkout", "main"], Some(&local)).await;
    std::fs::write(local.join("trunk.txt"), "trunk\n").unwrap();
    git(&["add", "."], Some(&local)).await;
    git(&["commit", "-m", "on trunk"], Some(&local)).await;
    git(
        &["merge", "--no-ff", "-m", "merge the side in", "side"],
        Some(&local),
    )
    .await;
    git(&["push", "origin", "main"], Some(&local)).await;
    let merge = git(&["rev-parse", "HEAD"], Some(&local)).await;
    let first_parents = git(&["rev-list", "--first-parent", "HEAD"], Some(&local)).await;

    let changes = client.diff_commit(&id, merge.trim(), "").await.unwrap();
    let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["side.txt"],
        "a merge's diff is what the side brought, not what trunk already had"
    );

    let (commits, _next) = client.list_commits(&id, merge.trim(), 0, "").await.unwrap();
    let listed: Vec<String> = commits.iter().map(|c| hex(c.commit_id.as_ref())).collect();
    assert_eq!(
        listed,
        first_parents.lines().map(str::to_owned).collect::<Vec<_>>(),
        "the history is not git's own first-parent walk"
    );
}

/// A commit carries what a listing is drawn from: its summary, its body, and
/// who wrote it — so a caller draws one without reading the object.
#[tokio::test]
async fn a_commit_carries_its_summary_and_author() {
    let (client, id, _tmp, local, _servers) = pushed_repo_serving("body\n").await;
    std::fs::write(local.join("a.txt"), "again\n").unwrap();
    git(&["add", "."], Some(&local)).await;
    git(
        &["commit", "-m", "a summary", "-m", "and a body of its own"],
        Some(&local),
    )
    .await;
    git(&["push", "origin", "main"], Some(&local)).await;
    let written = git(
        &["log", "-1", "--format=%H%n%T%n%P%n%an <%ae>"],
        Some(&local),
    )
    .await;
    let [head, tree, parent, author] = <[&str; 4]>::try_from(written.lines().collect::<Vec<_>>())
        .expect("four lines, one per format directive");

    let (commits, _next) = client.list_commits(&id, head, 0, "").await.unwrap();
    let commit = commits.first().expect("a history has its own tip in it");
    assert_eq!(hex(commit.commit_id.as_ref()), head);
    assert_eq!(commit.summary, "a summary");
    // Trailing newline and all: the body is the commit's own bytes after the
    // summary, not a trimmed rendering of them.
    assert_eq!(commit.body, "and a body of its own\n");
    assert_eq!(hex(commit.tree_id.as_ref()), tree);
    assert_eq!(hexes(&commit.parent_commit_ids), vec![parent],);
    let identity = commit.author.as_ref().expect("a commit has an author");
    assert_eq!(
        format!("{} <{}>", identity.name, identity.email),
        author,
        "the commit does not report the author git wrote"
    );
}

/// Where two branches last agreed, which only the commit graph knows.
///
/// An application reads what a branch changed against this commit, not against
/// the other branch's tip — everything landed there since would read as undone.
#[tokio::test]
async fn a_merge_base_is_the_commit_two_branches_forked_at() {
    let (stack, client, repo) = hook_backed_repo(TRUNK).await;
    let scratch = Scratch::new().await;

    // One root, then a commit on each side of it.
    let root = scratch.commit(b"root\n", None, "root").await;
    let mine = scratch.commit(b"mine\n", Some(&root), "mine").await;
    let yours = scratch.commit(b"yours\n", Some(&root), "yours").await;
    stack.seed(&repo, TRUNK, &scratch, &mine).await;
    stack
        .seed(&repo, "refs/heads/yours", &scratch, &yours)
        .await;

    let found = client
        .find_merge_bases(&repo, &mine.oid, &yours.oid)
        .await
        .unwrap();
    assert_eq!(
        hexes(&found.base_commit_ids),
        vec![root.oid.as_str()],
        "the two forked at the root"
    );
    assert!(
        !found.exhausted,
        "a walk this small must not report itself cut short"
    );
    // Symmetric, and a commit's base with itself is itself.
    assert_eq!(
        hexes(
            &client
                .find_merge_bases(&repo, &yours.oid, &mine.oid)
                .await
                .unwrap()
                .base_commit_ids
        ),
        vec![root.oid.as_str()],
    );
    assert_eq!(
        hexes(
            &client
                .find_merge_bases(&repo, &mine.oid, &mine.oid)
                .await
                .unwrap()
                .base_commit_ids
        ),
        vec![mine.oid.as_str()],
    );
}

/// A branch against a trunk, which is the pair a merge request is drawn from.
///
/// Every commit here writes the same path, so which base was used shows in
/// the blob on the old side rather than in the paths.
#[tokio::test]
async fn a_diff_reads_against_the_base_it_is_given() {
    let (stack, client, repo) = hook_backed_repo(TRUNK).await;
    let scratch = Scratch::new().await;

    // Trunk moves on after the branch is cut, so the two have different ideas
    // of what the file says.
    let root = scratch.commit(b"root\n", None, "root").await;
    let landed = scratch
        .commit(b"landed\n", Some(&root), "landed on trunk")
        .await;
    let mine = scratch.commit(b"mine\n", Some(&root), "mine").await;
    stack.seed(&repo, TRUNK, &scratch, &landed).await;
    stack.seed(&repo, "refs/heads/mine", &scratch, &mine).await;

    let older = |changes: Vec<FileChange>| {
        assert_eq!(changes.len(), 1, "one path, written by every commit here");
        hex(changes[0].old_object_id.as_ref())
    };

    // The base the two forked at, which `FindMergeBases` names and the test
    // above already holds it to.
    let against_base = client
        .diff_commit(&repo, &mine.oid, &root.oid)
        .await
        .unwrap();
    assert_eq!(
        older(against_base),
        root.blob.clone(),
        "what the branch changed, read from where it forked"
    );

    // Against trunk's tip the old side is trunk's own work, so the diff
    // claims the branch undid it.
    let against_tip = client
        .diff_commit(&repo, &mine.oid, &landed.oid)
        .await
        .unwrap();
    assert_eq!(older(against_tip), landed.blob.clone());

    // Empty still means the first parent, which for this commit is the root.
    let against_parent = client.diff_commit(&repo, &mine.oid, "").await.unwrap();
    assert_eq!(older(against_parent), root.blob.clone());
}

/// A base that is not a commit is refused, as a diff of a tree is.
#[tokio::test]
async fn a_diff_against_a_base_that_is_not_a_commit_is_refused() {
    let (stack, client, repo) = hook_backed_repo(TRUNK).await;
    let scratch = Scratch::new().await;
    let only = scratch.commit(b"only\n", None, "only").await;
    stack.seed(&repo, TRUNK, &scratch, &only).await;

    let refused = client
        .diff_commit(&repo, &only.oid, &only.tree)
        .await
        .expect_err("a tree is not a base");
    assert_eq!(status_of(&refused).code(), tonic::Code::InvalidArgument);
}
