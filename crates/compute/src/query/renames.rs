//! Renames among a diff's deletes and adds, paired as `git diff -M` pairs them.
//!
//! Identical blobs pair first. Then a delete and an add whose base names are
//! each unique on their side pair at a bar halfway to identical, and then the
//! most alike of the rest pair, best first. How alike two blobs are is git's
//! `diffcore-delta` estimate, so a pair scores here what it scores in git.
//!
//! Both limits are checked on sizes alone, before any blob is read, so a diff
//! over them reads nothing and reports the identical pairs.

use std::sync::Arc;

use anyhow::{Context, Result};
use foldhash::{HashMap, HashMapExt, HashSet};

use crate::git::oid::{Kind, Oid};
use crate::repo::view::{DenseId, Snapshot};

use super::FileChange;

/// Git's scale for how alike two blobs are, where identical scores this.
const MAX_SCORE: u64 = 60_000;

/// Deletes times adds compared by content, at most: git's default
/// `diff.renameLimit`, squared, as git applies it.
const CANDIDATE_LIMIT: usize = 1_000 * 1_000;

/// Bytes compared by content, at most, a blob counted once for each pair it
/// is in, which bounds both what is read and the work of comparing it.
const COMPARED_BYTE_LIMIT: u64 = 256 * 1024 * 1024;

/// Git's `diffcore-delta` hashes a chunk modulo this prime.
const SPAN_HASH_BASE: u32 = 107_927;

/// The longest chunk, in bytes, when no newline ends it sooner.
const SPAN_MAX_LEN: u64 = 64;

/// Git reads a blob as binary when its first this many bytes hold a NUL.
const BINARY_SNIFF_LEN: usize = 8_000;

/// How alike a delete and an add must be to pair, on git's scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Similarity(u64);

impl Similarity {
    /// `-M<percent>%`, where zero is git's default of 50; `None` above 100.
    pub(crate) fn from_percent(percent: u32) -> Option<Self> {
        match percent {
            0 => Some(Self(MAX_SCORE / 2)),
            1..=100 => Some(Self(u64::from(percent) * MAX_SCORE / 100)),
            _ => None,
        }
    }

    fn identical_only(self) -> bool {
        self.0 >= MAX_SCORE
    }

    fn halfway_to_identical(self) -> Self {
        Self(self.0 + (MAX_SCORE - self.0) / 2)
    }

    /// Whether blobs of these sizes could score this, which git checks before
    /// it reads either.
    fn sizes_allow(self, a: u64, b: u64) -> bool {
        let (small, large) = (u128::from(a.min(b)), u128::from(a.max(b)));
        large * u128::from(MAX_SCORE - self.0) >= (large - small) * u128::from(MAX_SCORE)
    }
}

/// A diff's changes with renames paired.
#[derive(Debug)]
pub(super) struct Renamed {
    pub changes: Vec<FileChange>,
    /// A limit stopped the comparison of contents.
    pub incomplete: bool,
}

/// Pair the deletes and adds among `changes` that are at least `min` alike.
///
/// # Errors
///
/// A blob the diff names is not stored, or a read failed.
pub(super) async fn pair(
    snap: &Arc<Snapshot>,
    changes: Vec<FileChange>,
    min: Similarity,
) -> Result<Renamed> {
    let mut pairing = Pairing::new(changes);
    pairing.pair_identical();
    if min.identical_only() {
        return Ok(pairing.finish(false));
    }
    let mut contents = Contents::new(snap, &pairing)?;

    let bar = min.halfway_to_identical();
    let same_name = pairing.unique_name_candidates();
    let Some(scored) = contents.score(same_name, bar).await? else {
        return Ok(pairing.finish(true));
    };
    pairing.accept(scored, bar);

    let Some(every) = pairing.every_candidate() else {
        return Ok(pairing.finish(true));
    };
    let Some(scored) = contents.score(every, min).await? else {
        return Ok(pairing.finish(true));
    };
    pairing.accept(scored, min);
    Ok(pairing.finish(false))
}

/// A deleted or added path, and the index of the path it pairs with.
#[derive(Debug)]
struct Side {
    path: Vec<u8>,
    oid: Oid,
    partner: Option<usize>,
}

impl Side {
    fn name(&self) -> &[u8] {
        self.path
            .rsplit(|b| *b == b'/')
            .next()
            .unwrap_or(&self.path)
    }
}

/// A delete and an add that may be a rename.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    deleted: usize,
    added: usize,
    same_name: bool,
}

/// A candidate and its score on git's scale.
#[derive(Debug, Clone, Copy)]
struct Scored {
    candidate: Candidate,
    score: u64,
}

/// The changes of a diff, split into what can pair and what cannot.
#[derive(Debug)]
struct Pairing {
    modified: Vec<FileChange>,
    deleted: Vec<Side>,
    added: Vec<Side>,
}

impl Pairing {
    fn new(changes: Vec<FileChange>) -> Self {
        let mut pairing = Pairing {
            modified: Vec::new(),
            deleted: Vec::new(),
            added: Vec::new(),
        };
        for change in changes {
            match (change.old, change.new) {
                (Some(oid), None) => pairing.deleted.push(Side {
                    path: change.path,
                    oid,
                    partner: None,
                }),
                (None, Some(oid)) => pairing.added.push(Side {
                    path: change.path,
                    oid,
                    partner: None,
                }),
                _ => pairing.modified.push(change),
            }
        }
        pairing.deleted.sort_by(|a, b| a.path.cmp(&b.path));
        pairing.added.sort_by(|a, b| a.path.cmp(&b.path));
        pairing
    }

    fn link(&mut self, Candidate { deleted, added, .. }: Candidate) {
        if let (Some(d), Some(a)) = (self.deleted.get_mut(deleted), self.added.get_mut(added))
            && d.partner.is_none()
            && a.partner.is_none()
        {
            d.partner = Some(added);
            a.partner = Some(deleted);
        }
    }

    /// Each add takes an unpaired delete of the same blob, one with its base
    /// name if there is one.
    fn pair_identical(&mut self) {
        let mut by_blob: HashMap<Oid, Vec<usize>> = HashMap::new();
        for (i, d) in self.deleted.iter().enumerate() {
            by_blob.entry(d.oid).or_default().push(i);
        }
        for added in 0..self.added.len() {
            if let Some(deleted) = self.identical_delete(added, &by_blob) {
                self.link(Candidate {
                    deleted,
                    added,
                    same_name: false,
                });
            }
        }
    }

    fn identical_delete(&self, added: usize, by_blob: &HashMap<Oid, Vec<usize>>) -> Option<usize> {
        let add = self.added.get(added)?;
        let free: Vec<(usize, &Side)> = by_blob
            .get(&add.oid)?
            .iter()
            .filter_map(|&i| Some((i, self.deleted.get(i)?)))
            .filter(|(_, d)| d.partner.is_none())
            .collect();
        free.iter()
            .find(|(_, d)| d.name() == add.name())
            .or(free.first())
            .map(|(i, _)| *i)
    }

    fn unpaired(sides: &[Side]) -> impl Iterator<Item = (usize, &Side)> {
        sides
            .iter()
            .enumerate()
            .filter(|(_, side)| side.partner.is_none())
    }

    /// The unpaired delete and add for each base name that each side has
    /// exactly once.
    fn unique_name_candidates(&self) -> Vec<Candidate> {
        fn unique(sides: &[Side]) -> HashMap<&[u8], Option<usize>> {
            let mut names: HashMap<&[u8], Option<usize>> = HashMap::new();
            for (i, side) in Pairing::unpaired(sides) {
                names
                    .entry(side.name())
                    .and_modify(|seen| *seen = None)
                    .or_insert(Some(i));
            }
            names
        }
        let added = unique(&self.added);
        let mut candidates: Vec<Candidate> = unique(&self.deleted)
            .into_iter()
            .filter_map(|(name, deleted)| {
                Some(Candidate {
                    deleted: deleted?,
                    added: (*added.get(name)?)?,
                    same_name: true,
                })
            })
            .collect();
        candidates.sort_by_key(|c| c.deleted);
        candidates
    }

    /// Every unpaired delete against every unpaired add; `None` past
    /// [`CANDIDATE_LIMIT`].
    fn every_candidate(&self) -> Option<Vec<Candidate>> {
        let deleted: Vec<(usize, &Side)> = Self::unpaired(&self.deleted).collect();
        let added: Vec<(usize, &Side)> = Self::unpaired(&self.added).collect();
        if deleted.len().saturating_mul(added.len()) > CANDIDATE_LIMIT {
            return None;
        }
        Some(
            added
                .iter()
                .flat_map(|&(a, add)| {
                    deleted.iter().map(move |&(d, del)| Candidate {
                        deleted: d,
                        added: a,
                        same_name: del.name() == add.name(),
                    })
                })
                .collect(),
        )
    }

    /// Pair the best of `scored` that reach `bar` first, ties going to a
    /// shared base name, then to path order.
    fn accept(&mut self, mut scored: Vec<Scored>, bar: Similarity) {
        scored.retain(|s| s.score >= bar.0);
        scored.sort_by(|x, y| {
            y.score
                .cmp(&x.score)
                .then(y.candidate.same_name.cmp(&x.candidate.same_name))
                .then(x.candidate.added.cmp(&y.candidate.added))
                .then(x.candidate.deleted.cmp(&y.candidate.deleted))
        });
        for s in scored {
            self.link(s.candidate);
        }
    }

    fn finish(self, incomplete: bool) -> Renamed {
        let mut changes = self.modified;
        for add in &self.added {
            let from = add.partner.and_then(|d| self.deleted.get(d));
            changes.push(FileChange {
                path: add.path.clone(),
                old: from.map(|d| d.oid),
                new: Some(add.oid),
                old_path: from.map(|d| d.path.clone()),
            });
        }
        changes.extend(
            self.deleted
                .into_iter()
                .filter(|d| d.partner.is_none())
                .map(|d| FileChange {
                    path: d.path,
                    old: Some(d.oid),
                    new: None,
                    old_path: None,
                }),
        );
        Renamed {
            changes,
            incomplete,
        }
    }
}

/// A blob's place in the snapshot and its size.
#[derive(Debug, Clone, Copy)]
struct Blob {
    d: DenseId,
    size: u64,
}

/// What comparing contents has read, and what it may still read.
struct Contents<'a> {
    snap: &'a Arc<Snapshot>,
    deleted: Vec<Oid>,
    added: Vec<Oid>,
    blobs: HashMap<Oid, Blob>,
    spans: HashMap<Oid, Spans>,
    budget: u64,
}

impl<'a> Contents<'a> {
    fn new(snap: &'a Arc<Snapshot>, pairing: &Pairing) -> Result<Self> {
        let deleted: Vec<Oid> = pairing.deleted.iter().map(|s| s.oid).collect();
        let added: Vec<Oid> = pairing.added.iter().map(|s| s.oid).collect();
        let mut blobs = HashMap::new();
        for side in Pairing::unpaired(&pairing.deleted).chain(Pairing::unpaired(&pairing.added)) {
            let oid = side.1.oid;
            let d = snap
                .lookup_kind(&oid, Kind::Blob)
                .with_context(|| format!("blob {oid} is missing"))?;
            blobs.insert(
                oid,
                Blob {
                    d,
                    size: snap.size(d),
                },
            );
        }
        Ok(Contents {
            snap,
            deleted,
            added,
            blobs,
            spans: HashMap::new(),
            budget: COMPARED_BYTE_LIMIT,
        })
    }

    fn blobs_of(&self, c: Candidate) -> Option<(Oid, Blob, Oid, Blob)> {
        let old = *self.deleted.get(c.deleted)?;
        let new = *self.added.get(c.added)?;
        Some((old, *self.blobs.get(&old)?, new, *self.blobs.get(&new)?))
    }

    /// The score of each of `candidates` whose sizes allow `bar`; `None`
    /// when comparing them would pass [`COMPARED_BYTE_LIMIT`].
    async fn score(
        &mut self,
        candidates: Vec<Candidate>,
        bar: Similarity,
    ) -> Result<Option<Vec<Scored>>> {
        let mut compared = 0u64;
        let mut work = Vec::new();
        for c in candidates {
            let Some((old, old_blob, new, new_blob)) = self.blobs_of(c) else {
                continue;
            };
            if bar.sizes_allow(old_blob.size, new_blob.size) {
                compared = compared.saturating_add(old_blob.size.saturating_add(new_blob.size));
                work.push((c, old, new, old_blob.size.max(new_blob.size)));
            }
        }
        if compared > self.budget {
            return Ok(None);
        }
        self.budget -= compared;

        let unread: HashSet<DenseId> = work
            .iter()
            .flat_map(|&(_, old, new, _)| [old, new])
            .filter(|oid| !self.spans.contains_key(oid))
            .filter_map(|oid| self.blobs.get(&oid).map(|b| b.d))
            .collect();
        let read: Vec<(Oid, Arc<Vec<u8>>)> = self
            .snap
            .load_many(unread)
            .await?
            .into_iter()
            .map(|(d, data)| (self.snap.oid(d), data))
            .collect();
        let mut spans = std::mem::take(&mut self.spans);
        let (spans, scored) = tokio::task::spawn_blocking(move || {
            for (oid, data) in read {
                spans.insert(oid, Spans::of(&data));
            }
            let scored = work
                .into_iter()
                .map(|(candidate, old, new, larger)| {
                    let copied = match (spans.get(&old), spans.get(&new)) {
                        (Some(old), Some(new)) => old.shared(new),
                        _ => 0,
                    };
                    Scored {
                        candidate,
                        score: score(copied, larger),
                    }
                })
                .collect::<Vec<_>>();
            (spans, scored)
        })
        .await?;
        self.spans = spans;
        Ok(Some(scored))
    }
}

/// How alike two blobs are, on git's scale, given the bytes they share and
/// the size of the larger.
fn score(copied: u64, larger: u64) -> u64 {
    if larger == 0 {
        return 0;
    }
    copied.saturating_mul(MAX_SCORE) / larger
}

/// A blob as git's rename estimate sees it: how many bytes fall in chunks of
/// each hash, sorted by hash.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spans(Vec<(u32, u64)>);

impl Spans {
    /// Chunks end at a newline or at [`SPAN_MAX_LEN`] bytes. In text, a
    /// carriage return before a newline is skipped, so line endings do not
    /// count as content.
    fn of(data: &[u8]) -> Self {
        let text = !data.iter().take(BINARY_SNIFF_LEN).any(|b| *b == 0);
        let mut bytes: HashMap<u32, u64> = HashMap::new();
        let mut chunk = Chunk::default();
        let mut rest = data.iter().copied().peekable();
        while let Some(c) = rest.next() {
            if text && c == b'\r' && rest.peek() == Some(&b'\n') {
                continue;
            }
            chunk.push(c);
            if chunk.len == SPAN_MAX_LEN || c == b'\n' {
                *bytes.entry(chunk.hash()).or_default() += chunk.len;
                chunk = Chunk::default();
            }
        }
        if chunk.len > 0 {
            *bytes.entry(chunk.hash()).or_default() += chunk.len;
        }
        let mut spans: Vec<(u32, u64)> = bytes.into_iter().collect();
        spans.sort_unstable();
        Spans(spans)
    }

    /// The bytes of `self` that `other` also holds, per hash.
    fn shared(&self, other: &Spans) -> u64 {
        let mut theirs = other.0.iter().peekable();
        let mut copied = 0;
        for (hash, ours) in &self.0 {
            while theirs.next_if(|(h, _)| h < hash).is_some() {}
            if let Some((_, theirs)) = theirs.next_if(|(h, _)| h == hash) {
                copied += ours.min(theirs);
            }
        }
        copied
    }
}

/// The rolling state of git's chunk hash.
#[derive(Debug, Default)]
struct Chunk {
    high: u32,
    low: u32,
    len: u64,
}

impl Chunk {
    fn push(&mut self, c: u8) {
        let high = self.high;
        self.high = ((self.high << 7) ^ (self.low >> 25)).wrapping_add(u32::from(c));
        self.low = (self.low << 7) ^ (high >> 25);
        self.len += 1;
    }

    fn hash(&self) -> u32 {
        self.high.wrapping_add(self.low.wrapping_mul(0x61)) % SPAN_HASH_BASE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize, line: impl Fn(usize) -> String) -> Vec<u8> {
        (0..n).map(line).collect::<String>().into_bytes()
    }

    fn plain(i: usize) -> String {
        format!("line {i}\n")
    }

    fn percent(a: &[u8], b: &[u8]) -> u64 {
        let larger = u64::try_from(a.len().max(b.len())).unwrap_or(u64::MAX);
        score(Spans::of(a).shared(&Spans::of(b)), larger) * 100 / MAX_SCORE
    }

    #[test]
    fn similarity_is_what_git_reports() {
        let base = lines(20, plain);
        let cases: [(&str, Vec<u8>, Vec<u8>, u64); 7] = [
            (
                "one line edited",
                base.clone(),
                lines(20, |i| {
                    if i == 5 {
                        "changed 5\n".into()
                    } else {
                        plain(i)
                    }
                }),
                93,
            ),
            ("doubled in length", base.clone(), lines(40, plain), 48),
            (
                "CRLF to LF",
                lines(20, |i| format!("line {i}\r\n")),
                base.clone(),
                88,
            ),
            (
                "lines past the chunk limit",
                lines(10, |i| format!("{} {i}\n", "x".repeat(150))),
                lines(10, |i| {
                    let c = if i == 3 { "y" } else { "x" };
                    format!("{} {i}\n", c.repeat(150))
                }),
                98,
            ),
            (
                "lines reordered",
                base.clone(),
                lines(20, |i| plain(19 - i)),
                100,
            ),
            (
                "every other line rewritten",
                base.clone(),
                lines(20, |i| {
                    if i % 2 == 1 {
                        format!("other {i}\n")
                    } else {
                        plain(i)
                    }
                }),
                46,
            ),
            (
                "one unterminated chunk",
                b"abcdef".to_vec(),
                b"abcdefg".to_vec(),
                0,
            ),
        ];
        for (what, a, b, git) in cases {
            assert_eq!(
                percent(&a, &b),
                git,
                "{what}: git diff -M1% reports R{git:03}"
            );
        }
    }

    #[test]
    fn sizes_too_far_apart_are_never_read() {
        let half = Similarity::from_percent(50).unwrap();
        assert!(half.sizes_allow(100, 200), "half the larger can be shared");
        assert!(!half.sizes_allow(100, 201), "less than half can be shared");
        assert!(!half.sizes_allow(0, 1), "an empty blob shares nothing");
        assert_eq!(
            Similarity::from_percent(0),
            Some(half),
            "zero asks for git's default"
        );
        assert_eq!(
            Similarity::from_percent(101),
            None,
            "no pair is more than identical"
        );
        assert_eq!(
            half.halfway_to_identical(),
            Similarity::from_percent(75).unwrap(),
            "git's bar for a shared base name"
        );
    }

    fn change(path: &str, old: Option<u8>, new: Option<u8>) -> FileChange {
        let oid = |b: u8| Oid::from_slice(crate::git::oid::ObjectFormat::Sha1, &[b; 20]);
        FileChange {
            path: path.into(),
            old: old.map(oid),
            new: new.map(oid),
            old_path: None,
        }
    }

    fn renames(renamed: &Renamed) -> Vec<(&str, &str)> {
        let mut pairs: Vec<(&str, &str)> = renamed
            .changes
            .iter()
            .filter_map(|c| {
                let from = std::str::from_utf8(c.old_path.as_deref()?).ok()?;
                Some((from, std::str::from_utf8(&c.path).ok()?))
            })
            .collect();
        pairs.sort_unstable();
        pairs
    }

    #[test]
    fn an_identical_blob_pairs_with_its_own_base_name_first() {
        let mut pairing = Pairing::new(vec![
            change("a/other.txt", Some(1), None),
            change("a/kept.txt", Some(1), None),
            change("b/kept.txt", None, Some(1)),
            change("b/new.txt", None, Some(1)),
            change("edited.txt", Some(2), Some(3)),
        ]);
        pairing.pair_identical();
        let renamed = pairing.finish(false);
        assert_eq!(
            renames(&renamed),
            [("a/kept.txt", "b/kept.txt"), ("a/other.txt", "b/new.txt")],
            "the shared name decides between two copies of one blob"
        );
        assert_eq!(renamed.changes.len(), 3, "a modification is left as it was");
    }

    #[test]
    fn the_best_score_pairs_first_and_each_side_pairs_once() {
        let mut pairing = Pairing::new(vec![
            change("x", Some(1), None),
            change("y", Some(2), None),
            change("p", None, Some(3)),
            change("q", None, Some(4)),
        ]);
        let every = pairing.every_candidate().unwrap();
        let table: HashMap<(usize, usize), u64> =
            HashMap::from_iter([((0, 0), 50_000), ((0, 1), 55_000), ((1, 1), 59_000)]);
        let scored = every
            .into_iter()
            .map(|candidate| Scored {
                candidate,
                score: table
                    .get(&(candidate.deleted, candidate.added))
                    .copied()
                    .unwrap_or(0),
            })
            .collect();
        pairing.accept(scored, Similarity::from_percent(50).unwrap());
        assert_eq!(
            renames(&pairing.finish(false)),
            [("x", "p"), ("y", "q")],
            "y takes q first, which leaves x its second best"
        );
    }
}
