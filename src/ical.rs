//! Just enough iCalendar (RFC 5545) to rewrite alarms without losing anything:
//! a component tree whose properties keep their parameters and values verbatim.

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub struct Component {
    pub name: String,
    pub props: Vec<Property>,
    pub children: Vec<Component>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Property {
    /// Upper-cased.
    pub name: String,
    /// Raw, including each leading `;` (`;TZID=Europe/Paris`), or empty.
    pub params: String,
    pub value: String,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Component { name: name.to_string(), props: Vec::new(), children: Vec::new() }
    }

    pub fn prop(&self, name: &str) -> Option<&Property> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.prop(name).map(|p| p.value.as_str())
    }

    pub fn push(&mut self, name: &str, value: &str) {
        self.props.push(Property { name: name.to_string(), params: String::new(), value: value.to_string() });
    }

    pub fn to_ics(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        fold(out, &format!("BEGIN:{}", self.name));
        for p in &self.props {
            fold(out, &format!("{}{}:{}", p.name, p.params, p.value));
        }
        for c in &self.children {
            c.write(out);
        }
        fold(out, &format!("END:{}", self.name));
    }
}

/// The address of a calendar user (`mailto:a@b.c`).
pub fn mailto(v: &str) -> String {
    let v = v.trim();
    v.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).map(|_| &v[7..]).unwrap_or(v).to_string()
}

impl Property {
    /// The value of parameter `name` (case-insensitive), unquoted.
    pub fn param(&self, name: &str) -> Option<&str> {
        split_params(&self.params).into_iter().find_map(|(k, v)| k.eq_ignore_ascii_case(name).then_some(v))
    }
}

fn split_params(params: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = params;
    while let Some(stripped) = rest.strip_prefix(';') {
        let end = scan_unquoted(stripped, b';').unwrap_or(stripped.len());
        let item = &stripped[..end];
        if let Some((k, v)) = item.split_once('=') {
            out.push((k, v.trim_matches('"')));
        }
        rest = &stripped[end..];
    }
    out
}

/// Index of the first `byte` not inside double quotes.
fn scan_unquoted(s: &str, byte: u8) -> Option<usize> {
    let mut quoted = false;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b if b == byte && !quoted => return Some(i),
            _ => {}
        }
    }
    None
}

/// Lines longer than 75 octets are folded, never inside a UTF-8 character.
fn fold(out: &mut String, line: &str) {
    let mut start = 0;
    let mut limit = 75;
    while line.len() - start > limit {
        let mut end = start + limit;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n ");
        start = end;
        limit = 74;
    }
    out.push_str(&line[start..]);
    out.push_str("\r\n");
}

/// Parses every top-level component in `text` (usually one VCALENDAR).
pub fn parse(text: &str) -> Result<Vec<Component>> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(cont) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t'))
            && let Some(last) = lines.last_mut() {
                last.push_str(cont);
                continue;
            }
        if !raw.trim().is_empty() {
            lines.push(raw.to_string());
        }
    }

    let mut stack: Vec<Component> = Vec::new();
    let mut done = Vec::new();
    for line in lines {
        let colon = scan_unquoted(&line, b':').ok_or_else(|| anyhow::anyhow!("line without ':': {line}"))?;
        let head = &line[..colon];
        let value = &line[colon + 1..];
        let name_end = head.find(';').unwrap_or(head.len());
        let name = head[..name_end].to_ascii_uppercase();
        let params = head[name_end..].to_string();
        match name.as_str() {
            "BEGIN" => stack.push(Component::new(&value.to_ascii_uppercase())),
            "END" => {
                let Some(c) = stack.pop() else { bail!("END:{value} without BEGIN") };
                if !c.name.eq_ignore_ascii_case(value) {
                    bail!("END:{value} closes {}", c.name);
                }
                match stack.last_mut() {
                    Some(parent) => parent.children.push(c),
                    None => done.push(c),
                }
            }
            _ => match stack.last_mut() {
                Some(c) => c.props.push(Property { name, params, value: value.to_string() }),
                None => bail!("property outside a component: {name}"),
            },
        }
    }
    if let Some(c) = stack.last() {
        bail!("{} is never closed", c.name);
    }
    Ok(done)
}

/// Seconds in an RFC 5545 duration (`-PT12M`, `P1DT3H`, `-P1W`).
pub fn parse_duration(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'-' => (-1, &s[1..]),
        b'+' => (1, &s[1..]),
        _ => (1, s),
    };
    let rest = rest.strip_prefix('P')?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    let mut any = false;
    for ch in rest.chars() {
        match ch {
            'T' => in_time = true,
            '0'..='9' => num.push(ch),
            _ => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match (ch, in_time) {
                    ('W', false) => 7 * 86400,
                    ('D', false) => 86400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
                any = true;
            }
        }
    }
    (any && num.is_empty()).then_some(sign * total)
}

/// `-PT12M`, `-P1DT3H`, `PT9H` for a number of minutes.
pub fn format_duration(minutes: i64) -> String {
    let sign = if minutes < 0 { "-" } else { "" };
    let m = minutes.abs();
    let (d, h, mm) = (m / 1440, m % 1440 / 60, m % 60);
    let mut s = format!("{sign}P");
    if d > 0 {
        s.push_str(&format!("{d}D"));
    }
    if h > 0 || mm > 0 || d == 0 {
        s.push('T');
        if h > 0 {
            s.push_str(&format!("{h}H"));
        }
        if mm > 0 || h == 0 {
            s.push_str(&format!("{mm}M"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc\r\nDTSTART;TZID=\"Europe/Paris\":20261023T100000\r\nSUMMARY:A very long summary that goes on and on and on so that it must be fol\r\n ded somewhere é\r\nATTENDEE;CN=\"Doe: J\";PARTSTAT=ACCEPTED:mailto:j@x.fr\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn round_trip() {
        let cals = parse(SAMPLE).unwrap();
        assert_eq!(cals.len(), 1);
        let ev = &cals[0].children[0];
        assert_eq!(ev.value("UID"), Some("abc"));
        assert!(ev.value("SUMMARY").unwrap().ends_with("folded somewhere é"));
        let att = ev.prop("ATTENDEE").unwrap();
        assert_eq!(att.param("cn"), Some("Doe: J"));
        assert_eq!(att.value, "mailto:j@x.fr");
        assert_eq!(mailto(" MAILTO:a@b.c"), "a@b.c");
        assert_eq!(ev.prop("DTSTART").unwrap().param("TZID"), Some("Europe/Paris"));
        let again = parse(&cals[0].to_ics()).unwrap();
        assert_eq!(again, cals);
        assert!(cals[0].to_ics().lines().all(|l| l.len() <= 75));
    }

    #[test]
    fn bad_nesting() {
        assert!(parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nEND:VCALENDAR\n").is_err());
        assert!(parse("BEGIN:VCALENDAR\n").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("-PT12M"), Some(-720));
        assert_eq!(parse_duration("P1DT3H"), Some(97200));
        assert_eq!(parse_duration("-P1W"), Some(-604800));
        assert_eq!(parse_duration("PT0S"), Some(0));
        assert_eq!(parse_duration("P1H"), None);
        assert_eq!(parse_duration("20261023T100000Z"), None);
        for m in [-12, -2, -1620, 540, 0, -1440, -61] {
            assert_eq!(parse_duration(&format_duration(m)), Some(m * 60), "{m}");
        }
        assert_eq!(format_duration(-12), "-PT12M");
        assert_eq!(format_duration(-1620), "-P1DT3H");
        assert_eq!(format_duration(-1440), "-P1D");
        assert_eq!(format_duration(0), "PT0M");
    }
}
