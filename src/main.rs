mod accounts;
mod alarms;
mod calendar;
mod dav;
mod folders;
mod ical;
mod imapd;
mod import;
mod invites;
mod mailstore;
mod mime;
mod mutf7;
mod notify;
mod smtpd;
mod store;
mod sync;
mod util;
mod web;
mod xml;

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, RwLock};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::accounts::Account;
use crate::ical::Component;
use crate::mailstore::MailStore;
use crate::store::{Calendar, Store};

pub struct Config {
    pub user: String,
    password: String,
    /// The user's email addresses, so calendar apps recognise them in invites.
    pub addresses: Vec<String>,
}

impl Config {
    fn from_env() -> Result<Config> {
        let password = std::env::var("VALET_PASSWORD").context("VALET_PASSWORD must be set")?;
        anyhow::ensure!(password.len() >= 12, "VALET_PASSWORD must be at least 12 characters");
        Ok(Config {
            user: std::env::var("VALET_USER").unwrap_or_else(|_| "valet".into()),
            password,
            addresses: std::env::var("VALET_ADDRESSES")
                .unwrap_or_default()
                .split(',')
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect(),
        })
    }

    pub fn check_password(&self, user: &str, password: &str) -> bool {
        let user_ok = user.as_bytes().ct_eq(self.user.as_bytes());
        let pass_ok = password.as_bytes().ct_eq(self.password.as_bytes());
        (user_ok & pass_ok).into()
    }

    /// HTTP Basic credentials, as calendar apps send them.
    pub fn check_basic(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else { return false };
        let Some(encoded) = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")) else { return false };
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else { return false };
        let decoded = String::from_utf8_lossy(&decoded);
        let Some((user, password)) = decoded.split_once(':') else { return false };
        self.check_password(user, password)
    }

    /// The web UI's session cookie: derived from the credentials, so it
    /// survives restarts and dies with a password change.
    pub fn session(&self) -> String {
        hex::encode(Sha256::digest(format!("valet-session\0{}\0{}", self.user, self.password)))
    }
}

pub struct App {
    pub store: Store,
    pub config: Config,
    pub mail: MailStore,
    accounts: RwLock<Vec<Account>>,
    /// Each account's sync task.
    pub syncs: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// OAuth logins in progress: state → (account id, PKCE verifier, redirect URI).
    pub oauth_pending: Mutex<HashMap<String, (String, String, String)>>,
    /// True once valet is shutting down: what streams to pages ends then.
    pub stopping: tokio::sync::watch::Sender<bool>,
}

impl App {
    pub fn new(store: Store, config: Config, mail: MailStore) -> App {
        let accounts = RwLock::new(mail.accounts());
        App { store, config, mail, accounts, syncs: Mutex::default(), oauth_pending: Mutex::default(), stopping: tokio::sync::watch::channel(false).0 }
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.accounts.read().unwrap().clone()
    }

    pub fn account(&self, id: &str) -> Option<Account> {
        self.accounts.read().unwrap().iter().find(|a| a.id == id).cloned()
    }

    /// Saves an account (new, or replacing the one with its id) and restarts its sync.
    pub fn save_account(self: &Arc<Self>, acc: Account) -> Result<()> {
        self.mail.put_account(&acc)?;
        *self.accounts.write().unwrap() = self.mail.accounts();
        sync::restart(self, &acc.id);
        Ok(())
    }

    pub fn delete_account(self: &Arc<Self>, id: &str) -> Result<()> {
        if let Some(task) = self.syncs.lock().unwrap().remove(id) {
            task.abort();
        }
        self.mail.delete_account(id)?;
        *self.accounts.write().unwrap() = self.mail.accounts();
        Ok(())
    }

    /// Every address that is the user: `VALET_ADDRESSES` and the accounts'.
    pub fn addresses(&self) -> Vec<String> {
        let mut all = self.config.addresses.clone();
        for a in self.accounts.read().unwrap().iter() {
            for addr in std::iter::once(&a.address).chain(&a.aliases) {
                if !all.iter().any(|x| x.eq_ignore_ascii_case(addr)) {
                    all.push(addr.clone());
                }
            }
        }
        all
    }
}

/// All of a calendar's events as one iCalendar file.
pub fn export(store: &Store, cal: &Calendar) -> String {
    let mut out = Component::new("VCALENDAR");
    out.push("VERSION", "2.0");
    out.push("PRODID", "-//valet//EN");
    out.push("X-WR-CALNAME", &cal.name);
    let mut zones = HashSet::new();
    for o in store.objects(&cal.id) {
        let Ok(parsed) = ical::parse(&o.data) else { continue };
        for c in parsed.into_iter().flat_map(|c| c.children) {
            if c.name == "VTIMEZONE" && !zones.insert(c.value("TZID").unwrap_or("").to_string()) {
                continue;
            }
            out.children.push(c);
        }
    }
    out.to_ics()
}

/// What the web UI's routes don't take: CalDAV (any WebDAV method, and
/// everything under /dav) and its discovery redirect.
pub async fn fallback(State(app): State<Arc<App>>, req: Request) -> Response {
    let path = req.uri().path();
    if path.starts_with("/.well-known/caldav") {
        return Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(header::LOCATION, "/dav/")
            .body(Body::empty())
            .unwrap();
    }
    let web_method = matches!(*req.method(), Method::GET | Method::HEAD | Method::POST);
    if path == "/dav" || path.starts_with("/dav/") || !web_method {
        return dav::handle(app, req).await;
    }
    Response::builder().status(StatusCode::NOT_FOUND).body(Body::from("Not found")).unwrap()
}

pub fn router(app: Arc<App>) -> axum::Router {
    web::set_app(&app);
    web::routes()
        .fallback(any(fallback))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(app)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = Config::from_env()?;
    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into()));
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let store = Store::open(&data_dir.join("valet.db"))?;
    if let Some(tz) = store.setting("tz").and_then(|z| calendar::zone(&z)) {
        calendar::set_default_zone(tz);
    }
    let mail = MailStore::open(&data_dir.join("mail.db"))?;
    if mail.accounts().is_empty() {
        let from_env = accounts::from_env()?;
        for a in &from_env {
            mail.put_account(a)?;
        }
        if !from_env.is_empty() {
            tracing::info!("imported {} accounts from VALET_ACCOUNTS: they are managed on the Mail page now, VALET_ACCOUNTS can go", from_env.len());
        }
    }
    let app = Arc::new(App::new(store, config, mail));

    let bind = async |var: &str, default: &str| -> Result<tokio::net::TcpListener> {
        let addr = std::env::var(var).unwrap_or_else(|_| default.into());
        let l = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr} ({var})"))?;
        tracing::info!("{var}: listening on {addr}");
        Ok(l)
    };
    let http = bind("LISTEN", "0.0.0.0:8080").await?;
    let imap = bind("IMAP_LISTEN", "0.0.0.0:1143").await?;
    let smtp = bind("SMTP_LISTEN", "0.0.0.0:1587").await?;
    tokio::spawn(imapd::serve(app.clone(), imap));
    tokio::spawn(smtpd::serve(app.clone(), smtp));
    sync::start(app.clone());
    tracing::info!("{} mail accounts", app.accounts().len());
    let stopping = app.clone();
    axum::serve(http, router(app))
        .with_graceful_shutdown(async move {
            shutdown().await;
            stopping.stopping.send_replace(true);
        })
        .await?;
    Ok(())
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
