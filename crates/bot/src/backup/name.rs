//! A backup's object name, `judgebot-<UTC stamp>.dump.gz`, exactly as
//! `scripts/backup-db.sh` writes it (`date -u +%Y%m%dT%H%M%SZ`), so the
//! script and the service read each other's backups: `list` and `fetch`
//! show both, and the schedule and the pruning count both.
//!
//! The stamp in the name is the backup's age. The schedule and the pruning
//! read it rather than the object's `LastModified`, so they only ever
//! consider objects with this name shape and need nothing from the store but
//! a listing.

use std::fmt;

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

/// What every backup's name starts with.
const PREFIX: &str = "judgebot-";
/// What every backup's name ends with: a gzipped `pg_dump -Fc` archive.
const SUFFIX: &str = ".dump.gz";

/// A UTC time to the second, as the stamp in a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp(OffsetDateTime);

impl Stamp {
    /// The current time, truncated to the second.
    #[must_use]
    pub fn now() -> Self {
        Self::from_unix(OffsetDateTime::now_utc().unix_timestamp())
            .unwrap_or(Self(OffsetDateTime::UNIX_EPOCH))
    }

    /// The time `secs` after the Unix epoch, if it is one `time` can hold.
    #[must_use]
    pub fn from_unix(secs: i64) -> Option<Self> {
        OffsetDateTime::from_unix_timestamp(secs).ok().map(Self)
    }

    /// Seconds since the Unix epoch.
    #[must_use]
    pub fn unix(self) -> i64 {
        self.0.unix_timestamp()
    }

    /// Parse `YYYYMMDDTHHMMSSZ`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 16
            || b.get(8) != Some(&b'T')
            || b.get(15) != Some(&b'Z')
            || !s.get(..8)?.bytes().all(|c| c.is_ascii_digit())
            || !s.get(9..15)?.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<u16>().ok();
        let month = Month::try_from(u8::try_from(num(4..6)?).ok()?).ok()?;
        let date =
            Date::from_calendar_date(i32::from(num(0..4)?), month, u8::try_from(num(6..8)?).ok()?)
                .ok()?;
        let clock = Time::from_hms(
            u8::try_from(num(9..11)?).ok()?,
            u8::try_from(num(11..13)?).ok()?,
            u8::try_from(num(13..15)?).ok()?,
        )
        .ok()?;
        Some(Self(PrimitiveDateTime::new(date, clock).assume_utc()))
    }

    /// `2026-10-09 04:15 UTC`, for a message a person reads.
    #[must_use]
    pub fn human(self) -> String {
        let t = self.0;
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02} UTC",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute()
        )
    }
}

impl fmt::Display for Stamp {
    /// `YYYYMMDDTHHMMSSZ`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.0;
        write!(
            f,
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        )
    }
}

/// The name of one backup object: `judgebot-<stamp>.dump.gz`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackupName(Stamp);

impl BackupName {
    /// The name of a backup taken at `stamp`.
    #[must_use]
    pub const fn at(stamp: Stamp) -> Self {
        Self(stamp)
    }

    /// Read a name back; `None` for anything that is not exactly this shape
    /// (another file in the prefix, a renamed copy).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        name.strip_prefix(PREFIX)?
            .strip_suffix(SUFFIX)
            .and_then(Stamp::parse)
            .map(Self)
    }

    /// When it was taken.
    #[must_use]
    pub const fn stamp(self) -> Stamp {
        self.0
    }
}

impl fmt::Display for BackupName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}{SUFFIX}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_in_the_scripts_format() -> Result<(), &'static str> {
        // `date -u +%Y%m%dT%H%M%SZ` at 1791519300 (2026-10-09T04:15:00Z).
        let stamp = Stamp::from_unix(1_791_519_300).ok_or("in range")?;
        let name = BackupName::at(stamp);
        assert_eq!(name.to_string(), "judgebot-20261009T041500Z.dump.gz");
        assert_eq!(BackupName::parse(&name.to_string()), Some(name));
        assert_eq!(stamp.human(), "2026-10-09 04:15 UTC");
        let leap = BackupName::parse("judgebot-20280229T235959Z.dump.gz").ok_or("leap day")?;
        assert_eq!(leap.stamp().to_string(), "20280229T235959Z");
        Ok(())
    }

    #[test]
    fn only_the_exact_shape_is_a_backup() {
        for other in [
            "judgebot-20261009T041500Z.dump",
            "judgebot-20261009T041500Z.dump.gz.part",
            "copy-judgebot-20261009T041500Z.dump.gz",
            "judgebot-2026109T041500Z.dump.gz",
            "judgebot-20261009 041500Z.dump.gz",
            "judgebot-20261309T041500Z.dump.gz",
            "judgebot-20270229T041500Z.dump.gz",
            "judgebot-20261009T246000Z.dump.gz",
            "judgebot-+2026109T041500Z.dump.gz",
            "judgebot-２0261009T041500Z.dump.gz",
            "",
        ] {
            assert_eq!(BackupName::parse(other), None, "{other}");
        }
    }

    #[test]
    fn names_sort_by_time() -> Result<(), &'static str> {
        let a = BackupName::parse("judgebot-20261009T041500Z.dump.gz").ok_or("a")?;
        let b = BackupName::parse("judgebot-20261016T041500Z.dump.gz").ok_or("b")?;
        assert!(a < b && a.to_string() < b.to_string());
        assert_eq!(b.stamp().unix() - a.stamp().unix(), 7 * 86_400);
        Ok(())
    }
}
