//! A namespace-resolved element tree for WebDAV request bodies, and escaping
//! for the responses (which are written as strings).

use anyhow::{Result, bail};
use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const CS: &str = "http://calendarserver.org/ns/";
pub const APPLE: &str = "http://apple.com/ns/ical/";

#[derive(Debug, Default)]
pub struct Element {
    pub ns: String,
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    pub fn child(&self, ns: &str, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.is(ns, name))
    }

    pub fn all<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.is(ns, name))
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Every descendant (depth first) with this name.
    pub fn find<'a>(&'a self, ns: &'a str, name: &'a str, out: &mut Vec<&'a Element>) {
        for c in &self.children {
            if c.is(ns, name) {
                out.push(c);
            }
            c.find(ns, name, out);
        }
    }
}

pub fn parse(text: &str) -> Result<Element> {
    let mut reader = NsReader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<Element> = vec![Element::default()];
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let ns = match ns {
            ResolveResult::Bound(n) => n.as_ref().to_string(),
            _ => String::new(),
        };
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let mut el = Element {
                    ns,
                    name: e.local_name().as_ref().to_string(),
                    ..Default::default()
                };
                for a in e.attributes().flatten() {
                    let key = a.key.local_name().as_ref().to_string();
                    el.attrs.push((key, a.normalized_value(XmlVersion::Implicit1_0)?.into_owned()));
                }
                if matches!(event, Event::Empty(_)) {
                    stack.last_mut().unwrap().children.push(el);
                } else {
                    stack.push(el);
                }
            }
            Event::End(_) => {
                let el = stack.pop().unwrap();
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => bail!("unbalanced XML"),
                }
            }
            Event::Text(t) => stack.last_mut().unwrap().text.push_str(&t.xml10_content()),
            Event::CData(t) => stack.last_mut().unwrap().text.push_str(&t),
            Event::GeneralRef(r) => {
                let s: &str = &r;
                let ch = match s {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => s.strip_prefix("#x").map_or_else(
                        || s.strip_prefix('#').and_then(|n| n.parse().ok()),
                        |h| u32::from_str_radix(h, 16).ok(),
                    )
                    .and_then(char::from_u32),
                };
                if let Some(ch) = ch {
                    stack.last_mut().unwrap().text.push(ch);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        bail!("unclosed XML element");
    }
    stack.pop().unwrap().children.pop().ok_or_else(|| anyhow::anyhow!("empty XML document"))
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
    out
}

/// The prefix responses use for a namespace (declared on `<D:multistatus>`).
pub fn prefix(ns: &str) -> Option<&'static str> {
    match ns {
        DAV => Some("D"),
        CALDAV => Some("C"),
        CS => Some("CS"),
        APPLE => Some("A"),
        _ => None,
    }
}

pub const NAMESPACES: &str =
    r#"xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:A="http://apple.com/ns/ical/""#;

/// `<P:name>inner</P:name>`, declaring an unknown namespace in place.
pub fn element(ns: &str, name: &str, inner: &str) -> String {
    let (open, close) = match prefix(ns) {
        Some(p) => (format!("{p}:{name}"), format!("{p}:{name}")),
        None if ns.is_empty() => (name.to_string(), name.to_string()),
        None => (format!("X:{name} xmlns:X=\"{}\"", escape(ns)), format!("X:{name}")),
    };
    if inner.is_empty() { format!("<{open}/>") } else { format!("<{open}>{inner}</{close}>") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaces_and_text() {
        let doc = parse(
            r#"<?xml version="1.0"?><c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
              <d:prop><d:getetag/><x:color xmlns:x="urn:other"/></d:prop>
              <d:href>/dav/cal/w/a%20b&amp;c.ics</d:href></c:calendar-multiget>"#,
        )
        .unwrap();
        assert!(doc.is(CALDAV, "calendar-multiget"));
        let prop = doc.child(DAV, "prop").unwrap();
        assert!(prop.children[0].is(DAV, "getetag"));
        assert!(prop.children[1].is("urn:other", "color"));
        assert_eq!(doc.child(DAV, "href").unwrap().text, "/dav/cal/w/a%20b&c.ics");
        assert_eq!(element("urn:other", "color", ""), r#"<X:color xmlns:X="urn:other"/>"#);
    }
}
