//! Notifications of new mail: turning them on in a browser (a Web Push
//! subscription), the devices where they are on, and the arrivals valet's
//! Android app asks for.

use axum::Json;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;

use super::{St, authed, back};
use crate::notify::{self, Note};
use crate::store::Push;

#[derive(Deserialize)]
pub struct Keys {
    p256dh: String,
    auth: String,
}

/// A browser's `PushSubscription`, as `toJSON()` gives it.
#[derive(Deserialize)]
pub struct Subscription {
    endpoint: String,
    keys: Keys,
}

pub async fn subscribe(State(app): St, headers: HeaderMap, Json(s): Json<Subscription>) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !s.endpoint.starts_with("https://") {
        return (StatusCode::BAD_REQUEST, "push endpoints are https").into_response();
    }
    let agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    // Push services want a way to reach whoever sends: the site will do if the user has no address yet.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).filter(|o| o.starts_with("https://")) {
        let _ = app.store.set_setting("push_origin", origin);
    }
    let p = Push { endpoint: s.endpoint, p256dh: s.keys.p256dh, auth: s.keys.auth, agent, created: crate::mailstore::now() };
    match app.store.put_push(&p) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct Endpoint {
    endpoint: String,
}

pub async fn unsubscribe(State(app): St, headers: HeaderMap, Json(e): Json<Endpoint>) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _ = app.store.delete_push(&e.endpoint);
    StatusCode::NO_CONTENT.into_response()
}

/// A notification to this device now, to see they work.
pub async fn test(State(app): St, headers: HeaderMap, Json(e): Json<Endpoint>) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(p) = app.store.pushes().into_iter().find(|p| p.endpoint == e.endpoint) else {
        return (StatusCode::NOT_FOUND, "valet doesn't know this device: turn notifications off and on again").into_response();
    };
    let note = Note {
        tag: "test".into(),
        title: "valet".into(),
        body: "Notifications work: new mail shows like this.".into(),
        account: String::new(),
        url: "/settings".into(),
        message: None,
    };
    match notify::send(&app, &p, &note).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

/// From the list of devices: stops notifying one.
pub async fn remove(State(app): St, headers: HeaderMap, Form(e): Form<Endpoint>) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match app.store.delete_push(&e.endpoint) {
        Ok(_) => back("/settings", Some("That device won't be notified any more."), None),
        Err(e) => back("/settings", None, Some(&format!("Not removed: {e}"))),
    }
}

#[derive(Deserialize)]
pub struct After {
    after: Option<i64>,
}

/// For valet's Android app: what came in since arrival `after` (none
/// without it: the app just learns where the list is), and the last
/// arrival's number, to ask from next time.
pub async fn new_mail(State(app): St, headers: HeaderMap, Query(q): Query<After>) -> Response {
    if !authed(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let last = app.mail.last_arrival();
    let notes = match q.after {
        Some(after) if after < last => notify::notes(&app, &app.mail.arrivals_after(after)),
        _ => Vec::new(),
    };
    Json(serde_json::json!({ "last": last, "notes": notes })).into_response()
}

/// The settings' part: this device, and every device where notifications are on.
pub fn section(app: &crate::App) -> Markup {
    let pushes = app.store.pushes();
    html! {
        h2 { "Notifications" }
        div.card #push data-key=(notify::public_key(app)) {
            p { "New mail in your inboxes shows as a notification — who it's from, the subject and its first words — on each device where you turn them on." }
            p #push-state.muted { "…" }
            div style="display:flex; gap:8px; flex-wrap:wrap" {
                button type="button" #push-on hidden { "Turn on for this device" }
                button type="button" #push-test hidden { "Send a test" }
                button.plain type="button" #push-off hidden { "Turn off for this device" }
            }
            @if !pushes.is_empty() {
                table.small style="margin-top:12px" {
                    tr { th { "On for" } th { "Since" } th {} }
                    @for p in &pushes {
                        tr data-endpoint=(p.endpoint) {
                            td { (notify::device(&p.agent)) span.this.muted hidden { " (this device)" } }
                            td.muted { (super::at(p.created)) }
                            td { form method="post" action="/push/remove" {
                                input type="hidden" name="endpoint" value=(p.endpoint);
                                button.plain type="submit" { "Remove" }
                            } }
                        }
                    }
                }
            }
        }
        script { (PreEscaped(SCRIPT)) }
    }
}

const SCRIPT: &str = r#"
{
  const box = document.getElementById('push'), state = document.getElementById('push-state');
  const on = document.getElementById('push-on'), off = document.getElementById('push-off'), test = document.getElementById('push-test');
  const say = (text) => { state.textContent = text; };
  const show = (sub) => {
    on.hidden = !!sub; off.hidden = test.hidden = !sub;
    for (const r of box.querySelectorAll('tr[data-endpoint]')) r.querySelector('.this').hidden = !sub || r.dataset.endpoint !== sub.endpoint;
  };
  const post = (url, body) => fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  const key = () => Uint8Array.from(atob(box.dataset.key.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
  const ios = /iPhone|iPad/.test(navigator.userAgent) && !navigator.standalone;
  if (window.ValetApp) {
    say("valet's Android app tells you of new mail itself: it looks every quarter of an hour or so. Android's settings for the app turn it off (Notifications → New mail).");
  } else if (!('serviceWorker' in navigator) || !('PushManager' in window) || !('Notification' in window)) {
    say(ios ? 'On an iPhone or iPad, add valet to the Home Screen first (Share → Add to Home Screen), then turn notifications on from there.' : "This browser can't show notifications from valet.");
  } else {
    navigator.serviceWorker.ready.then(async (reg) => {
      const sub = await reg.pushManager.getSubscription();
      if (Notification.permission === 'denied') say("Notifications are blocked for valet in this browser: allow them in the site's settings, then come back.");
      else say(sub ? 'On for this device.' : 'Off for this device.');
      show(Notification.permission === 'granted' ? sub : null);
    });
    on.addEventListener('click', async () => {
      try {
        if (await Notification.requestPermission() !== 'granted') { say("Notifications weren't allowed: allow them in the site's settings to turn them on."); return; }
        const reg = await navigator.serviceWorker.ready;
        const sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key() });
        const r = await post('/push/subscribe', sub.toJSON());
        if (!r.ok) throw new Error(await r.text());
        localStorage.setItem('valet.push', sub.endpoint);
        location.reload();
      } catch (e) { say(`Not turned on: ${e.message || e}`); }
    });
    off.addEventListener('click', async () => {
      const sub = await (await navigator.serviceWorker.ready).pushManager.getSubscription();
      if (sub) { await post('/push/unsubscribe', { endpoint: sub.endpoint }); await sub.unsubscribe(); }
      localStorage.removeItem('valet.push');
      location.reload();
    });
    test.addEventListener('click', async () => {
      const sub = await (await navigator.serviceWorker.ready).pushManager.getSubscription();
      say('Sending…');
      const r = await post('/push/test', { endpoint: sub.endpoint });
      say(r.ok ? 'Sent: the notification shows in a moment.' : `Not sent: ${await r.text()}`);
    });
  }
}
"#;
