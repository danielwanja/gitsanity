//! Local-time handling.
//!
//! Everything is stored as epoch seconds (UTC). Days, months, weekdays, hours
//! and every range the user sees are derived in the *viewer's* time zone at
//! query time, never pre-bucketed in UTC. Windows are half-open `[start, end)`
//! and always start and end on a local midnight, so "Nov 1 – Nov 30" means
//! exactly those thirty local days, DST changes included.

use jiff::Timestamp;
use jiff::tz::TimeZone;

pub const DAY: i64 = 86_400;

/// A time zone flattened to a list of `(from, utc offset)` steps: offset lookups
/// become a binary search, cheap enough to run per commit over millions of rows.
#[derive(Clone, Debug)]
pub struct Clock {
    starts: Vec<i64>,
    offsets: Vec<i32>,
    pub name: String,
}

impl Clock {
    pub fn system() -> Clock {
        Clock::new(TimeZone::system())
    }

    pub fn named(name: &str) -> anyhow::Result<Clock> {
        Ok(Clock::new(TimeZone::get(name)?))
    }

    pub fn new(tz: TimeZone) -> Clock {
        let name = tz
            .iana_name()
            .map(str::to_owned)
            .unwrap_or_else(|| "local".to_owned());
        // 1970 .. 2100 covers any commit date worth plotting.
        let first = Timestamp::from_second(0).expect("epoch");
        let last = 4_102_444_800;
        let mut starts = vec![i64::MIN];
        let mut offsets = vec![tz.to_offset(first).seconds()];
        for tr in tz.following(first) {
            let at = tr.timestamp().as_second();
            if at > last {
                break;
            }
            starts.push(at);
            offsets.push(tr.offset().seconds());
        }
        Clock {
            starts,
            offsets,
            name,
        }
    }

    /// Seconds east of UTC at instant `t`.
    pub fn offset(&self, t: i64) -> i64 {
        let i = self.starts.partition_point(|s| *s <= t) - 1;
        self.offsets[i] as i64
    }

    /// Local calendar day (days since 1970-01-01) that instant `t` falls on.
    pub fn day(&self, t: i64) -> i64 {
        (t + self.offset(t)).div_euclid(DAY)
    }

    /// The instant local day `day` begins (its local midnight, or the first
    /// instant after it when a DST jump skips midnight).
    pub fn day_start(&self, day: i64) -> i64 {
        let wall = day * DAY;
        // Guess with the offset in force around then, then correct once.
        let mut t = wall - self.offset(wall);
        t = wall - self.offset(t);
        if self.day(t) < day {
            // Midnight does not exist that day; step to the transition.
            let i = self.starts.partition_point(|s| *s <= t);
            if let Some(next) = self.starts.get(i) {
                t = *next;
            }
        }
        t
    }

    /// (weekday Mon=0..Sun=6, hour 0..23) in local time.
    pub fn weekday_hour(&self, t: i64) -> (usize, usize) {
        let local = t + self.offset(t);
        let day = local.div_euclid(DAY);
        let weekday = (day + 3).rem_euclid(7); // 1970-01-01 was a Thursday
        (weekday as usize, (local.rem_euclid(DAY) / 3600) as usize)
    }

    pub fn civil(&self, t: i64) -> (i32, u32, u32) {
        civil_from_days(self.day(t))
    }

    /// `2026-02-03 17:20`
    pub fn datetime(&self, t: i64) -> String {
        let local = t + self.offset(t);
        let (y, m, d) = civil_from_days(local.div_euclid(DAY));
        let s = local.rem_euclid(DAY);
        format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", s / 3600, s % 3600 / 60)
    }

    /// `2026-02-03`
    pub fn date(&self, t: i64) -> String {
        let (y, m, d) = self.civil(t);
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Days since 1970-01-01 -> (year, month, day). Proleptic Gregorian.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y as i32, m, d)
}

pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1)) as u32
}

/// Shift a local day by whole months, clamping to the month's last day
/// (Mar 31 - 1 month = Feb 28/29).
pub fn add_months(day: i64, months: i32) -> i64 {
    let (y, m, d) = civil_from_days(day);
    let total = y * 12 + (m as i32 - 1) + months;
    let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    days_from_civil(ny, nm, d.min(days_in_month(ny, nm)))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn month_name(m: u32) -> &'static str {
    MONTHS[(m as usize - 1) % 12]
}

// ------------------------------------------------------------------ ranges

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    D1,
    D5,
    M1,
    M6,
    Ytd,
    Y1,
    Y5,
    All,
}

impl Preset {
    pub const ALL: [Preset; 8] = [
        Preset::D1,
        Preset::D5,
        Preset::M1,
        Preset::M6,
        Preset::Ytd,
        Preset::Y1,
        Preset::Y5,
        Preset::All,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::D1 => "1D",
            Preset::D5 => "5D",
            Preset::M1 => "1M",
            Preset::M6 => "6M",
            Preset::Ytd => "YTD",
            Preset::Y1 => "1Y",
            Preset::Y5 => "5Y",
            Preset::All => "All",
        }
    }
}

/// A window of whole local days: `[first, last]` inclusive as the user reads it,
/// `[start, end)` in epoch seconds for filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub preset: Preset,
    pub first_day: i64,
    pub last_day: i64,
    pub start: i64,
    pub end: i64,
}

/// The repository's own span, as local days.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub first_day: i64,
    pub last_day: i64,
}

impl Window {
    fn of(clock: &Clock, preset: Preset, first_day: i64, last_day: i64) -> Window {
        Window {
            preset,
            first_day,
            last_day,
            start: clock.day_start(first_day),
            end: clock.day_start(last_day + 1),
        }
    }

    /// The window for `preset` ending on `anchor` (a local day: normally the
    /// day of the latest commit, or today if that is earlier).
    pub fn ending(clock: &Clock, preset: Preset, anchor: i64, bounds: Bounds) -> Window {
        let first = match preset {
            Preset::D1 => anchor,
            Preset::D5 => anchor - 4,
            Preset::M1 => add_months(anchor + 1, -1),
            Preset::M6 => add_months(anchor + 1, -6),
            Preset::Y1 => add_months(anchor + 1, -12),
            Preset::Y5 => add_months(anchor + 1, -60),
            Preset::Ytd => days_from_civil(civil_from_days(anchor).0, 1, 1),
            Preset::All => return Window::of(clock, preset, bounds.first_day, bounds.last_day),
        };
        Window::of(clock, preset, first, anchor)
    }

    /// The neighbouring window (`dir` = -1 earlier, +1 later), or None when it
    /// would leave the repository's span entirely.
    pub fn shifted(&self, clock: &Clock, dir: i32, bounds: Bounds) -> Option<Window> {
        let (first, last) = match self.preset {
            Preset::All => return None,
            Preset::D1 => (
                self.first_day + i64::from(dir),
                self.last_day + i64::from(dir),
            ),
            Preset::D5 => (
                self.first_day + 5 * i64::from(dir),
                self.last_day + 5 * i64::from(dir),
            ),
            Preset::Ytd => {
                // Stepping from "year to date" walks whole calendar years.
                let y = civil_from_days(self.first_day).0 + dir;
                (days_from_civil(y, 1, 1), days_from_civil(y, 12, 31))
            }
            Preset::M1 | Preset::M6 | Preset::Y1 | Preset::Y5 => {
                let months = match self.preset {
                    Preset::M1 => 1,
                    Preset::M6 => 6,
                    Preset::Y1 => 12,
                    _ => 60,
                } * dir;
                // Windows tile: the neighbour starts where this one stops. Month
                // arithmetic runs on the exclusive end, so a calendar month steps
                // to calendar months (Mar 1-31 -> Feb 1-28 -> Jan 1-31).
                if dir < 0 {
                    (add_months(self.first_day, months), self.first_day - 1)
                } else {
                    let first = self.last_day + 1;
                    (first, (add_months(first, months) - 1).max(first))
                }
            }
        };
        if last < bounds.first_day || first > bounds.last_day {
            return None;
        }
        Some(Window::of(clock, self.preset, first, last))
    }

    pub fn days(&self) -> i64 {
        self.last_day - self.first_day + 1
    }

    /// `Nov 12, 2026` / `Oct 13 – Nov 12, 2026` / `Nov 13, 2025 – Nov 12, 2026`
    pub fn label(&self) -> String {
        let (y1, m1, d1) = civil_from_days(self.first_day);
        let (y2, m2, d2) = civil_from_days(self.last_day);
        if self.first_day == self.last_day {
            format!("{} {d1}, {y1}", month_name(m1))
        } else if y1 == y2 {
            format!("{} {d1} – {} {d2}, {y2}", month_name(m1), month_name(m2))
        } else {
            format!(
                "{} {d1}, {y1} – {} {d2}, {y2}",
                month_name(m1),
                month_name(m2)
            )
        }
    }
}

// ----------------------------------------------------------------- buckets

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bucket {
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Bucket {
    pub fn label(self) -> &'static str {
        match self {
            Bucket::Hour => "hour",
            Bucket::Day => "day",
            Bucket::Week => "week",
            Bucket::Month => "month",
            Bucket::Quarter => "quarter",
            Bucket::Year => "year",
        }
    }
}

/// Bucket boundaries for a window: `edges[i]..edges[i+1]` is bucket `i`.
/// All edges are local-time aligned (local midnight, Monday, 1st of month...).
pub struct Buckets {
    pub bucket: Bucket,
    pub edges: Vec<i64>,
}

impl Buckets {
    /// The finest bucket that suits the range and still fits `max` columns.
    pub fn fit(clock: &Clock, w: &Window, max: usize) -> Buckets {
        let finest = match w.preset {
            Preset::D1 | Preset::D5 => Bucket::Hour,
            _ if w.days() <= 2 => Bucket::Hour,
            _ if w.days() <= 190 => Bucket::Day,
            _ => Bucket::Month,
        };
        let ladder = [
            Bucket::Hour,
            Bucket::Day,
            Bucket::Week,
            Bucket::Month,
            Bucket::Quarter,
            Bucket::Year,
        ];
        let mut last = None;
        for b in ladder.into_iter().filter(|b| *b >= finest) {
            let built = Buckets::build(clock, w, b);
            if built.len() <= max.max(1) {
                return built;
            }
            last = Some(built);
        }
        last.expect("ladder is not empty")
    }

    pub fn build(clock: &Clock, w: &Window, bucket: Bucket) -> Buckets {
        let mut edges = Vec::new();
        match bucket {
            Bucket::Hour => {
                let mut t = w.start;
                while t < w.end {
                    edges.push(t);
                    // Next local top-of-hour (zones with :30 / :45 offsets included).
                    t += 3600 - (t + clock.offset(t)).rem_euclid(3600);
                }
            }
            Bucket::Day => edges.extend((w.first_day..=w.last_day).map(|d| clock.day_start(d))),
            Bucket::Week => {
                // Weeks start on Monday; the first one may be partial.
                let mut d = w.first_day;
                while d <= w.last_day {
                    edges.push(clock.day_start(d));
                    d += 7 - (d + 3).rem_euclid(7);
                }
            }
            Bucket::Month | Bucket::Quarter | Bucket::Year => {
                let step = match bucket {
                    Bucket::Month => 1,
                    Bucket::Quarter => 3,
                    _ => 12,
                };
                let mut d = w.first_day;
                while d <= w.last_day {
                    edges.push(clock.day_start(d));
                    let (y, m, _) = civil_from_days(d);
                    let months = y * 12 + (m as i32 - 1);
                    let next = (months / step + 1) * step;
                    d = days_from_civil(next.div_euclid(12), next.rem_euclid(12) as u32 + 1, 1);
                }
            }
        }
        edges.push(w.end);
        Buckets { bucket, edges }
    }

    pub fn len(&self) -> usize {
        self.edges.len() - 1
    }

    /// Index of the bucket holding `t` (which must lie inside the window).
    #[cfg(test)]
    pub fn index(&self, t: i64) -> usize {
        (self.edges.partition_point(|e| *e <= t) - 1).min(self.len() - 1)
    }

    /// Human label for bucket `i`: `14:00`, `Nov 12`, `Nov 2026`, `Q4 2026`, `2026`.
    pub fn label(&self, clock: &Clock, i: usize) -> String {
        let t = self.edges[i];
        let (y, m, d) = clock.civil(t);
        match self.bucket {
            Bucket::Hour => format!("{} {d} {:02}:00", month_name(m), clock.weekday_hour(t).1),
            Bucket::Day | Bucket::Week => format!("{} {d}, {y}", month_name(m)),
            Bucket::Month => format!("{} {y}", month_name(m)),
            Bucket::Quarter => format!("Q{} {y}", (m - 1) / 3 + 1),
            Bucket::Year => format!("{y}"),
        }
    }
}

/// `3h ago`, `5d ago`, `2y ago`
pub fn ago(now: i64, t: i64) -> String {
    let s = (now - t).max(0);
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        86_400..2_592_000 => format!("{}d ago", s / 86_400),
        2_592_000..31_536_000 => format!("{}mo ago", s / 2_592_000),
        _ => format!("{}y ago", s / 31_536_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_second()
    }

    #[test]
    fn civil_round_trip() {
        for day in [-1, 0, 1, 59, 60, 11_016, 19_000, 20_769, 47_482] {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn late_evening_commit_belongs_to_the_local_day_not_the_utc_day() {
        // 2026-02-03 23:30 in Denver is already Feb 4 in UTC.
        let t = ts("2026-02-04T06:30:00Z");
        let denver = Clock::named("America/Denver").unwrap();
        assert_eq!(denver.date(t), "2026-02-03");
        assert_eq!(denver.datetime(t), "2026-02-03 23:30");
        assert_eq!(denver.weekday_hour(t), (1, 23)); // Tuesday
        let tokyo = Clock::named("Asia/Tokyo").unwrap();
        assert_eq!(tokyo.date(t), "2026-02-04");
        assert_eq!(tokyo.weekday_hour(t), (2, 15)); // Wednesday
    }

    #[test]
    fn day_start_handles_dst_and_odd_offsets() {
        let denver = Clock::named("America/Denver").unwrap();
        // Spring forward 2026-03-08: that local day is 23 hours long.
        let d = days_from_civil(2026, 3, 8);
        assert_eq!(denver.day_start(d), ts("2026-03-08T07:00:00Z"));
        assert_eq!(denver.day_start(d + 1) - denver.day_start(d), 23 * 3600);
        // Fall back 2026-11-01: 25 hours.
        let d = days_from_civil(2026, 11, 1);
        assert_eq!(denver.day_start(d + 1) - denver.day_start(d), 25 * 3600);
        // Half-hour zone.
        let kolkata = Clock::named("Asia/Kolkata").unwrap();
        assert_eq!(
            kolkata.day_start(days_from_civil(2026, 1, 1)),
            ts("2025-12-31T18:30:00Z")
        );
        // Every instant of a day maps back to that day.
        for clock in [&denver, &kolkata] {
            for day in days_from_civil(2026, 3, 1)..days_from_civil(2026, 3, 20) {
                let (a, b) = (clock.day_start(day), clock.day_start(day + 1));
                assert_eq!(clock.day(a), day);
                assert_eq!(clock.day(b - 1), day);
            }
        }
    }

    #[test]
    fn midnight_skipped_by_dst() {
        // Beirut springs forward at 00:00 -> 01:00; the day starts at 01:00.
        let beirut = Clock::named("Asia/Beirut").unwrap();
        let d = days_from_civil(2025, 3, 30);
        let start = beirut.day_start(d);
        assert_eq!(beirut.day(start), d);
        assert_eq!(beirut.day(start - 1), d - 1);
        assert_eq!(beirut.datetime(start), "2025-03-30 01:00");
    }

    fn bounds() -> Bounds {
        Bounds {
            first_day: days_from_civil(2005, 4, 16),
            last_day: days_from_civil(2026, 11, 12),
        }
    }

    #[test]
    fn preset_windows_are_whole_local_days() {
        let clock = Clock::named("America/Denver").unwrap();
        let anchor = days_from_civil(2026, 11, 12);
        let w = Window::ending(&clock, Preset::D1, anchor, bounds());
        assert_eq!(w.label(), "Nov 12, 2026");
        assert_eq!(
            (w.start, w.end),
            (ts("2026-11-12T07:00:00Z"), ts("2026-11-13T07:00:00Z"))
        );

        let w = Window::ending(&clock, Preset::M1, anchor, bounds());
        assert_eq!(w.label(), "Oct 13 – Nov 12, 2026");
        // Crosses the Nov 1 DST change: start is in MDT (-6), end in MST (-7).
        assert_eq!(
            (w.start, w.end),
            (ts("2026-10-13T06:00:00Z"), ts("2026-11-13T07:00:00Z"))
        );

        assert_eq!(
            Window::ending(&clock, Preset::D5, anchor, bounds()).label(),
            "Nov 8 – Nov 12, 2026"
        );
        assert_eq!(
            Window::ending(&clock, Preset::Ytd, anchor, bounds()).label(),
            "Jan 1 – Nov 12, 2026"
        );
        assert_eq!(
            Window::ending(&clock, Preset::Y1, anchor, bounds()).label(),
            "Nov 13, 2025 – Nov 12, 2026"
        );
        assert_eq!(
            Window::ending(&clock, Preset::All, anchor, bounds()).label(),
            "Apr 16, 2005 – Nov 12, 2026"
        );
    }

    #[test]
    fn stepping_tiles_without_gaps_or_overlap() {
        let clock = Clock::named("Europe/Zurich").unwrap();
        for preset in [
            Preset::D1,
            Preset::D5,
            Preset::M1,
            Preset::M6,
            Preset::Y1,
            Preset::Y5,
        ] {
            // Start on a month end to exercise clamping.
            let mut w = Window::ending(&clock, preset, days_from_civil(2026, 3, 31), bounds());
            for _ in 0..8 {
                let Some(prev) = w.shifted(&clock, -1, bounds()) else {
                    break;
                };
                assert_eq!(prev.last_day + 1, w.first_day, "{preset:?}");
                assert_eq!(prev.end, w.start, "{preset:?}");
                let back = prev.shifted(&clock, 1, bounds()).unwrap();
                assert_eq!(back.first_day, w.first_day, "{preset:?}");
                w = prev;
            }
        }
        let w = Window::ending(&clock, Preset::M1, days_from_civil(2026, 3, 31), bounds());
        assert_eq!(w.label(), "Mar 1 – Mar 31, 2026");
        assert_eq!(
            w.shifted(&clock, -1, bounds()).unwrap().label(),
            "Feb 1 – Feb 28, 2026"
        );
    }

    #[test]
    fn ytd_steps_by_calendar_year_and_all_does_not_move() {
        let clock = Clock::named("UTC").unwrap();
        let anchor = days_from_civil(2026, 11, 12);
        let ytd = Window::ending(&clock, Preset::Ytd, anchor, bounds());
        assert_eq!(
            ytd.shifted(&clock, -1, bounds()).unwrap().label(),
            "Jan 1 – Dec 31, 2025"
        );
        assert!(
            Window::ending(&clock, Preset::All, anchor, bounds())
                .shifted(&clock, -1, bounds())
                .is_none()
        );
        // Cannot step past the repository's span.
        let d1 = Window::ending(&clock, Preset::D1, anchor, bounds());
        assert!(d1.shifted(&clock, 1, bounds()).is_none());
    }

    #[test]
    fn buckets_align_to_local_boundaries() {
        let clock = Clock::named("America/Denver").unwrap();
        let anchor = days_from_civil(2026, 11, 12);
        let day = Window::ending(&clock, Preset::D1, anchor, bounds());
        let b = Buckets::build(&clock, &day, Bucket::Hour);
        assert_eq!(b.len(), 24);
        assert_eq!(b.label(&clock, 0), "Nov 12 00:00");
        // 23:59:59 local is the last bucket of that day, not the next day.
        assert_eq!(b.index(day.end - 1), 23);

        let year = Window::ending(&clock, Preset::Y1, anchor, bounds());
        let b = Buckets::build(&clock, &year, Bucket::Month);
        assert_eq!(b.len(), 13); // partial Nov 2025 .. partial Nov 2026
        assert_eq!(b.label(&clock, 1), "Dec 2025");
        assert_eq!(b.edges[1], ts("2025-12-01T07:00:00Z"));
        // New Year's Eve 23:30 local stays in December even though UTC is January.
        assert_eq!(
            b.label(&clock, b.index(ts("2026-01-01T06:30:00Z"))),
            "Dec 2025"
        );

        let m = Window::ending(&clock, Preset::M1, anchor, bounds());
        let b = Buckets::build(&clock, &m, Bucket::Day);
        assert_eq!(b.len(), 31);
        assert_eq!(b.edges[20] - b.edges[19], 25 * 3600); // Nov 1, fall back

        let b = Buckets::build(&clock, &m, Bucket::Week);
        assert_eq!(b.label(&clock, 0), "Oct 13, 2026"); // a Tuesday: partial first week
        assert_eq!(b.label(&clock, 1), "Oct 19, 2026"); // Monday

        let all = Window::ending(&clock, Preset::All, anchor, bounds());
        assert_eq!(Buckets::fit(&clock, &all, 300).bucket, Bucket::Month);
        assert_eq!(Buckets::fit(&clock, &all, 100).bucket, Bucket::Quarter);
        assert_eq!(Buckets::fit(&clock, &all, 30).bucket, Bucket::Year);
    }
}
