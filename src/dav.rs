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
    let dav = Dav { app: &app, headers: &headers, expand: None };
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

#[derive(Clone, Copy)]
struct Dav<'a> {
    app: &'a App,
    headers: &'a HeaderMap,
    /// A REPORT's `<C:expand start end/>`: calendar-data gives each
    /// occurrence in [start, end) on its own.
    expand: Option<(i64, i64)>,
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
            (CALDAV, "calendar-data", Node::Object(o)) => match self.expand {
                Some((from, to)) => escape(&crate::calendar::expanded(&o.data, from, to).unwrap_or_else(|| o.data.clone())),
                None => escape(&o.data),
            },
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
        let expand = root.child(DAV, "prop").and_then(|p| p.child(CALDAV, "calendar-data")).and_then(|d| d.child(CALDAV, "expand"));
        let expand = match expand.map(time_range) {
            None => None,
            Some(Some(range)) => Some(range),
            Some(None) => return precondition(StatusCode::FORBIDDEN, CALDAV, "valid-filter", ""),
        };
        let dav = Dav { expand, ..*self };
        let this = &dav;
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
                            Some(o) => this.response(&Node::Object(&o), &wanted),
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
            let mut objects: Vec<Object> = match res {
                Res::Object(_, name) => objects.into_iter().filter(|o| &o.name == name).collect(),
                _ => objects,
            };
            // The events that happen in the range, as the calendar page and
            // the agenda find them.
            let range = filters.iter().filter(|f| f.attr("name") == Some("VEVENT")).find_map(|f| f.child(CALDAV, "time-range"));
            if let Some(range) = range {
                let Some((from, to)) = time_range(range) else { return precondition(StatusCode::FORBIDDEN, CALDAV, "valid-filter", "") };
                objects.retain(|o| crate::calendar::happens(&o.data, from, to));
            }
            let out: Vec<String> = objects.iter().map(|o| this.response(&Node::Object(o), &wanted)).collect();
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
            let mut out: Vec<String> = changed.iter().map(|o| this.response(&Node::Object(o), &wanted)).collect();
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

/// A CalDAV UTC time (`20261114T000000Z`) as Unix seconds.
fn caldav_time(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s.trim().strip_suffix('Z')?, "%Y%m%dT%H%M%S").ok().map(|t| t.and_utc().timestamp())
}

/// The range of a `<C:time-range>` or `<C:expand>`: a missing bound is
/// open (here, from 1900 or to 2200); `None` when one can't be read.
fn time_range(e: &Element) -> Option<(i64, i64)> {
    const EARLIEST: i64 = -2_208_988_800;
    const LATEST: i64 = 7_258_118_400;
    let bound = |name, open| e.attr(name).map_or(Some(open), caldav_time);
    let (from, to) = (bound("start", EARLIEST)?, bound("end", LATEST)?);
    (from < to).then_some((from.max(EARLIEST), to.min(LATEST)))
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

#[cfg(test)]
mod query_tests {
    use super::*;
    use crate::calendar::{self, When};
    use crate::ical;

    const WEEKLY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:weekly\r\nSUMMARY:Standup\r\n\
        DTSTART;TZID=Europe/Paris:20261005T090000\r\nDTEND;TZID=Europe/Paris:20261005T091500\r\nRRULE:FREQ=WEEKLY\r\n\
        EXDATE;TZID=Europe/Paris:20261109T090000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:weekly\r\n\
        RECURRENCE-ID;TZID=Europe/Paris:20261019T090000\r\nSUMMARY:Standup (moved)\r\nDTSTART;TZID=Europe/Paris:20261020T140000\r\n\
        DTEND;TZID=Europe/Paris:20261020T141500\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:weekly\r\nRECURRENCE-ID:20261102T080000Z\r\n\
        STATUS:CANCELLED\r\nDTSTART:20261102T080000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const AWAY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:away\r\nSUMMARY:Away\r\n\
        DTSTART;VALUE=DATE:20261030\r\nDTEND;VALUE=DATE:20261103\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const LATER: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:later\r\nSUMMARY:Later\r\n\
        DTSTART:20261210T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn app() -> Arc<App> {
        let store = crate::store::Store::memory();
        store.create_calendar("work", "Work", None).unwrap();
        for (name, data) in [("weekly.ics", WEEKLY), ("away.ics", AWAY), ("later.ics", LATER)] {
            store.put_object("work", name, data, false).unwrap();
        }
        let config = crate::Config { user: "me".into(), password: "a long test password".into(), addresses: vec![] };
        Arc::new(App::new(store, config, crate::mailstore::MailStore::memory()))
    }

    async fn report(app: &Arc<App>, body: &str) -> xml::Element {
        use base64::Engine;
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("me:a long test password"));
        let req = Request::builder().method("REPORT").uri("/dav/cal/work/").header("Depth", "1").header(header::AUTHORIZATION, auth).body(Body::from(body.to_string())).unwrap();
        let res = handle(app.clone(), req).await;
        assert_eq!(res.status(), StatusCode::MULTI_STATUS);
        let bytes = to_bytes(res.into_body(), MAX_BODY).await.unwrap();
        // The request parser trims text around references: keep the line
        // breaks of calendar-data as references too.
        xml::parse(&std::str::from_utf8(&bytes).unwrap().replace("&#13;\n", "&#13;&#10;")).unwrap()
    }

    fn query(range: &str, data: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:prop><D:getetag/>{data}</D:prop>
            <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">{range}</C:comp-filter></C:comp-filter></C:filter>
            </C:calendar-query>"#
        )
    }

    fn hrefs(ms: &xml::Element) -> Vec<String> {
        let mut out: Vec<String> = ms.all(DAV, "response").filter_map(|r| r.child(DAV, "href")).map(|h| h.text.trim().to_string()).collect();
        out.sort();
        out
    }

    fn secs(s: &str) -> i64 {
        caldav_time(s).unwrap()
    }

    #[tokio::test]
    async fn time_range_and_expand_match_the_engine() {
        let app = app();
        // 19 October to 9 November: the series and the trip, not December's.
        let (start, end) = ("20261019T000000Z", "20261109T230000Z");
        let range = format!(r#"<C:time-range start="{start}" end="{end}"/>"#);
        let ms = report(&app, &query(&range, "")).await;
        assert_eq!(hrefs(&ms), ["/dav/cal/work/away.ics", "/dav/cal/work/weekly.ics"]);
        let ms = report(&app, &query("", "")).await;
        assert_eq!(hrefs(&ms).len(), 3, "no time-range, every event");
        let ms = report(&app, &query(r#"<C:time-range start="20261201T000000Z"/>"#, "")).await;
        assert_eq!(hrefs(&ms), ["/dav/cal/work/later.ics", "/dav/cal/work/weekly.ics"], "open-ended");

        // Expanded: every occurrence on its own, as the engine finds them.
        let data = format!(r#"<C:calendar-data><C:expand start="{start}" end="{end}"/></C:calendar-data>"#);
        let ms = report(&app, &query(&range, &data)).await;
        let mut got: Vec<(String, When, When, Option<When>)> = Vec::new();
        for r in ms.all(DAV, "response") {
            let mut found = Vec::new();
            r.find(CALDAV, "calendar-data", &mut found);
            let cal = &ical::parse(&found[0].text).unwrap()[0];
            assert!(cal.children.iter().all(|c| c.name == "VEVENT"), "no VTIMEZONE");
            for e in &cal.children {
                assert!(e.prop("RRULE").is_none() && e.prop("EXDATE").is_none());
                let w = |name: &str| {
                    let p = e.prop(name)?;
                    Some(if p.param("VALUE") == Some("DATE") { When::Day(chrono::NaiveDate::parse_from_str(&p.value, "%Y%m%d").unwrap()) } else { When::At(secs(&p.value)) })
                };
                got.push((e.value("UID").unwrap().to_string(), w("DTSTART").unwrap(), w("DTEND").unwrap(), w("RECURRENCE-ID")));
            }
        }
        let mut want = Vec::new();
        for o in app.store.objects("work") {
            let cal = &ical::parse(&o.data).unwrap()[0];
            for occ in calendar::occurrences_in(cal, secs(start), secs(end), calendar::default_zone()) {
                want.push((o.uid.clone(), occ.start, occ.end, occ.recurrence_id));
            }
        }
        let key = |x: &(String, When, When, Option<When>)| (x.0.clone(), format!("{:?}", x.1));
        got.sort_by_key(key);
        want.sort_by_key(key);
        assert_eq!(got, want);
        let weekly: Vec<When> = got.iter().filter(|x| x.0 == "weekly").map(|x| x.1).collect();
        assert_eq!(
            weekly,
            [When::At(secs("20261020T120000Z")), When::At(secs("20261026T080000Z"))],
            "the moved one, then the 26th after daylight saving; 2 and 9 November deleted"
        );

        // A bad range is refused.
        let bad = format!(r#"<C:time-range start="2026-10-19" end="{end}"/>"#);
        use base64::Engine;
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("me:a long test password"));
        let req = Request::builder().method("REPORT").uri("/dav/cal/work/").header(header::AUTHORIZATION, auth).body(Body::from(query(&bad, ""))).unwrap();
        assert_eq!(handle(app.clone(), req).await.status(), StatusCode::FORBIDDEN);
    }
}
