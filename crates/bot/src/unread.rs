//! Stored rows this build does not read, such as a value a newer build of the
//! same major wrote. Each read leaves such a row out, and [`first`] keeps the
//! WARN about it to once per row and site per process, so a page that lists
//! one on every request logs it once rather than on every request.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, PoisonError};

use uuid::Uuid;

/// Where a row was left out. One row can be unreadable at two sites (the feed
/// and the admin list read the same call), and each says so once.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Site {
    Feed,
    AdminList,
    AdminCall,
    AdminRating,
    Queue,
    PriorCall,
}

/// How many rows are remembered. Past it the set starts over, so a row may
/// be logged again: memory stays bounded whatever the table holds.
const REMEMBERED: usize = 4096;

/// The rows already logged.
#[derive(Default)]
struct Seen(Mutex<HashSet<(Site, Uuid)>>);

impl Seen {
    fn first(&self, site: Site, id: Uuid) -> bool {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= REMEMBERED && !seen.contains(&(site, id)) {
            seen.clear();
        }
        seen.insert((site, id))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
}

static SEEN: LazyLock<Seen> = LazyLock::new(Seen::default);

/// Whether this is the first time this process left row `id` out at `site`
/// (since the set last started over), i.e. whether to log it.
pub(crate) fn first(site: Site, id: Uuid) -> bool {
    SEEN.first(site, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_is_logged_once_per_site() {
        let seen = Seen::default();
        let id = Uuid::new_v4();
        assert!(seen.first(Site::Feed, id));
        assert!(!seen.first(Site::Feed, id));
        assert!(seen.first(Site::AdminList, id));
        assert!(!seen.first(Site::AdminList, id));
    }

    #[test]
    fn the_set_stays_bounded() {
        let seen = Seen::default();
        for _ in 0..=REMEMBERED {
            seen.first(Site::Queue, Uuid::new_v4());
        }
        assert!(seen.len() <= REMEMBERED, "{}", seen.len());
    }
}
