//! Alert patterns and the rules that put them on events.
//!
//! For each event, the first that applies wins:
//! 1. a `#tag` in its title naming a pattern,
//! 2. the pattern chosen for it on valet's web page,
//! 3. alerts someone set in a calendar app (left exactly as they are; those an
//!    app adds from its own defaults, when it marks them, do not count),
//! 4. its calendar's default pattern (one for timed events, one for all-day ones).
//!
//! Alarms valet adds carry `X-VALET:<tag>`, so it can tell them from manual ones.

use crate::ical::{self, Component, Property};

#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    pub id: i64,
    pub name: String,
    /// Lower-case, without the `#`.
    pub tag: String,
    /// Minutes relative to the event's start; negative is before.
    pub offsets: Vec<i64>,
    /// Its alerts ring like an alarm on valet's Android app.
    pub ring: bool,
}

pub struct Rules<'a> {
    pub patterns: &'a [Pattern],
    pub default_timed: Option<i64>,
    pub default_allday: Option<i64>,
    /// Pattern chosen on the web page for this event.
    pub choice: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Tag(String),
    Chosen(String),
    Manual,
    Default(String),
    Nothing,
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Tag(n) => format!("{n} (tag in the title)"),
            Source::Chosen(n) => format!("{n} (chosen here)"),
            Source::Manual => "set in an app".into(),
            Source::Default(n) => format!("{n} (calendar default)"),
            Source::Nothing => "none".into(),
        }
    }
}

const MARK: &str = "X-VALET";

impl Rules<'_> {
    fn pattern(&self, id: Option<i64>) -> Option<&Pattern> {
        id.and_then(|id| self.patterns.iter().find(|p| p.id == id))
    }

    fn tagged(&self, summary: &str) -> Option<&Pattern> {
        tags(summary).find_map(|t| self.patterns.iter().find(|p| p.tag.eq_ignore_ascii_case(t)))
    }

    /// What applies to one VEVENT, and the pattern its alarms should follow
    /// (`None`: no alarms from valet; `Manual` leaves them alone).
    fn decide(&self, ev: &Component) -> (Source, Option<&Pattern>) {
        if let Some(p) = self.tagged(ev.value("SUMMARY").unwrap_or("")) {
            return (Source::Tag(p.name.clone()), Some(p));
        }
        if let Some(p) = self.pattern(self.choice) {
            return (Source::Chosen(p.name.clone()), Some(p));
        }
        if self.has_manual(ev) {
            return (Source::Manual, None);
        }
        let default = if is_all_day(ev) { self.default_allday } else { self.default_timed };
        match self.pattern(default) {
            Some(p) => (Source::Default(p.name.clone()), Some(p)),
            None => (Source::Nothing, None),
        }
    }

    /// Alarms without valet's mark, unless they are exactly some pattern
    /// (an app that re-saved the event may have dropped the mark).
    fn has_manual(&self, ev: &Component) -> bool {
        let unmarked: Vec<&Component> = alarms(ev).filter(|a| a.prop(MARK).is_none() && !app_default(a)).collect();
        if unmarked.is_empty() {
            return false;
        }
        let Some(triggers) = unmarked.iter().map(|a| relative_trigger(a)).collect::<Option<Vec<_>>>() else {
            return true;
        };
        let triggers = sorted(triggers);
        !self.patterns.iter().any(|p| sorted(p.offsets.iter().map(|m| m * 60).collect()) == triggers)
    }

    /// What applies to the event, for display: its first VEVENT decides.
    pub fn source(&self, cal: &Component) -> Source {
        cal.children.iter().find(|c| c.name == "VEVENT").map(|ev| self.decide(ev).0).unwrap_or(Source::Nothing)
    }

    /// Rewrites the alarms of every VEVENT in `cal`; true if anything changed.
    pub fn apply(&self, cal: &mut Component) -> bool {
        let mut changed = false;
        for i in 0..cal.children.len() {
            if cal.children[i].name != "VEVENT" {
                continue;
            }
            let (source, pattern) = self.decide(&cal.children[i]);
            if source == Source::Manual {
                continue;
            }
            let (desired, tag) = pattern.map_or((&[][..], ""), |p| (&p.offsets[..], p.tag.as_str()));
            let ev = &cal.children[i];
            let current: Option<Vec<i64>> = alarms(ev).map(relative_trigger).collect();
            let wanted = sorted(desired.iter().map(|m| m * 60).collect());
            if current.map(sorted).as_ref() == Some(&wanted) {
                continue;
            }
            let ev = &mut cal.children[i];
            let summary = ev.value("SUMMARY").filter(|s| !s.is_empty()).unwrap_or("Reminder").to_string();
            ev.children.retain(|c| c.name != "VALARM");
            for m in desired {
                let mut a = Component::new("VALARM");
                a.push("ACTION", "DISPLAY");
                a.push("DESCRIPTION", &summary);
                a.push("TRIGGER", &ical::format_duration(*m));
                a.push(MARK, tag);
                ev.children.push(a);
            }
            changed = true;
        }
        changed
    }
}

/// An alarm an app added from its own default settings, not one picked by
/// hand: Apple Calendar marks those.
fn app_default(alarm: &Component) -> bool {
    alarm.value("X-APPLE-DEFAULT-ALARM").is_some_and(|v| v.eq_ignore_ascii_case("TRUE"))
}

fn alarms(ev: &Component) -> impl Iterator<Item = &Component> {
    ev.children.iter().filter(|c| c.name == "VALARM")
}

/// Seconds from the start, for a trigger relative to the start; `None` for
/// absolute triggers or triggers relative to the end.
fn relative_trigger(alarm: &Component) -> Option<i64> {
    let t: &Property = alarm.prop("TRIGGER")?;
    if t.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE-TIME"))
        || t.param("RELATED").is_some_and(|v| v.eq_ignore_ascii_case("END"))
    {
        return None;
    }
    ical::parse_duration(&t.value)
}

fn sorted(mut v: Vec<i64>) -> Vec<i64> {
    v.sort_unstable();
    v
}

pub fn is_all_day(ev: &Component) -> bool {
    ev.prop("DTSTART").is_some_and(|p| p.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || p.value.len() == 8)
}

/// The `#words` in a title.
pub fn tags(summary: &str) -> impl Iterator<Item = &str> {
    summary.split('#').skip(1).filter_map(|rest| {
        let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
        (end > 0).then(|| &rest[..end])
    })
}

pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Parses what people type for a pattern: comma-separated alerts, each made of
/// durations (`12m`, `2h`, `1d`, `1w`, before the start unless written `+30m`)
/// and optionally a clock time (`9:00`, from midnight: for all-day events).
/// `12m, 2m` · `1d 3h` · `9:00` (all-day: 9:00 that day) · `1d 18:00` (6 pm the day before).
pub fn parse_spec(spec: &str) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let mut total = 0i64;
        for tok in entry.split_whitespace() {
            if let Some((h, m)) = tok.split_once(':') {
                let (h, m): (i64, i64) = match (h.parse(), m.parse()) {
                    (Ok(h), Ok(m)) if h < 24 && m < 60 => (h, m),
                    _ => return Err(format!("“{tok}” is not a time like 9:00")),
                };
                total += h * 60 + m;
                continue;
            }
            let (sign, t) = match tok.strip_prefix('+') {
                Some(t) => (1, t),
                None => (-1, tok.strip_prefix('-').unwrap_or(tok)),
            };
            let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
            let n: i64 = t[..digits].parse().map_err(|_| format!("“{tok}” needs a number, like 12m"))?;
            let unit = match &t[digits..] {
                "m" | "min" | "mn" => 1,
                "h" => 60,
                "d" | "j" => 1440,
                "w" => 10080,
                u => return Err(format!("unknown unit “{u}” in “{tok}” (use m, h, d or w)")),
            };
            total += sign * n * unit;
        }
        out.push(total);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// `12 min before`, `1 d 3 h before`, `at the start`, `30 min after`.
pub fn describe_offset(minutes: i64) -> String {
    if minutes == 0 {
        return "at the start".into();
    }
    let m = minutes.abs();
    let mut parts = Vec::new();
    for (n, unit) in [(m / 1440, "d"), (m % 1440 / 60, "h"), (m % 60, "min")] {
        if n > 0 {
            parts.push(format!("{n} {unit}"));
        }
    }
    format!("{} {}", parts.join(" "), if minutes < 0 { "before" } else { "after" })
}

/// For all-day events (which start at midnight): `9:00 the same day`, `18:00 1 day before`.
pub fn describe_all_day(minutes: i64) -> String {
    let day = minutes.div_euclid(1440);
    let time = minutes.rem_euclid(1440);
    let when = match day {
        0 => "the same day".to_string(),
        -1 => "the day before".to_string(),
        d if d < 0 => format!("{} days before", -d),
        1 => "the day after".to_string(),
        d => format!("{d} days after"),
    };
    format!("{:02}:{:02} {when}", time / 60, time % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns() -> Vec<Pattern> {
        vec![
            Pattern { id: 1, name: "Meeting".into(), tag: "meeting".into(), offsets: vec![-12, -2], ring: false },
            Pattern { id: 2, name: "Flight".into(), tag: "flight".into(), offsets: vec![-1440, -180], ring: true },
            Pattern { id: 3, name: "Morning".into(), tag: "morning".into(), offsets: vec![540], ring: false },
            Pattern { id: 4, name: "Silent".into(), tag: "silent".into(), offsets: vec![], ring: false },
        ]
    }

    fn event(summary: &str, dtstart: &str, alarms: &[&str]) -> Component {
        let mut text = format!("BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:u1\n{dtstart}\nSUMMARY:{summary}\n");
        for a in alarms {
            text.push_str(&format!("BEGIN:VALARM\nACTION:DISPLAY\nTRIGGER:{a}\nEND:VALARM\n"));
        }
        text.push_str("END:VEVENT\nEND:VCALENDAR\n");
        ical::parse(&text).unwrap().remove(0)
    }

    fn triggers(cal: &Component) -> Vec<String> {
        alarms(&cal.children[0]).map(|a| a.value("TRIGGER").unwrap().to_string()).collect()
    }

    fn rules(patterns: &[Pattern], choice: Option<i64>) -> Rules<'_> {
        Rules { patterns, default_timed: Some(1), default_allday: Some(3), choice }
    }

    #[test]
    fn default_applies_and_is_stable() {
        let p = patterns();
        let r = rules(&p, None);
        let mut cal = event("Standup", "DTSTART:20261023T080000Z", &[]);
        assert!(r.apply(&mut cal));
        assert_eq!(triggers(&cal), ["-PT12M", "-PT2M"]);
        assert_eq!(r.source(&cal), Source::Default("Meeting".into()));
        assert!(!r.apply(&mut cal), "applying twice changes nothing");

        let mut day = event("Birthday", "DTSTART;VALUE=DATE:20261023", &[]);
        assert!(r.apply(&mut day));
        assert_eq!(triggers(&day), ["PT9H"]);
    }

    #[test]
    fn manual_alarms_are_kept() {
        let p = patterns();
        let r = rules(&p, None);
        let mut cal = event("Dentist", "DTSTART:20261023T080000Z", &["-PT1H"]);
        assert!(!r.apply(&mut cal));
        assert_eq!(triggers(&cal), ["-PT1H"]);
        assert_eq!(r.source(&cal), Source::Manual);
    }

    #[test]
    fn app_defaults_are_replaced() {
        let p = patterns();
        let r = rules(&p, None);
        let text = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:u\nDTSTART:20261023T080000Z\nSUMMARY:x\nBEGIN:VALARM\nACTION:DISPLAY\nTRIGGER:-PT30M\nX-APPLE-DEFAULT-ALARM:TRUE\nEND:VALARM\nEND:VEVENT\nEND:VCALENDAR\n";
        let mut cal = ical::parse(text).unwrap().remove(0);
        assert!(r.apply(&mut cal));
        assert_eq!(triggers(&cal), ["-PT12M", "-PT2M"]);
    }

    #[test]
    fn unmarked_copy_of_a_pattern_is_still_valet_s() {
        let p = patterns();
        let r = Rules { default_timed: Some(2), ..rules(&p, None) };
        // An app re-saved Meeting's alarms without the mark; the default is now Flight.
        let mut cal = event("Sync", "DTSTART:20261023T080000Z", &["-PT12M", "-PT2M"]);
        assert!(r.apply(&mut cal));
        assert_eq!(triggers(&cal), ["-P1D", "-PT3H"]);
    }

    #[test]
    fn tag_beats_choice_beats_manual() {
        let p = patterns();
        let mut cal = event("Lyon #Flight", "DTSTART:20261023T080000Z", &["-PT1H"]);
        let r = rules(&p, Some(4));
        assert!(r.apply(&mut cal));
        assert_eq!(triggers(&cal), ["-P1D", "-PT3H"]);
        assert_eq!(r.source(&cal), Source::Tag("Flight".into()));

        let mut cal = event("Lyon", "DTSTART:20261023T080000Z", &["-PT1H"]);
        assert!(r.apply(&mut cal));
        assert!(triggers(&cal).is_empty(), "the chosen Silent pattern removes the manual alarm");
        assert_eq!(r.source(&cal), Source::Chosen("Silent".into()));
    }

    #[test]
    fn no_default_removes_valet_alarms_only() {
        let p = patterns();
        let r = Rules { default_timed: None, ..rules(&p, None) };
        let mut cal = event("Standup", "DTSTART:20261023T080000Z", &[]);
        assert!(!r.apply(&mut cal));
        assert!(rules(&p, None).apply(&mut cal));
        assert!(r.apply(&mut cal));
        assert!(triggers(&cal).is_empty());
    }

    #[test]
    fn tag_scanning() {
        assert_eq!(tags("Lyon #flight, then #taxi!").collect::<Vec<_>>(), ["flight", "taxi"]);
        assert_eq!(tags("C# meetup").count(), 0);
    }

    #[test]
    fn specs() {
        assert_eq!(parse_spec("12m, 2m"), Ok(vec![-12, -2]));
        assert_eq!(parse_spec("1d 3h, 1h"), Ok(vec![-1620, -60]));
        assert_eq!(parse_spec("9:00"), Ok(vec![540]));
        assert_eq!(parse_spec("1d 18:00"), Ok(vec![-360]));
        assert_eq!(parse_spec("+30m, 0m"), Ok(vec![0, 30]));
        assert_eq!(parse_spec(""), Ok(vec![]));
        assert!(parse_spec("12 apples").is_err());
        assert!(parse_spec("25:00").is_err());
        assert_eq!(describe_offset(-12), "12 min before");
        assert_eq!(describe_offset(-1620), "1 d 3 h before");
        assert_eq!(describe_offset(30), "30 min after");
        assert_eq!(describe_all_day(540), "09:00 the same day");
        assert_eq!(describe_all_day(-360), "18:00 the day before");
    }
}
