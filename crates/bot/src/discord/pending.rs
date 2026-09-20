//! Questions waiting on a "did you mean…?" pick. In-memory, keyed by a random
//! [`PendingId`], expiring after a TTL. No background task: expired entries
//! are pruned on insert and refused on take. The map is also capped at
//! [`MAX_PENDING`] entries (oldest evicted), so the bound does not scale
//! with `JUDGE_CONCURRENCY`.

use std::{
    collections::HashMap,
    fmt,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use judge_core::Ambiguous;
use nonempty::NonEmpty;
use uuid::Uuid;

use super::Audience;

/// Random handle of a pending question; what a `pick` button carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PendingId(Uuid);

impl PendingId {
    /// A fresh, unguessable id.
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

impl From<Uuid> for PendingId {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

impl fmt::Display for PendingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

/// One ambiguous span and the full names it could be (button order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSpan {
    /// The span as the user wrote it.
    pub query: String,
    /// Candidate card names.
    pub candidates: NonEmpty<String>,
}

impl From<Ambiguous> for PendingSpan {
    fn from(a: Ambiguous) -> Self {
        Self {
            query: a.query,
            candidates: a.candidates.map(|c| c.name),
        }
    }
}

/// A question that stopped at ambiguity, waiting for its asker to pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Channel / thread the question was asked in.
    pub thread_id: String,
    /// Discord user id of the asker; only they may pick.
    pub user_id: String,
    /// The question text (with any earlier picks already pinned as `[[Name]]`).
    pub text: String,
    /// Ambiguous spans, first one first.
    pub spans: NonEmpty<PendingSpan>,
    /// Who the answer is for; a pick re-runs the question for the same audience.
    pub audience: Audience,
}

/// Default lifetime of a pending question.
pub const DEFAULT_TTL: Duration = Duration::from_mins(10);
/// Most pending questions held at once; inserting past this evicts the oldest.
pub const MAX_PENDING: usize = 256;

/// Why a take failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TakeError {
    /// Unknown id: expired, already taken, or never issued.
    #[error("no pending question with that id (expired or already answered)")]
    Missing,
    /// Present, but owned by someone else; left in place.
    #[error("that pending question belongs to another user")]
    NotOwner,
}

struct Entry {
    created: Instant,
    pending: Pending,
}

/// The map of pending questions.
pub struct PendingStore {
    ttl: Duration,
    inner: Mutex<HashMap<PendingId, Entry>>,
}

impl fmt::Debug for PendingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingStore")
            .field("ttl", &self.ttl)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl Default for PendingStore {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

impl PendingStore {
    /// An empty store whose entries live for `ttl`.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Store `p`, pruning expired entries first and evicting the oldest while
    /// the map is at [`MAX_PENDING`]; returns its id.
    pub fn insert(&self, p: Pending) -> PendingId {
        self.insert_at(p, Instant::now())
    }

    /// [`Self::insert`] with an explicit clock (tests).
    pub fn insert_at(&self, p: Pending, now: Instant) -> PendingId {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let ttl = self.ttl;
        map.retain(|_, e| now.duration_since(e.created) < ttl);
        while map.len() >= MAX_PENDING {
            let Some(oldest) = map.iter().min_by_key(|(_, e)| e.created).map(|(id, _)| *id) else {
                break;
            };
            map.remove(&oldest);
        }
        let id = PendingId::random();
        map.insert(
            id,
            Entry {
                created: now,
                pending: p,
            },
        );
        id
    }

    /// Remove and return `id` if it exists, has not expired, and belongs to `user_id`.
    ///
    /// # Errors
    /// [`TakeError::Missing`] (an expired entry is removed on the way) or
    /// [`TakeError::NotOwner`] (entry left in place for its owner).
    pub fn take_for(&self, id: PendingId, user_id: &str) -> Result<Pending, TakeError> {
        self.take_for_at(id, user_id, Instant::now())
    }

    /// [`Self::take_for`] with an explicit clock (tests).
    ///
    /// # Errors
    /// As [`Self::take_for`].
    pub fn take_for_at(
        &self,
        id: PendingId,
        user_id: &str,
        now: Instant,
    ) -> Result<Pending, TakeError> {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = map.get(&id) else {
            return Err(TakeError::Missing);
        };
        if now.duration_since(entry.created) >= self.ttl {
            map.remove(&id);
            return Err(TakeError::Missing);
        }
        if entry.pending.user_id != user_id {
            return Err(TakeError::NotOwner);
        }
        map.remove(&id).map(|e| e.pending).ok_or(TakeError::Missing)
    }

    /// Entries currently held (expired ones included until pruned).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// True if nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(user: &str) -> Pending {
        Pending {
            thread_id: "t".into(),
            user_id: user.into(),
            text: "what does urza do?".into(),
            audience: Audience::Channel,
            spans: NonEmpty::new(PendingSpan {
                query: "urza".into(),
                candidates: NonEmpty::from((
                    "Urza's Mine".to_owned(),
                    vec!["Urza's Tower".to_owned()],
                )),
            }),
        }
    }

    #[test]
    fn insert_then_take_by_owner_only() {
        let store = PendingStore::default();
        let id = store.insert(pending("u1"));
        assert_eq!(store.len(), 1);
        assert_eq!(store.take_for(id, "u2"), Err(TakeError::NotOwner));
        assert_eq!(store.len(), 1, "a foreign take leaves the entry");
        assert_eq!(store.take_for(id, "u1"), Ok(pending("u1")));
        assert!(store.is_empty());
        assert_eq!(store.take_for(id, "u1"), Err(TakeError::Missing));
        assert_eq!(
            store.take_for(PendingId::random(), "u1"),
            Err(TakeError::Missing)
        );
    }

    #[test]
    fn expires_after_ttl_and_prunes_on_insert() {
        let ttl = Duration::from_mins(10);
        let store = PendingStore::new(ttl);
        let t0 = Instant::now();
        let id = store.insert_at(pending("u1"), t0);
        // Just before the TTL: still there (a foreign take fails without removing it).
        assert!(
            store
                .take_for_at(id, "u2", t0 + Duration::from_secs(599))
                .is_err()
        );
        assert_eq!(store.len(), 1);
        // At the TTL: gone, and the take removed it.
        assert_eq!(
            store.take_for_at(id, "u1", t0 + ttl),
            Err(TakeError::Missing)
        );
        assert!(store.is_empty());
        // Prune on insert: an old entry is dropped when a new one arrives late enough.
        let old = store.insert_at(pending("u1"), t0);
        let fresh = store.insert_at(pending("u1"), t0 + ttl + Duration::from_secs(1));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.take_for_at(old, "u1", t0 + ttl + Duration::from_secs(2)),
            Err(TakeError::Missing)
        );
        assert!(
            store
                .take_for_at(fresh, "u1", t0 + ttl + Duration::from_secs(2))
                .is_ok()
        );
    }

    #[test]
    fn caps_the_map_by_evicting_the_oldest() {
        let store = PendingStore::default();
        let t0 = Instant::now();
        let first = store.insert_at(pending("u1"), t0);
        for i in 1..MAX_PENDING {
            store.insert_at(pending("u1"), t0 + Duration::from_millis(i as u64));
        }
        assert_eq!(store.len(), MAX_PENDING);
        let newest = store.insert_at(pending("u1"), t0 + Duration::from_secs(1));
        assert_eq!(store.len(), MAX_PENDING);
        assert_eq!(
            store.take_for_at(first, "u1", t0 + Duration::from_secs(2)),
            Err(TakeError::Missing)
        );
        assert!(
            store
                .take_for_at(newest, "u1", t0 + Duration::from_secs(2))
                .is_ok()
        );
    }

    #[test]
    fn ids_are_distinct_and_display_as_uuids() {
        let a = PendingId::random();
        let b = PendingId::random();
        assert_ne!(a, b);
        assert_eq!(a.to_string().len(), 36);
        assert_eq!(
            PendingId::from(Uuid::nil()).to_string(),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn pending_span_keeps_candidate_names_in_order() {
        use judge_core::{Card, CardId, Face, Layout};
        let card = |n: u128, name: &str| Card {
            id: CardId::new(Uuid::from_u128(n)),
            name: name.into(),
            layout: Layout::Normal,
            faces: NonEmpty::new(Face {
                name: name.into(),
                oracle_text: String::new(),
                mana_cost: String::new(),
                type_line: String::new(),
            }),
        };
        let a = Ambiguous {
            query: "urza".into(),
            candidates: NonEmpty::from((card(1, "Urza's Mine"), vec![card(2, "Urza's Tower")])),
        };
        let s = PendingSpan::from(a);
        assert_eq!(s.query, "urza");
        assert_eq!(
            s.candidates.iter().map(String::as_str).collect::<Vec<_>>(),
            ["Urza's Mine", "Urza's Tower"]
        );
    }
}
