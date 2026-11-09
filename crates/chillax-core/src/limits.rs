//! Why an agent session failed, and how long to wait before trying again (ADR 0008). Pure: the clock, the local UTC offset and the jitter are passed in, so the
//! rules are unit-tested without sleeping.

use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveTime, TimeDelta, Utc};

/// What an `AgentFailed` error means for the dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    /// The agent CLI is signed out. Retrying cannot help until the stakeholder signs in.
    Auth,
    /// The account hit a usage limit. `reset` is when the agent said it comes back.
    RateLimit {
        reset: Option<DateTime<Utc>>,
    },
    Other,
}

/// Whether error text reads like a usage limit. Agents report it as `errorKind:
/// rate_limit` in the error data or as prose ("You've hit your session limit").
pub fn is_rate_limit(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "rate_limit",
        "rate limit",
        "ratelimit",
        "session limit",
        "usage limit",
        "hit your limit",
        "limit reached",
        "too many requests",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Sort an error message. `offset` is the local UTC offset used to read clock times
/// such as "resets 3pm".
pub fn classify_failure(error: &str, now: DateTime<Utc>, offset: FixedOffset) -> FailureKind {
    let lower = error.to_lowercase();
    if lower.contains("isn't signed in") || lower.contains("auth_required") {
        return FailureKind::Auth;
    }
    if is_rate_limit(error) {
        return FailureKind::RateLimit {
            reset: parse_reset(error, now, offset),
        };
    }
    FailureKind::Other
}

/// The time a limit message says the agent is available again, if it says. Reads, in
/// order: a relative wait ("in 2 hours 15 minutes"), an epoch (`resetsAt`), an RFC 3339
/// timestamp, and a clock time ("resets 3pm", read in `offset`). A time zone name in the
/// message is ignored; a wrong guess only makes the first probe early or late.
pub fn parse_reset(text: &str, now: DateTime<Utc>, offset: FixedOffset) -> Option<DateTime<Utc>> {
    let found = parse_relative(text)
        .map(|wait| now + wait)
        .or_else(|| parse_epoch(text))
        .or_else(|| parse_rfc3339(text))
        .or_else(|| parse_clock(text, now, offset))?;
    (found > now).then_some(found)
}

fn parse_relative(text: &str) -> Option<TimeDelta> {
    let lower = text.to_lowercase();
    let words: Vec<String> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '.'))
        .map(|w| w.trim_end_matches('.'))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    for (at, word) in words.iter().enumerate() {
        // "in 2 hours", "after 30 seconds", "in 5m"
        if !matches!(word.as_str(), "in" | "after") {
            continue;
        }
        let mut total = TimeDelta::zero();
        let mut found = false;
        let mut i = at + 1;
        while i < words.len() {
            let (number, unit) = split_number(&words[i]);
            let Some(number) = number else { break };
            let unit = if unit.is_empty() {
                i += 1;
                match words.get(i) {
                    Some(u) => u.as_str(),
                    None => break,
                }
            } else {
                unit
            };
            let seconds = match unit {
                "d" | "day" | "days" => 86_400.0,
                "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
                "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
                "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
                _ => break,
            };
            total += TimeDelta::milliseconds((number * seconds * 1000.0) as i64);
            found = true;
            i += 1;
            if words.get(i).is_some_and(|w| w == "and") {
                i += 1;
            }
        }
        if found {
            return Some(total);
        }
    }
    None
}

/// "15" -> (15, ""), "5m" -> (5, "m"), "hours" -> (None, "hours").
fn split_number(word: &str) -> (Option<f64>, &str) {
    let end = word
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(word.len());
    match word[..end].parse::<f64>() {
        Ok(n) => (Some(n), &word[end..]),
        Err(_) => (None, word),
    }
}

fn parse_epoch(text: &str) -> Option<DateTime<Utc>> {
    let lower = text.to_lowercase();
    for key in ["resetsat", "resets_at", "reset_at", "resetat", "reset_time"] {
        let Some(at) = lower.find(key) else { continue };
        let digits: String = lower[at + key.len()..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect();
        let Ok(value) = digits.parse::<i64>() else {
            continue;
        };
        // Milliseconds when it is too big for seconds.
        let seconds = if value > 100_000_000_000 {
            value / 1000
        } else {
            value
        };
        if let Some(time) = DateTime::from_timestamp(seconds, 0) {
            return Some(time);
        }
    }
    None
}

fn parse_rfc3339(text: &str) -> Option<DateTime<Utc>> {
    text.split_whitespace()
        .map(|t| t.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | '(' | ')' | '.' | ';')))
        .find_map(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

fn parse_clock(text: &str, now: DateTime<Utc>, offset: FixedOffset) -> Option<DateTime<Utc>> {
    let lower = text.to_lowercase();
    let at = lower.find("reset")?;
    let rest = &lower[at..];
    let words: Vec<&str> = rest
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '·' | '"'))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, word) in words.iter().enumerate().skip(1) {
        let mut word = word.trim_end_matches('.').to_string();
        // "3 pm" is two words.
        if matches!(words.get(i + 1), Some(&"am" | &"pm"))
            && word.chars().all(|c| c.is_ascii_digit() || c == ':')
        {
            word.push_str(words[i + 1]);
        }
        if let Some(time) = clock_word(&word) {
            let local = now.with_timezone(&offset);
            let mut day = local.date_naive();
            for _ in 0..2 {
                let candidate = day
                    .and_time(time)
                    .and_local_timezone(offset)
                    .single()?
                    .with_timezone(&Utc);
                if candidate > now {
                    return Some(candidate);
                }
                day = day.succ_opt()?;
            }
        }
    }
    None
}

/// "3pm", "3:30pm", "15:00" -> a time of day.
fn clock_word(word: &str) -> Option<NaiveTime> {
    let (body, meridiem) = if let Some(b) = word.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = word.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (word, None)
    };
    let (hour, minute) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        // A bare number is only a time with am/pm after it.
        None if meridiem.is_some() => (body.parse::<u32>().ok()?, 0),
        None => return None,
    };
    let hour = match meridiem {
        Some(pm) if (1..=12).contains(&hour) => hour % 12 + if pm { 12 } else { 0 },
        Some(_) => return None,
        None => hour,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// How long a ticket is left alone after its `failures`-th failed session in a row:
/// 30 s, 1 min, 2 min, … capped at 15 min.
pub fn ticket_backoff(failures: u32) -> Duration {
    backoff(
        failures,
        Duration::from_secs(30),
        Duration::from_secs(15 * 60),
    )
}

/// `base`, doubled for each failure after the first, never more than `cap`.
pub fn backoff(failures: u32, base: Duration, cap: Duration) -> Duration {
    let exp = failures.saturating_sub(1).min(16);
    base.saturating_mul(1 << exp).min(cap)
}

/// Where randomness for probe timing comes from. A trait so tests can pin it.
pub trait Jitter: Send + Sync {
    /// A duration in `lo..=hi`.
    fn between(&self, lo: Duration, hi: Duration) -> Duration;
}

/// Jitter from the clock's sub-second noise; good enough to spread probes out.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClockJitter;

impl Jitter for ClockJitter {
    fn between(&self, lo: Duration, hi: Duration) -> Duration {
        if hi <= lo {
            return lo;
        }
        let noise = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs().rotate_left(17))
            .unwrap_or_default();
        let span = (hi - lo).as_millis() as u64 + 1;
        lo + Duration::from_millis(noise % span)
    }
}

/// When to probe a limited backend next. With a known reset: 1–3 minutes after it.
/// Otherwise 10–20 minutes, doubling after each failed probe (`attempt`), up to an hour.
pub fn probe_delay(
    reset: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    attempt: u32,
    jitter: &dyn Jitter,
) -> Duration {
    const MINUTE: Duration = Duration::from_secs(60);
    if let Some(reset) = reset
        && attempt == 0
    {
        let wait = (reset - now).to_std().unwrap_or_default();
        return wait + jitter.between(MINUTE, 3 * MINUTE);
    }
    let factor = 1u32 << attempt.min(2);
    let (lo, hi) = (10 * MINUTE * factor, 20 * MINUTE * factor);
    let cap = 60 * MINUTE;
    jitter.between(lo.min(cap), hi.min(cap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct Fixed(Duration);
    impl Jitter for Fixed {
        fn between(&self, lo: Duration, hi: Duration) -> Duration {
            self.0.clamp(lo, hi)
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 23, 12, 0, 0).unwrap()
    }
    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    #[test]
    fn rate_limit_and_auth_failures_are_told_apart() {
        let kind = |s: &str| classify_failure(s, now(), utc());
        assert_eq!(
            kind("the Claude Code agent isn't signed in. Run `claude`"),
            FailureKind::Auth
        );
        assert!(matches!(
            kind("You've hit your session limit"),
            FailureKind::RateLimit { reset: None }
        ));
        assert!(matches!(
            kind("Internal error ({\"errorKind\":\"rate_limit\"})"),
            FailureKind::RateLimit { .. }
        ));
        assert_eq!(kind("spawn failed: No such file"), FailureKind::Other);
    }

    #[test]
    fn relative_waits_are_read() {
        let reset = |s: &str| parse_reset(s, now(), utc());
        assert_eq!(
            reset("limit reached, resets in 2 hours 15 minutes"),
            Some(now() + TimeDelta::minutes(135))
        );
        assert_eq!(
            reset("Try again in 45 minutes."),
            Some(now() + TimeDelta::minutes(45))
        );
        assert_eq!(
            reset("retry after 30 seconds"),
            Some(now() + TimeDelta::seconds(30))
        );
        assert_eq!(
            reset("window closes in 5m"),
            Some(now() + TimeDelta::minutes(5))
        );
        assert_eq!(reset("nothing to see"), None);
    }

    #[test]
    fn absolute_times_are_read() {
        let reset = |s: &str, off| parse_reset(s, now(), off);
        let target = now() + TimeDelta::hours(3);
        assert_eq!(
            reset(&format!("x {{\"resetsAt\":{}}}", target.timestamp()), utc()),
            Some(target)
        );
        assert_eq!(
            reset(
                &format!("x \"resetsAt\": {}", target.timestamp_millis()),
                utc()
            ),
            Some(target)
        );
        assert_eq!(
            reset("resets at 2026-10-23T18:30:00Z please", utc()),
            Some(Utc.with_ymd_and_hms(2026, 10, 23, 18, 30, 0).unwrap())
        );
        // A time already past is no reset time.
        assert_eq!(reset("resets at 2026-10-23T08:00:00Z", utc()), None);
    }

    #[test]
    fn clock_times_use_the_local_offset_and_roll_to_tomorrow() {
        let reset = |s: &str, off| parse_reset(s, now(), off);
        assert_eq!(
            reset(
                "You've hit your session limit · resets 3pm (America/Los_Angeles)",
                utc()
            ),
            Some(Utc.with_ymd_and_hms(2026, 10, 23, 15, 0, 0).unwrap())
        );
        assert_eq!(
            reset("resets 11:30 am", utc()),
            Some(Utc.with_ymd_and_hms(2026, 10, 24, 11, 30, 0).unwrap())
        );
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            reset("resets 3pm", plus_two),
            Some(Utc.with_ymd_and_hms(2026, 10, 23, 13, 0, 0).unwrap())
        );
    }

    #[test]
    fn ticket_backoff_doubles_to_fifteen_minutes() {
        let secs = |n| ticket_backoff(n).as_secs();
        assert_eq!(
            [
                secs(1),
                secs(2),
                secs(3),
                secs(4),
                secs(5),
                secs(6),
                secs(50)
            ],
            [30, 60, 120, 240, 480, 900, 900]
        );
        assert_eq!(secs(0), 30);
    }

    #[test]
    fn probes_follow_the_reset_or_back_off() {
        let min = |n: u64| Duration::from_secs(n * 60);
        let jitter = Fixed(min(2));
        // Reset in 30 minutes: probe 1–3 minutes after it.
        let reset = Some(now() + TimeDelta::minutes(30));
        assert_eq!(probe_delay(reset, now(), 0, &jitter), min(32));
        assert_eq!(probe_delay(reset, now(), 0, &Fixed(min(0))), min(31));
        assert_eq!(probe_delay(reset, now(), 0, &Fixed(min(9))), min(33));
        // No reset: 10–20 minutes, doubling per failed probe.
        let lo = Fixed(Duration::ZERO);
        assert_eq!(probe_delay(None, now(), 0, &lo), min(10));
        assert_eq!(probe_delay(None, now(), 1, &lo), min(20));
        assert_eq!(probe_delay(None, now(), 5, &lo), min(40));
        assert_eq!(probe_delay(None, now(), 5, &Fixed(min(999))), min(60));
        // A reset that already passed still waits the jitter.
        let past = Some(now() - TimeDelta::minutes(5));
        assert_eq!(probe_delay(past, now(), 0, &jitter), min(2));
        // A failed probe falls back to backoff even when a reset was known.
        assert_eq!(probe_delay(reset, now(), 1, &lo), min(20));
    }

    #[test]
    fn clock_jitter_stays_in_range() {
        let j = ClockJitter;
        for _ in 0..50 {
            let d = j.between(Duration::from_secs(60), Duration::from_secs(180));
            assert!((60..=180).contains(&d.as_secs()), "{d:?}");
        }
    }
}
