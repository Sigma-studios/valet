//! CalDAV (RFC 4791) with sync-collection (RFC 6578): what Apple Calendar,
//! DAVx⁵, Thunderbird and GNOME/Evolution need.
//!
//! /dav/                 root
//! /dav/principal/       the one user
//! /dav/cal/             calendar home
//! /dav/cal/<id>/        a calendar
//! /dav/cal/<id>/<name>  an event

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

use crate::App;
use crate::store::{Calendar, Object, PutError};
use crate::xml::{self, APPLE, CALDAV, CS, DAV, Element, element, escape};

const ROOT: &str = "/dav/";
const PRINCIPAL: &str = "/dav/principal/";
const HOME: &str = "/dav/cal/";
const SYNC_PREFIX: &str = "http://valet/sync/";
const MAX_BODY: usize = 20 * 1024 * 1024;

/// What a path segment keeps unescaped in hrefs.
const SEGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'/').add(b'<').add(b'>').add(b'?')
    .add(b'[').add(b'\\').add(b']').add(b'^').add(b'`').add(b'{').add(b'|').add(b'}');

fn enc(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

#[derive(Debug, PartialEq)]
enum Res {
    Root,
    Principal,
    Home,
    Calendar(String),
    Object(String, String),
}

fn resolve(path: &str) -> Option<Res> {
    let rest = path.strip_prefix("/dav").filter(|r| r.is_empty() || r.starts_with('/'));
    let Some(rest) = rest else {
        // Some clients PROPFIND the site root.
        return (path == "/" || path.is_empty()).then_some(Res::Root);
    };
    let parts: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        .collect();
    match parts.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => Some(Res::Root),
        ["principal"] => Some(Res::Principal),
        ["cal"] => Some(Res::Home),
        ["cal", id] => Some(Res::Calendar(id.to_string())),
        ["cal", id, name] => Some(Res::Object(id.to_string(), name.to_string())),
        _ => None,
    }
}

fn cal_href(id: &str) -> String {
    format!("{HOME}{}/", enc(id))
}

fn obj_href(cal: &str, name: &str) -> String {
    format!("{HOME}{}/{}", enc(cal), enc(name))
}

fn sync_token(rev: i64) -> String {
    format!("{SYNC_PREFIX}{rev}")
}

fn status(code: StatusCode) -> Response<Body> {
    Response::builder().status(code).body(Body::empty()).unwrap()
}

fn xml_response(code: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{body}")))
        .unwrap()
}

fn multistatus(responses: &[String], extra: &str) -> Response<Body> {
    xml_response(
        StatusCode::MULTI_STATUS,
        format!("<D:multistatus {}>{}{extra}</D:multistatus>", xml::NAMESPACES, responses.concat()),
    )
}

/// A precondition failure: `<D:error><C:valid-calendar-data/></D:error>`.
fn precondition(code: StatusCode, ns: &str, name: &str, inner: &str) -> Response<Body> {
    xml_response(code, format!("<D:error {}>{}</D:error>", xml::NAMESPACES, element(ns, name, inner)))
}

pub async fn handle(app: Arc<App>, req: Request<Body>) -> Response<Body> {
    if !app.config.check_basic(req.headers()) {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Basic realm=\"valet\", charset=\"UTF-8\"")
            .body(Body::empty())
            .unwrap();
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let Ok(body) = to_bytes(req.into_body(), MAX_BODY).await else {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    let Some(res) = resolve(&path) else { return status(StatusCode::NOT_FOUND) };
    let dav = Dav { app: &app, headers: &headers };
    let response = match method.as_str() {
        "OPTIONS" => options(),
        "PROPFIND" => dav.propfind(&res, &body),
        "REPORT" => dav.report(&res, &body),
        "PROPPATCH" => dav.proppatch(&res, &path, &body),
        "MKCALENDAR" | "MKCOL" => dav.mkcalendar(&res, &body),
        "GET" | "HEAD" => dav.get(&res, method == Method::HEAD),
        "PUT" => dav.put(&res, &body),
        "DELETE" => dav.delete(&res),
        _ => status(StatusCode::METHOD_NOT_ALLOWED),
    };
    tracing::debug!(%method, %path, status = %response.status(), "dav");
    response
}

fn options() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("DAV", "1, 3, calendar-access, extended-mkcol")
        .header(header::ALLOW, "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCALENDAR, MKCOL")
        .body(Body::empty())
        .unwrap()
}

/// Which properties a PROPFIND or REPORT asks for.
enum Wanted {
    All,
    Names,
    Some(Vec<(String, String)>),
}

fn wanted(root: Option<&Element>) -> Wanted {
    let Some(root) = root else { return Wanted::All };
    if root.child(DAV, "propname").is_some() {
        return Wanted::Names;
    }
    match root.child(DAV, "prop") {
        Some(p) => Wanted::Some(p.children.iter().map(|c| (c.ns.clone(), c.name.clone())).collect()),
        None => Wanted::All,
    }
}

/// Properties returned for `<D:allprop/>` (and an empty PROPFIND).
const ALLPROP: &[(&str, &str)] = &[
    (DAV, "resourcetype"),
    (DAV, "displayname"),
    (DAV, "getetag"),
    (DAV, "getcontenttype"),
    (DAV, "current-user-principal"),
    (DAV, "sync-token"),
    (CS, "getctag"),
    (APPLE, "calendar-color"),
];

const PRIVILEGES: &str = "<D:privilege><D:all/></D:privilege><D:privilege><D:read/></D:privilege>\
    <D:privilege><D:write/></D:privilege><D:privilege><D:write-properties/></D:privilege>\
    <D:privilege><D:write-content/></D:privilege><D:privilege><D:bind/></D:privilege>\
    <D:privilege><D:unbind/></D:privilege><D:privilege><C:read-free-busy/></D:privilege>";

const REPORTS: &str = "<D:supported-report><D:report><C:calendar-multiget/></D:report></D:supported-report>\
    <D:supported-report><D:report><C:calendar-query/></D:report></D:supported-report>\
    <D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report>";

/// A resource as PROPFIND and REPORT see it.
enum Node<'a> {
    Root,
    Principal,
    Home,
    Calendar(&'a Calendar),
    Object(&'a Object),
}

struct Dav<'a> {
    app: &'a App,
    headers: &'a HeaderMap,
}

impl Dav<'_> {
    fn href(&self, node: &Node) -> String {
        match node {
            Node::Root => ROOT.into(),
            Node::Principal => PRINCIPAL.into(),
            Node::Home => HOME.into(),
            Node::Calendar(c) => cal_href(&c.id),
            Node::Object(o) => obj_href(&o.calendar, &o.name),
        }
    }

    /// The inner XML of a property, or `None` if the resource has no such property.
    fn prop(&self, node: &Node, ns: &str, name: &str) -> Option<String> {
        let principal_href = format!("<D:href>{PRINCIPAL}</D:href>");
        let collection = !matches!(node, Node::Object(_));
        Some(match (ns, name, node) {
            (DAV, "resourcetype", Node::Principal) => "<D:collection/><D:principal/>".into(),
            (DAV, "resourcetype", Node::Calendar(_)) => "<D:collection/><C:calendar/>".into(),
            (DAV, "resourcetype", Node::Object(_)) => String::new(),
            (DAV, "resourcetype", _) => "<D:collection/>".into(),
            (DAV, "displayname", Node::Root) => "valet".into(),
            (DAV, "displayname", Node::Principal) => escape(&self.app.config.user),
            (DAV, "displayname", Node::Home) => "Calendars".into(),
            (DAV, "displayname", Node::Calendar(c)) => escape(&c.name),
            (DAV, "current-user-principal", _) | (DAV, "owner", _) | (DAV, "principal-URL", Node::Principal) => {
                principal_href
            }
            (DAV, "principal-collection-set", _) => format!("<D:href>{ROOT}</D:href>"),
            (DAV, "current-user-privilege-set", _) => PRIVILEGES.into(),
            (DAV, "supported-report-set", _) if collection => REPORTS.into(),
            (CALDAV, "calendar-home-set", Node::Principal) => format!("<D:href>{HOME}</D:href>"),
            (CALDAV, "calendar-user-address-set", Node::Principal) => {
                let mut s: String = self
                    .app
                    .addresses()
                    .iter()
                    .map(|a| format!("<D:href>mailto:{}</D:href>", escape(a)))
                    .collect();
                s.push_str(&principal_href);
                s
            }
            (DAV, "sync-token", Node::Calendar(c)) => escape(&sync_token(c.rev)),
            (CS, "getctag", Node::Calendar(c)) => c.rev.to_string(),
            (CALDAV, "supported-calendar-component-set", Node::Calendar(_)) => "<C:comp name=\"VEVENT\"/>".into(),
            (CALDAV, "supported-calendar-data", Node::Calendar(_)) => {
                "<C:calendar-data content-type=\"text/calendar\" version=\"2.0\"/>".into()
            }
            (CALDAV, "max-resource-size", Node::Calendar(_)) => MAX_BODY.to_string(),
            (CALDAV, "calendar-description", Node::Calendar(c)) if !c.description.is_empty() => escape(&c.description),
            (APPLE, "calendar-color", Node::Calendar(c)) => escape(&c.color),
            (APPLE, "calendar-order", Node::Calendar(c)) => c.ord.to_string(),
            (DAV, "getetag", Node::Object(o)) => escape(&o.etag),
            (DAV, "getcontenttype", Node::Object(_)) => "text/calendar; charset=utf-8; component=vevent".into(),
            (DAV, "getcontentlength", Node::Object(o)) => o.data.len().to_string(),
            (CALDAV, "calendar-data", Node::Object(o)) => escape(&o.data),
            _ => return None,
        })
    }

    fn response(&self, node: &Node, wanted: &Wanted) -> String {
        let mut found = String::new();
        let mut missing = String::new();
        match wanted {
            Wanted::Names => {
                for (ns, name) in ALLPROP {
                    if self.prop(node, ns, name).is_some() {
                        found.push_str(&element(ns, name, ""));
                    }
                }
            }
            Wanted::All => {
                for (ns, name) in ALLPROP {
                    if let Some(v) = self.prop(node, ns, name) {
                        found.push_str(&element(ns, name, &v));
                    }
                }
            }
            Wanted::Some(list) => {
                for (ns, name) in list {
                    match self.prop(node, ns, name) {
                        Some(v) => found.push_str(&element(ns, name, &v)),
                        None => missing.push_str(&element(ns, name, "")),
                    }
                }
            }
        }
        let mut out = format!("<D:response><D:href>{}</D:href>", escape(&self.href(node)));
        if !found.is_empty() {
            out.push_str(&format!("<D:propstat><D:prop>{found}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"));
        }
        if !missing.is_empty() {
            out.push_str(&format!(
                "<D:propstat><D:prop>{missing}</D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"
            ));
        }
        out.push_str("</D:response>");
        out
    }

    fn depth(&self) -> u8 {
        match self.headers.get("Depth").and_then(|v| v.to_str().ok()) {
            Some("0") => 0,
            _ => 1,
        }
    }

    fn propfind(&self, res: &Res, body: &str) -> Response<Body> {
        let root = if body.trim().is_empty() {
            None
        } else {
            match xml::parse(body) {
                Ok(r) => Some(r),
                Err(_) => return status(StatusCode::BAD_REQUEST),
            }
        };
        let wanted = wanted(root.as_ref());
        let store = &self.app.store;
        let depth = self.depth();
        let mut out = Vec::new();
        match res {
            Res::Root => {
                out.push(self.response(&Node::Root, &wanted));
                if depth > 0 {
                    out.push(self.response(&Node::Principal, &wanted));
                    out.push(self.response(&Node::Home, &wanted));
                }
            }
            Res::Principal => out.push(self.response(&Node::Principal, &wanted)),
            Res::Home => {
                out.push(self.response(&Node::Home, &wanted));
                if depth > 0 {
                    for c in store.calendars() {
                        out.push(self.response(&Node::Calendar(&c), &wanted));
                    }
                }
            }
            Res::Calendar(id) => {
                let Some(c) = store.calendar(id) else { return status(StatusCode::NOT_FOUND) };
                out.push(self.response(&Node::Calendar(&c), &wanted));
                if depth > 0 {
                    for o in store.objects(id) {
                        out.push(self.response(&Node::Object(&o), &wanted));
                    }
                }
            }
            Res::Object(id, name) => {
                let Some(o) = store.object(id, name) else { return status(StatusCode::NOT_FOUND) };
                out.push(self.response(&Node::Object(&o), &wanted));
            }
        }
        multistatus(&out, "")
    }

    fn report(&self, res: &Res, body: &str) -> Response<Body> {
        let Ok(root) = xml::parse(body) else { return status(StatusCode::BAD_REQUEST) };
        let wanted = wanted(Some(&root));
        let store = &self.app.store;
        let cal_id = match res {
            Res::Calendar(id) | Res::Object(id, _) => Some(id.as_str()),
            _ => None,
        };
        if root.is(CALDAV, "calendar-multiget") {
            let out: Vec<String> = root
                .all(DAV, "href")
                .map(|h| {
                    let href = h.text.trim();
                    let path = href.find("/dav/").map_or(href, |i| &href[i..]);
                    match resolve(path) {
                        Some(Res::Object(c, n)) => match store.object(&c, &n) {
                            Some(o) => self.response(&Node::Object(&o), &wanted),
                            None => not_found(href),
                        },
                        _ => not_found(href),
                    }
                })
                .collect();
            return multistatus(&out, "");
        }
        if root.is(CALDAV, "calendar-query") {
            let Some(id) = cal_id else { return multistatus(&[], "") };
            let mut filters = Vec::new();
            root.find(CALDAV, "comp-filter", &mut filters);
            let other = filters.iter().any(|f| f.attr("name").is_some_and(|n| n != "VCALENDAR" && n != "VEVENT"));
            let objects = if other { Vec::new() } else { store.objects(id) };
            let objects = match res {
                Res::Object(_, name) => objects.into_iter().filter(|o| &o.name == name).collect(),
                _ => objects,
            };
            let out: Vec<String> = objects.iter().map(|o| self.response(&Node::Object(o), &wanted)).collect();
            return multistatus(&out, "");
        }
        if root.is(DAV, "sync-collection") {
            let Some(id) = cal_id else { return status(StatusCode::FORBIDDEN) };
            let Some(cal) = store.calendar(id) else { return status(StatusCode::NOT_FOUND) };
            let token = root.child(DAV, "sync-token").map(|t| t.text.trim()).unwrap_or("");
            let since = if token.is_empty() {
                0
            } else {
                match token.strip_prefix(SYNC_PREFIX).and_then(|n| n.parse::<i64>().ok()) {
                    Some(n) if n <= cal.rev => n,
                    _ => return precondition(StatusCode::FORBIDDEN, DAV, "valid-sync-token", ""),
                }
            };
            let (changed, deleted) = store.changes(id, since);
            let mut out: Vec<String> = changed.iter().map(|o| self.response(&Node::Object(o), &wanted)).collect();
            if since > 0 {
                out.extend(deleted.iter().map(|n| not_found(&obj_href(id, n))));
            }
            let token = format!("<D:sync-token>{}</D:sync-token>", escape(&sync_token(cal.rev)));
            return multistatus(&out, &token);
        }
        // principal-property-search, expand-property, …: nothing to report.
        multistatus(&[], "")
    }

    fn proppatch(&self, res: &Res, path: &str, body: &str) -> Response<Body> {
        let Ok(root) = xml::parse(body) else { return status(StatusCode::BAD_REQUEST) };
        let mut cal = match res {
            Res::Calendar(id) => self.app.store.calendar(id),
            _ => None,
        };
        let mut names = String::new();
        for op in root.children.iter() {
            let set = op.is(DAV, "set");
            for prop in op.child(DAV, "prop").map(|p| p.children.as_slice()).unwrap_or(&[]) {
                names.push_str(&element(&prop.ns, &prop.name, ""));
                let Some(cal) = cal.as_mut() else { continue };
                let text = if set { prop.text.trim().to_string() } else { String::new() };
                match (prop.ns.as_str(), prop.name.as_str()) {
                    (DAV, "displayname") if !text.is_empty() => cal.name = text,
                    (APPLE, "calendar-color") if !text.is_empty() => cal.color = text,
                    (APPLE, "calendar-order") => cal.ord = text.parse().unwrap_or(cal.ord),
                    (CALDAV, "calendar-description") => cal.description = text,
                    _ => {}
                }
            }
        }
        if let Some(cal) = cal
            && self.app.store.update_calendar(&cal).is_err()
        {
            return status(StatusCode::INTERNAL_SERVER_ERROR);
        }
        let out = format!(
            "<D:response><D:href>{}</D:href><D:propstat><D:prop>{names}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
            escape(path)
        );
        multistatus(&[out], "")
    }

    fn mkcalendar(&self, res: &Res, body: &str) -> Response<Body> {
        let Res::Calendar(id) = res else { return status(StatusCode::FORBIDDEN) };
        let root = if body.trim().is_empty() { None } else { xml::parse(body).ok() };
        let mut props = Vec::new();
        if let Some(root) = &root {
            root.find(DAV, "prop", &mut props);
        }
        let text = |ns: &str, name: &str| {
            props.iter().find_map(|p| p.child(ns, name)).map(|e| e.text.trim().to_string()).filter(|t| !t.is_empty())
        };
        let name = text(DAV, "displayname").unwrap_or_else(|| id.clone());
        match self.app.store.create_calendar(id, &name, text(APPLE, "calendar-color").as_deref()) {
            Ok(true) => status(StatusCode::CREATED),
            Ok(false) => status(StatusCode::METHOD_NOT_ALLOWED),
            Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    fn get(&self, res: &Res, head: bool) -> Response<Body> {
        let (data, tag) = match res {
            Res::Object(c, n) => match self.app.store.object(c, n) {
                Some(o) => (o.data, Some(o.etag)),
                None => return status(StatusCode::NOT_FOUND),
            },
            Res::Calendar(id) => match self.app.store.calendar(id) {
                Some(c) => (crate::export(&self.app.store, &c), None),
                None => return status(StatusCode::NOT_FOUND),
            },
            _ => return status(StatusCode::METHOD_NOT_ALLOWED),
        };
        let mut r = Response::builder().status(StatusCode::OK).header(header::CONTENT_TYPE, "text/calendar; charset=utf-8");
        if let Some(tag) = tag {
            r = r.header(header::ETAG, tag);
        }
        r.body(if head { Body::empty() } else { Body::from(data) }).unwrap()
    }

    /// If-Match / If-None-Match against the current ETag (`None`: no resource).
    fn preconditions_fail(&self, current: Option<&str>) -> bool {
        let header = |name| self.headers.get(name).and_then(|v: &HeaderValue| v.to_str().ok()).map(str::trim);
        if let Some(m) = header(header::IF_MATCH) {
            let ok = match current {
                None => false,
                Some(cur) => m == "*" || m.split(',').any(|t| t.trim() == cur),
            };
            if !ok {
                return true;
            }
        }
        if let Some(m) = header(header::IF_NONE_MATCH)
            && let Some(cur) = current
            && (m == "*" || m.split(',').any(|t| t.trim() == cur))
        {
            return true;
        }
        false
    }

    fn put(&self, res: &Res, body: &str) -> Response<Body> {
        let Res::Object(cal, name) = res else { return status(StatusCode::METHOD_NOT_ALLOWED) };
        let current = self.app.store.object(cal, name);
        if self.preconditions_fail(current.as_ref().map(|o| o.etag.as_str())) {
            return status(StatusCode::PRECONDITION_FAILED);
        }
        match self.app.store.put_object(cal, name, body, false) {
            Ok(put) => {
                let mut r = Response::builder().status(if put.created { StatusCode::CREATED } else { StatusCode::NO_CONTENT });
                // RFC 4791 5.3.4: no ETag when what is stored differs from what was sent.
                if !put.rewritten {
                    r = r.header(header::ETAG, put.etag);
                }
                r.body(Body::empty()).unwrap()
            }
            Err(PutError::NoCalendar) => status(StatusCode::CONFLICT),
            Err(PutError::UidConflict(holder)) => precondition(
                StatusCode::FORBIDDEN,
                CALDAV,
                "no-uid-conflict",
                &format!("<D:href>{}</D:href>", escape(&obj_href(cal, &holder))),
            ),
            Err(PutError::Invalid(why)) => {
                tracing::info!(%cal, %name, %why, "rejected event");
                precondition(StatusCode::FORBIDDEN, CALDAV, "valid-calendar-data", "")
            }
        }
    }

    fn delete(&self, res: &Res) -> Response<Body> {
        match res {
            Res::Object(cal, name) => {
                let current = self.app.store.object(cal, name);
                if current.is_none() {
                    return status(StatusCode::NOT_FOUND);
                }
                if self.preconditions_fail(current.as_ref().map(|o| o.etag.as_str())) {
                    return status(StatusCode::PRECONDITION_FAILED);
                }
                match self.app.store.delete_object(cal, name) {
                    Ok(_) => status(StatusCode::NO_CONTENT),
                    Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
                }
            }
            Res::Calendar(id) => {
                if self.app.store.calendar(id).is_none() {
                    return status(StatusCode::NOT_FOUND);
                }
                match self.app.store.delete_calendar(id) {
                    Ok(()) => status(StatusCode::NO_CONTENT),
                    Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
                }
            }
            _ => status(StatusCode::FORBIDDEN),
        }
    }
}

fn not_found(href: &str) -> String {
    format!("<D:response><D:href>{}</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>", escape(href))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(resolve("/"), Some(Res::Root));
        assert_eq!(resolve("/dav"), Some(Res::Root));
        assert_eq!(resolve("/dav/principal/"), Some(Res::Principal));
        assert_eq!(resolve("/dav/cal"), Some(Res::Home));
        assert_eq!(resolve("/dav/cal/work/"), Some(Res::Calendar("work".into())));
        assert_eq!(resolve("/dav/cal/work/a%20b%40c.ics"), Some(Res::Object("work".into(), "a b@c.ics".into())));
        assert_eq!(resolve("/davx"), None);
        assert_eq!(resolve("/dav/cal/a/b/c"), None);
        assert_eq!(obj_href("work", "a b@c.ics"), "/dav/cal/work/a%20b@c.ics");
    }
}
