//! The MIME structure of a message, with byte offsets, for IMAP: ENVELOPE,
//! BODYSTRUCTURE and BODY[section] all need the raw bytes, not decoded text.

use std::fmt::Write;

#[derive(Debug)]
pub struct Part {
    /// Start of this part's header.
    pub start: usize,
    pub body_start: usize,
    pub end: usize,
    /// (name, start, end) of each header field, end including its line break.
    pub fields: Vec<(String, usize, usize)>,
    pub mtype: String,
    pub subtype: String,
    pub params: Vec<(String, String)>,
    pub children: Vec<Part>,
    /// For message/rfc822: the enclosed message.
    pub message: Option<Box<Part>>,
}

impl Part {
    pub fn header(&self, raw: &[u8], name: &str) -> Option<String> {
        let (_, s, e) = self.fields.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(name))?;
        let line = String::from_utf8_lossy(&raw[*s..*e]);
        let value = line.split_once(':').map(|(_, v)| v).unwrap_or("");
        Some(unfold(value))
    }

    fn is_multipart(&self) -> bool {
        self.mtype == "multipart"
    }
}

fn unfold(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for (i, line) in v.split('\n').enumerate() {
        let line = line.trim_end_matches('\r');
        if i == 0 {
            out.push_str(line);
        } else if !line.is_empty() {
            out.push(' ');
            out.push_str(line.trim_start());
        }
    }
    out.trim().to_string()
}

pub fn parse(raw: &[u8]) -> Part {
    parse_part(raw, 0, raw.len(), ("text", "plain"), 0)
}

/// Offsets of each line start in `raw[start..end]`.
fn next_line(raw: &[u8], pos: usize, end: usize) -> usize {
    match raw[pos..end].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i + 1,
        None => end,
    }
}

fn parse_part(raw: &[u8], start: usize, end: usize, default: (&str, &str), depth: usize) -> Part {
    let mut fields: Vec<(String, usize, usize)> = Vec::new();
    let mut pos = start;
    let mut body_start = end;
    while pos < end {
        let line_end = next_line(raw, pos, end);
        let line = &raw[pos..line_end];
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.is_empty() {
            body_start = line_end;
            break;
        }
        if (line[0] == b' ' || line[0] == b'\t') && !fields.is_empty() {
            fields.last_mut().unwrap().2 = line_end;
        } else if let Some(colon) = trimmed.iter().position(|&b| b == b':') {
            let name = String::from_utf8_lossy(&trimmed[..colon]).trim().to_string();
            fields.push((name, pos, line_end));
        }
        pos = line_end;
    }
    if pos >= end && body_start == end && start == end {
        body_start = end;
    }
    let mut part = Part {
        start,
        body_start,
        end,
        fields,
        mtype: default.0.into(),
        subtype: default.1.into(),
        params: Vec::new(),
        children: Vec::new(),
        message: None,
    };
    if let Some(ct) = part.header(raw, "Content-Type") {
        let (t, params) = parse_params(&ct);
        if let Some((a, b)) = t.split_once('/') {
            part.mtype = a.trim().to_ascii_lowercase();
            part.subtype = b.trim().to_ascii_lowercase();
        }
        part.params = params;
    }
    if depth > 20 {
        return part;
    }
    if part.is_multipart() {
        let boundary = part.params.iter().find(|(k, _)| k.eq_ignore_ascii_case("boundary")).map(|(_, v)| v.clone());
        if let Some(b) = boundary {
            let child_default = if part.subtype == "digest" { ("message", "rfc822") } else { ("text", "plain") };
            for (s, e) in split_multipart(raw, body_start, end, &b) {
                part.children.push(parse_part(raw, s, e, child_default, depth + 1));
            }
        }
    } else if part.mtype == "message" && (part.subtype == "rfc822" || part.subtype == "global") {
        part.message = Some(Box::new(parse_part(raw, body_start, end, ("text", "plain"), depth + 1)));
    }
    part
}

/// The (start, end) of each body part between `--boundary` lines.
fn split_multipart(raw: &[u8], start: usize, end: usize, boundary: &str) -> Vec<(usize, usize)> {
    let delim = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut current: Option<usize> = None;
    let mut pos = start;
    while pos < end {
        let line_end = next_line(raw, pos, end);
        let line = &raw[pos..line_end];
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        let text: &[u8] = {
            let mut t = text;
            while let Some(s) = t.strip_suffix(b" ").or_else(|| t.strip_suffix(b"\t")) {
                t = s;
            }
            t
        };
        if text.starts_with(delim.as_bytes()) {
            let rest = &text[delim.len()..];
            if rest.is_empty() || rest == b"--" {
                if let Some(s) = current {
                    // The line break before a delimiter belongs to the delimiter.
                    let mut e = pos;
                    if e > s && raw[e - 1] == b'\n' {
                        e -= 1;
                        if e > s && raw[e - 1] == b'\r' {
                            e -= 1;
                        }
                    }
                    parts.push((s, e.max(s)));
                }
                if rest == b"--" {
                    return parts;
                }
                current = Some(line_end);
            }
        }
        pos = line_end;
    }
    if let Some(s) = current {
        parts.push((s, end));
    }
    parts
}

/// `text/plain; charset="utf-8"` → ("text/plain", [("charset", "utf-8")]).
pub fn parse_params(v: &str) -> (String, Vec<(String, String)>) {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in v.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => items.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    items.push(cur);
    let mut it = items.into_iter();
    let first = it.next().unwrap_or_default().trim().to_string();
    let params = it
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect();
    (first, params)
}

// IMAP strings

/// A quoted string, or a literal when it holds bytes a quoted string can't.
pub fn istr(s: &str) -> String {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        format!("{{{}}}\r\n{s}", s.len())
    }
}

pub fn nstr(s: Option<&str>) -> String {
    match s {
        Some(s) => istr(s),
        None => "NIL".into(),
    }
}

// ENVELOPE

/// (name, mailbox, host) of each address in a header value; names are kept
/// raw (encoded words and all), clients decode them.
pub fn addresses(v: &str) -> Vec<(Option<String>, String, String)> {
    let mut out = Vec::new();
    let mut items = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut angle, mut paren) = (false, false, 0);
    for c in v.chars() {
        match c {
            '"' if paren == 0 => quoted = !quoted,
            '<' if !quoted && paren == 0 => angle = true,
            '>' if !quoted && paren == 0 => angle = false,
            '(' if !quoted => paren += 1,
            ')' if !quoted && paren > 0 => paren -= 1,
            _ => {}
        }
        if (c == ',' || c == ';') && !quoted && !angle && paren == 0 {
            items.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    items.push(cur);
    for item in items {
        // A group's name ("undisclosed-recipients:") carries no address.
        let item = match item.find(':') {
            Some(i) if !item[..i].contains('<') && !item[..i].contains('@') && !item[..i].contains('"') => item[i + 1..].to_string(),
            _ => item,
        };
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, addr) = match (item.rfind('<'), item.rfind('>')) {
            (Some(a), Some(b)) if a < b => {
                let name = item[..a].trim().trim_matches('"').trim().to_string();
                (if name.is_empty() { None } else { Some(name) }, item[a + 1..b].trim().to_string())
            }
            _ => {
                // `addr (Comment Name)`
                let (addr, comment) = match item.find('(') {
                    Some(i) => (item[..i].trim(), Some(item[i + 1..].trim_end_matches(')').trim().to_string())),
                    None => (item, None),
                };
                (comment.filter(|c| !c.is_empty()), addr.to_string())
            }
        };
        let (mailbox, host) = match addr.rsplit_once('@') {
            Some((m, h)) => (m.to_string(), h.to_string()),
            None => (addr.clone(), String::new()),
        };
        if mailbox.is_empty() && host.is_empty() {
            continue;
        }
        out.push((name, mailbox, host));
    }
    out
}

fn address_list(v: Option<String>) -> String {
    let Some(v) = v else { return "NIL".into() };
    let list = addresses(&v);
    if list.is_empty() {
        return "NIL".into();
    }
    let mut s = String::from("(");
    for (name, mailbox, host) in list {
        let _ = write!(s, "({} NIL {} {})", nstr(name.as_deref()), istr(&mailbox), if host.is_empty() { "NIL".into() } else { istr(&host) });
    }
    s.push(')');
    s
}

pub fn envelope(raw: &[u8], p: &Part) -> String {
    let h = |n: &str| p.header(raw, n);
    let from = h("From");
    let sender = h("Sender").or_else(|| from.clone());
    let reply_to = h("Reply-To").or_else(|| from.clone());
    format!(
        "({} {} {} {} {} {} {} {} {} {})",
        nstr(h("Date").as_deref()),
        nstr(h("Subject").as_deref()),
        address_list(from),
        address_list(sender),
        address_list(reply_to),
        address_list(h("To")),
        address_list(h("Cc")),
        address_list(h("Bcc")),
        nstr(h("In-Reply-To").as_deref()),
        nstr(h("Message-ID").as_deref()),
    )
}

// BODYSTRUCTURE

fn param_list(params: &[(String, String)]) -> String {
    if params.is_empty() {
        return "NIL".into();
    }
    let items: Vec<String> = params.iter().map(|(k, v)| format!("{} {}", istr(k), istr(v))).collect();
    format!("({})", items.join(" "))
}

fn disposition(raw: &[u8], p: &Part) -> String {
    match p.header(raw, "Content-Disposition") {
        Some(d) => {
            let (kind, params) = parse_params(&d);
            format!("({} {})", istr(&kind.to_ascii_lowercase()), param_list(&params))
        }
        None => "NIL".into(),
    }
}

/// BODYSTRUCTURE (`extended`) or BODY.
pub fn bodystructure(raw: &[u8], p: &Part, extended: bool) -> String {
    if p.is_multipart() {
        let mut s = String::from("(");
        if p.children.is_empty() {
            s.push_str("(\"text\" \"plain\" NIL NIL NIL \"7bit\" 0 0)");
        }
        for c in &p.children {
            s.push_str(&bodystructure(raw, c, extended));
        }
        let _ = write!(s, " {}", istr(&p.subtype));
        if extended {
            let _ = write!(s, " {} {} NIL NIL", param_list(&p.params), disposition(raw, p));
        }
        s.push(')');
        return s;
    }
    let body = &raw[p.body_start.min(p.end)..p.end];
    let encoding = p.header(raw, "Content-Transfer-Encoding").unwrap_or_else(|| "7bit".into()).to_ascii_lowercase();
    let mut s = format!(
        "({} {} {} {} {} {} {}",
        istr(&p.mtype),
        istr(&p.subtype),
        param_list(&p.params),
        nstr(p.header(raw, "Content-ID").as_deref()),
        nstr(p.header(raw, "Content-Description").as_deref()),
        istr(&encoding),
        body.len()
    );
    let lines = body.iter().filter(|&&b| b == b'\n').count();
    if let Some(m) = &p.message {
        let _ = write!(s, " {} {} {lines}", envelope(raw, m), bodystructure(raw, m, extended));
    } else if p.mtype == "text" {
        let _ = write!(s, " {lines}");
    }
    if extended {
        let _ = write!(s, " NIL {} NIL NIL", disposition(raw, p));
    }
    s.push(')');
    s
}

// BODY[section]

fn nav(p: &Part, n: usize) -> Option<&Part> {
    if let Some(m) = &p.message {
        return nav(m, n);
    }
    if p.is_multipart() {
        p.children.get(n.checked_sub(1)?)
    } else if n == 1 {
        Some(p)
    } else {
        None
    }
}

/// The bytes of `section` (`""`, `HEADER`, `1.2`, `2.HEADER.FIELDS (From To)`, `1.MIME`…).
pub fn section(raw: &[u8], root: &Part, section: &str) -> Option<Vec<u8>> {
    let mut rest = section.trim();
    let mut part = root;
    let mut numbered = false;
    loop {
        let end = rest.find('.').unwrap_or(rest.len());
        let Ok(n) = rest[..end].parse::<usize>() else { break };
        part = nav(part, n)?;
        numbered = true;
        rest = rest.get(end + 1..).unwrap_or("");
    }
    let upper = rest.to_ascii_uppercase();
    // HEADER and TEXT of a numbered part refer to its enclosed message.
    let message = if numbered { part.message.as_deref() } else { Some(root) };
    if upper.is_empty() {
        return Some(if numbered { raw[part.body_start.min(part.end)..part.end].to_vec() } else { raw.to_vec() });
    }
    if upper == "MIME" {
        return numbered.then(|| raw[part.start..part.body_start.min(part.end)].to_vec());
    }
    let m = message?;
    if upper == "TEXT" {
        return Some(raw[m.body_start.min(m.end)..m.end].to_vec());
    }
    if upper == "HEADER" {
        return Some(raw[m.start..m.body_start.min(m.end)].to_vec());
    }
    let (not, list) = if let Some(l) = upper.strip_prefix("HEADER.FIELDS.NOT") {
        (true, l)
    } else {
        let l = upper.strip_prefix("HEADER.FIELDS")?;
        (false, l)
    };
    let names: Vec<String> = list.trim().trim_start_matches('(').trim_end_matches(')').split_whitespace().map(|s| s.trim_matches('"').to_string()).collect();
    let mut out = Vec::new();
    for (name, s, e) in &m.fields {
        let wanted = names.iter().any(|n| n.eq_ignore_ascii_case(name));
        if wanted != not {
            out.extend_from_slice(&raw[*s..*e]);
            if !out.ends_with(b"\n") {
                out.extend_from_slice(b"\r\n");
            }
        }
    }
    out.extend_from_slice(b"\r\n");
    Some(out)
}

/// The field a removed attachment keeps in its header, with its size.
pub const REMOVED: &str = "X-Valet-Removed";

/// What a message's attachments weigh: how many there are, their size once
/// decoded (what they cost stored compressed), and how many were removed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Attachments {
    pub count: u32,
    pub bytes: u64,
    pub removed: u32,
}

/// The parts a message's attachments are: the files and pictures, not its
/// text, nor an invitation's calendar, nor what was removed already. In a
/// message attached, its own attachments, not its text.
fn attachment_parts<'a>(raw: &[u8], p: &'a Part, out: &mut Vec<&'a Part>, removed: &mut u32) {
    if p.is_multipart() {
        for c in &p.children {
            attachment_parts(raw, c, out, removed);
        }
        return;
    }
    if let Some(m) = &p.message {
        attachment_parts(raw, m, out, removed);
        return;
    }
    if p.header(raw, REMOVED).is_some() {
        *removed += 1;
        return;
    }
    let attached = p.header(raw, "Content-Disposition").is_some_and(|d| parse_params(&d).0.eq_ignore_ascii_case("attachment"));
    let text = p.mtype == "text" && matches!(p.subtype.as_str(), "plain" | "html" | "enriched") && !attached;
    let calendar = (p.mtype == "text" && p.subtype == "calendar") || (p.mtype == "application" && p.subtype == "ics");
    if text || calendar || p.mtype == "message" || p.body_start >= p.end {
        return;
    }
    out.push(p);
}

/// About the size of a part's content once decoded.
fn decoded_size(raw: &[u8], p: &Part) -> u64 {
    let n = (p.end - p.body_start.min(p.end)) as u64;
    let base64 = p.header(raw, "Content-Transfer-Encoding").is_some_and(|e| e.trim().eq_ignore_ascii_case("base64"));
    if base64 { n * 3 / 4 } else { n }
}

pub fn attachments(raw: &[u8]) -> Attachments {
    let root = parse(raw);
    let (mut parts, mut removed) = (Vec::new(), 0);
    attachment_parts(raw, &root, &mut parts, &mut removed);
    Attachments { count: parts.len() as u32, bytes: parts.iter().map(|p| decoded_size(raw, p)).sum(), removed }
}

/// The message without its attachments' content (None if it has none):
/// each keeps its header (name, type), with `X-Valet-Removed: <size>`, and
/// an empty body. The structure stays as it was.
pub fn strip(raw: &[u8]) -> Option<Vec<u8>> {
    let root = parse(raw);
    let (mut parts, mut removed) = (Vec::new(), 0);
    attachment_parts(raw, &root, &mut parts, &mut removed);
    if parts.is_empty() {
        return None;
    }
    parts.sort_by_key(|p| p.start);
    let mut out = Vec::with_capacity(raw.len() / 4);
    let mut at = 0;
    for p in parts {
        // At the end of its header, before the empty line.
        let header_end = p.fields.last().map_or(p.start, |f| f.2);
        out.extend_from_slice(&raw[at..header_end]);
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("{REMOVED}: {}\r\n", decoded_size(raw, p)).as_bytes());
        out.extend_from_slice(&raw[header_end..p.body_start.min(p.end)]);
        at = p.end;
    }
    out.extend_from_slice(&raw[at..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &str = "From: \"Doe, J\" <j@x.fr>\r\nTo: a@b.c, =?utf-8?q?R=C3=A9mi?= <r@b.c>\r\nSubject: Hi\r\n there\r\nMessage-ID: <1@x>\r\nContent-Type: multipart/mixed; boundary=\"XX\"\r\n\r\npreamble\r\n--XX\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello\r\nworld\r\n--XX\r\nContent-Type: message/rfc822\r\n\r\nSubject: inner\r\n\r\ninner body\r\n--XX\r\nContent-Type: application/pdf; name=a.pdf\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nAAAA\r\n--XX--\r\nepilogue\r\n";

    #[test]
    fn structure_and_sections() {
        let raw = MSG.as_bytes();
        let p = parse(raw);
        assert_eq!((p.mtype.as_str(), p.subtype.as_str()), ("multipart", "mixed"));
        assert_eq!(p.children.len(), 3);
        assert_eq!(p.header(raw, "subject").as_deref(), Some("Hi there"));
        assert_eq!(section(raw, &p, "1").unwrap(), b"Hello\r\nworld");
        assert_eq!(section(raw, &p, "2.HEADER").unwrap(), b"Subject: inner\r\n\r\n");
        assert_eq!(section(raw, &p, "2.TEXT").unwrap(), b"inner body");
        assert_eq!(section(raw, &p, "3").unwrap(), b"AAAA");
        assert!(section(raw, &p, "3.MIME").unwrap().starts_with(b"Content-Type: application/pdf"));
        assert_eq!(section(raw, &p, "HEADER.FIELDS (SUBJECT MESSAGE-ID)").unwrap(), b"Subject: Hi\r\n there\r\nMessage-ID: <1@x>\r\n\r\n");
        assert_eq!(section(raw, &p, "").unwrap(), raw);
        assert!(section(raw, &p, "4").is_none());
        let bs = bodystructure(raw, &p, true);
        assert!(bs.starts_with("((\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL)"), "{bs}");
        assert!(bs.contains("(\"attachment\" (\"filename\" \"a.pdf\"))"), "{bs}");
        assert!(bs.ends_with(" \"mixed\" (\"boundary\" \"XX\") NIL NIL NIL)"), "{bs}");
        let env = envelope(raw, &p);
        assert!(env.contains("((\"Doe, J\" NIL \"j\" \"x.fr\"))"), "{env}");
        assert!(env.contains("((NIL NIL \"a\" \"b.c\")(\"=?utf-8?q?R=C3=A9mi?=\" NIL \"r\" \"b.c\"))"), "{env}");
    }

    #[test]
    fn single_part_and_no_body() {
        let raw = b"Subject: x\n\nbody\n";
        let p = parse(raw);
        assert_eq!(section(raw, &p, "1").unwrap(), b"body\n");
        assert_eq!(section(raw, &p, "TEXT").unwrap(), b"body\n");
        let raw = b"Subject: x\r\n";
        let p = parse(raw);
        assert_eq!(section(raw, &p, "TEXT").unwrap(), b"");
    }

    #[test]
    fn address_forms() {
        assert_eq!(addresses("a@b.c (Al)"), [(Some("Al".into()), "a".into(), "b.c".into())]);
        assert_eq!(addresses("undisclosed-recipients:;"), []);
        assert_eq!(addresses("Team: x@y.z;").len(), 1);
        assert_eq!(istr("é"), "{2}\r\né");
        assert_eq!(istr("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn attachments_removed() {
        let raw = MSG.as_bytes();
        assert_eq!(attachments(raw), Attachments { count: 1, bytes: 3, removed: 0 });
        let slim = strip(raw).unwrap();
        let text = String::from_utf8(slim.clone()).unwrap();
        assert!(text.contains("Content-Transfer-Encoding: base64\r\nX-Valet-Removed: 3\r\n\r\n\r\n--XX--"), "{text}");
        assert!(text.contains("Hello\r\nworld") && text.contains("inner body"), "the text stays");
        assert_eq!(attachments(&slim), Attachments { count: 0, bytes: 0, removed: 1 });
        assert_eq!(strip(&slim), None, "nothing more to remove");
        let p = parse(&slim);
        assert_eq!(p.children.len(), 3, "same structure");
        assert_eq!(section(&slim, &p, "3").unwrap(), b"");
        // An invitation's calendar stays; a file alone, the whole message, goes.
        let invite = b"Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\n--b\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\n\r\niVBO\r\n--b--\r\n";
        assert_eq!(attachments(invite).count, 1);
        assert!(String::from_utf8(strip(invite).unwrap()).unwrap().contains("BEGIN:VCALENDAR"));
        let lone = b"Subject: scan\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4 ...";
        assert_eq!(strip(lone).unwrap(), b"Subject: scan\r\nContent-Type: application/pdf\r\nX-Valet-Removed: 12\r\n\r\n");
        let attached_text = b"Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b\r\nContent-Type: text/plain; name=notes.txt\r\nContent-Disposition: attachment; filename=notes.txt\r\n\r\nnotes\r\n--b--\r\n";
        assert_eq!(attachments(attached_text).count, 1, "a text file attached is an attachment");
        assert_eq!(attachments(b"Subject: x\r\n\r\njust text"), Attachments::default());
    }
}
