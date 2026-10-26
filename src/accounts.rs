//! The mailboxes valet gathers: their settings (kept in `mail.db`, edited on
//! the Mail page), logging in to their IMAP and SMTP servers, and OAuth for
//! Microsoft 365.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use lettre::AsyncTransport;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::mailstore::{MailStore, now};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    /// Short, stable identifier (`work`).
    pub id: String,
    /// What mail apps show as the folder (`Work`).
    pub name: String,
    pub address: String,
    /// Other addresses this account may send as.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// `imap.ionos.fr:993` (implicit TLS).
    pub imap: String,
    /// `smtp.ionos.fr:465` (implicit TLS) or `…:587` (STARTTLS).
    pub smtp: String,
    /// Login, if not the address.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub oauth: Option<OAuth>,
    /// How far back the first sync of a folder goes.
    #[serde(default = "default_days")]
    pub history_days: u32,
    /// Provider folders to leave out (their names as the provider lists them, decoded).
    #[serde(default)]
    pub skip: Vec<String>,
    /// False only for a local test server: IMAP and SMTP without encryption.
    #[serde(default = "yes")]
    pub tls: bool,
    /// Since when (Unix seconds) valet keeps its mail without syncing it: it
    /// no longer logs in or sends, and changes to its mail stay in valet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired: Option<i64>,
}

fn default_days() -> u32 {
    90
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuth {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "default_tenant")]
    pub tenant: String,
    /// Where the provider sends the browser back. Valet's own
    /// `https://<host>/mail/oauth` finishes by itself; anything else (like
    /// `https://localhost`) means pasting the address you land on.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: String,
}

fn default_tenant() -> String {
    "common".into()
}

fn default_scopes() -> String {
    "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send".into()
}

impl Account {
    pub fn login(&self) -> &str {
        self.user.as_deref().unwrap_or(&self.address)
    }

    /// Whether it can send as `address` now (a retired account sends nothing).
    pub fn sends_now(&self, address: &str) -> bool {
        self.retired.is_none() && self.sends_as(address)
    }

    pub fn sends_as(&self, address: &str) -> bool {
        self.address.eq_ignore_ascii_case(address) || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(address))
    }

    /// What makes an account unusable, said for the person typing it in.
    pub fn check(&self) -> Result<()> {
        if self.id.is_empty() {
            bail!("the account needs an id");
        }
        if self.name.trim().is_empty() || self.name.contains('/') {
            bail!("the account's name can't be empty or contain a /");
        }
        if !self.address.contains('@') {
            bail!("{} is not an email address", self.address);
        }
        if self.password.as_deref().is_none_or(str::is_empty) && self.oauth.is_none() {
            bail!("{} needs a password", self.name);
        }
        for (what, server) in [("IMAP", &self.imap), ("SMTP", &self.smtp)] {
            if server.trim().is_empty() {
                bail!("the {what} server is missing");
            }
        }
        Ok(())
    }
}

/// Accounts from `VALET_ACCOUNTS`, the way they used to be configured: imported
/// once into valet's database.
pub fn from_env() -> Result<Vec<Account>> {
    let Ok(json) = std::env::var("VALET_ACCOUNTS") else { return Ok(Vec::new()) };
    let accounts: Vec<Account> = serde_json::from_str(&json).context("VALET_ACCOUNTS is not valid JSON")?;
    for a in &accounts {
        a.check().with_context(|| format!("account {}", a.id))?;
    }
    Ok(accounts)
}

/// A provider whose servers are known: only the address and password to type.
pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub imap: &'static str,
    pub smtp: &'static str,
    /// Microsoft 365: OAuth with Thunderbird's public client, which
    /// organisations commonly allow.
    pub microsoft: bool,
    /// Where to get what the form asks: HTML, links included.
    pub help: &'static [&'static str],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        key: "gmail",
        label: "Gmail",
        imap: "imap.gmail.com:993",
        smtp: "smtp.gmail.com:465",
        microsoft: false,
        help: &[
            r#"Not your Google password: an app password. Make one at <a href="https://myaccount.google.com/apppasswords">myaccount.google.com/apppasswords</a> (name it "valet") and paste its 16 letters, spaces or not."#,
            r#"App passwords need <a href="https://myaccount.google.com/signinoptions/twosv">2-Step Verification</a>: turn it on first if the page says they aren't available."#,
            r#"A work or school account may not offer them: its admin decides (<a href="https://support.google.com/accounts/answer/185833">Google's help</a>)."#,
        ],
    },
    Preset {
        key: "ionos",
        label: "IONOS",
        imap: "imap.ionos.fr:993",
        smtp: "smtp.ionos.fr:465",
        microsoft: false,
        help: &[
            "The mailbox's own password, chosen when the address was made: not the one of your IONOS customer account.",
            r#"Forgotten? Set a new one in <a href="https://my.ionos.fr">your IONOS account</a>: Email, then ⋯ next to the address, then Change password (<a href="https://www.ionos.com/help/email/troubleshooting-mail-basic-mail-business/changing-an-ionos-email-account-password/">IONOS help</a>)."#,
            "The servers are IONOS France's. Elsewhere, change them under Servers to your country's (imap.ionos.de, imap.ionos.co.uk…).",
        ],
    },
    Preset {
        key: "microsoft",
        label: "Microsoft 365 / Outlook",
        imap: "outlook.office365.com:993",
        smtp: "smtp.office365.com:587",
        microsoft: true,
        help: &[
            "No password here: once saved, log in to Microsoft from the Mail page.",
            r#"valet logs in as Thunderbird does. If Microsoft says "Need admin approval", your organisation's admin has to allow Mozilla Thunderbird (and IMAP) for your account."#,
        ],
    },
];

/// The same for the other providers: where their servers and app passwords are.
pub const OTHER_HELP: &[&str] = &[
    r#"Your provider's help lists its IMAP and SMTP servers, often on the page about setting up mail apps or Thunderbird."#,
    "With two-step verification on, most want an app password rather than your usual one.",
    r#"iCloud Mail: an <a href="https://support.apple.com/102654">app-specific password</a>; servers imap.mail.me.com:993 and smtp.mail.me.com:587."#,
    r#"Yahoo: an <a href="https://help.yahoo.com/kb/SLN15241.html">app password</a>; servers imap.mail.yahoo.com:993 and smtp.mail.yahoo.com:465."#,
    r#"Fastmail: an <a href="https://www.fastmail.help/hc/en-us/articles/360058752854">app password</a> (not on Basic plans); servers imap.fastmail.com:993 and smtp.fastmail.com:465."#,
];

pub const THUNDERBIRD_CLIENT: &str = "9e5f94bc-e8a4-4e73-b8be-63364c29d753";

pub fn preset(key: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.key == key)
}

/// Logs in to both servers, to tell a wrong password at once.
pub async fn test_login(acc: &Account, mail: &MailStore) -> Result<()> {
    let mut s = imap(acc, mail).await.context("IMAP")?;
    let _ = s.logout().await;
    smtp(acc, mail).await?.test_connection().await.context("SMTP")?.then_some(()).context("SMTP: the server refused")?;
    Ok(())
}

fn host_port(s: &str, default: u16) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default)),
        None => (s.to_string(), default),
    }
}

pub fn tls() -> TlsConnector {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + std::fmt::Debug {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + std::fmt::Debug> Io for T {}

pub type Imap = async_imap::Session<Box<dyn Io>>;

struct XOAuth2(String);

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> String {
        // A second challenge carries an error: answer empty to end it.
        std::mem::take(&mut self.0)
    }
}

pub fn xoauth2(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

pub async fn imap(acc: &Account, mail: &MailStore) -> Result<Imap> {
    let (host, port) = host_port(&acc.imap, 993);
    let tcp = tokio::time::timeout(Duration::from_secs(20), TcpStream::connect((host.as_str(), port))).await??;
    let stream: Box<dyn Io> = if acc.tls {
        let server = rustls::pki_types::ServerName::try_from(host.clone())?;
        Box::new(tls().connect(server, tcp).await?)
    } else {
        Box::new(tcp)
    };
    let mut client = async_imap::Client::new(stream);
    client.read_response().await?.ok_or_else(|| anyhow!("{host} closed before greeting"))?;
    let session = match &acc.oauth {
        Some(_) => {
            let token = access_token(acc, mail).await?;
            client.authenticate("XOAUTH2", XOAuth2(xoauth2(acc.login(), &token))).await.map_err(|(e, _)| refused(e))?
        }
        None => client.login(acc.login(), acc.password.as_deref().unwrap_or("")).await.map_err(|(e, _)| refused(e))?,
    };
    Ok(session)
}

/// A refused login, with the server's own words (async-imap wraps them in a
/// debug dump: `code: None, info: Some("…")`).
fn refused(e: async_imap::error::Error) -> anyhow::Error {
    use async_imap::error::Error;
    match &e {
        Error::No(s) | Error::Bad(s) => {
            let said = s.split_once("info: Some(\"").and_then(|(_, r)| r.rsplit_once("\")")).map(|(m, _)| m.replace("\\\"", "\""));
            anyhow!("login refused: {}", said.as_deref().unwrap_or(s))
        }
        _ => e.into(),
    }
}

async fn smtp(acc: &Account, mail: &MailStore) -> Result<lettre::AsyncSmtpTransport<lettre::Tokio1Executor>> {
    use lettre::transport::smtp::authentication::{Credentials, Mechanism};
    use lettre::{AsyncSmtpTransport, Tokio1Executor};
    let (host, port) = host_port(&acc.smtp, 465);
    let builder = if !acc.tls {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
    } else if port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)?
    }
    .port(port)
    .timeout(Some(Duration::from_secs(60)));
    Ok(match &acc.oauth {
        Some(_) => builder
            .credentials(Credentials::new(acc.login().into(), access_token(acc, mail).await?))
            .authentication(vec![Mechanism::Xoauth2]),
        None => builder.credentials(Credentials::new(acc.login().into(), acc.password.clone().unwrap_or_default())),
    }
    .build())
}

/// Relays a message through the account's SMTP server.
pub async fn send(acc: &Account, mail: &MailStore, from: &str, to: &[String], raw: &[u8]) -> Result<()> {
    if acc.retired.is_some() {
        bail!("{} is retired: valet no longer sends from it", acc.name);
    }
    let envelope = lettre::address::Envelope::new(
        Some(from.parse()?),
        to.iter().map(|t| t.parse()).collect::<Result<Vec<lettre::Address>, _>>()?,
    )?;
    smtp(acc, mail).await?.send_raw(&envelope, raw).await?;
    Ok(())
}

// OAuth (authorization code with PKCE)

fn endpoint(o: &OAuth, what: &str) -> String {
    format!("https://login.microsoftonline.com/{}/oauth2/v2.0/{what}", o.tenant)
}

/// A PKCE verifier and the browser address that starts the login.
pub fn authorize_url(acc: &Account, redirect: &str, state: &str) -> Result<(String, String)> {
    let o = acc.oauth.as_ref().context("this account doesn't use OAuth")?;
    let verifier = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
    let url = format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&response_mode=query&scope={}&state={}&code_challenge={challenge}&code_challenge_method=S256&login_hint={}",
        endpoint(o, "authorize"),
        enc(&o.client_id),
        enc(redirect),
        enc(&o.scopes),
        enc(state),
        enc(&acc.address)
    );
    Ok((verifier, url))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

async fn token_request(acc: &Account, mail: &MailStore, form: &[(&str, &str)], old_refresh: Option<&str>) -> Result<String> {
    let o = acc.oauth.as_ref().context("no oauth")?;
    let mut form: Vec<(&str, &str)> = form.to_vec();
    form.push(("client_id", &o.client_id));
    form.push(("scope", &o.scopes));
    if let Some(secret) = &o.client_secret {
        form.push(("client_secret", secret));
    }
    let resp = reqwest::Client::new().post(endpoint(o, "token")).form(&form).send().await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("token request refused: {}", body.chars().take(400).collect::<String>());
    }
    let t: TokenResponse = resp.json().await?;
    let refresh = t.refresh_token.as_deref().or(old_refresh).context("no refresh token (is offline_access in the scopes?)")?;
    mail.set_token(&acc.id, refresh, &t.access_token, now() + t.expires_in.unwrap_or(3600))?;
    Ok(t.access_token)
}

/// Finishes a login: trades the code for tokens.
pub async fn exchange_code(acc: &Account, mail: &MailStore, code: &str, verifier: &str, redirect: &str) -> Result<()> {
    token_request(
        acc,
        mail,
        &[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect), ("code_verifier", verifier)],
        None,
    )
    .await?;
    Ok(())
}

/// A valid access token, refreshed when it is about to expire.
pub async fn access_token(acc: &Account, mail: &MailStore) -> Result<String> {
    let (refresh, access, expires) = mail.token(&acc.id).with_context(|| format!("{} is not connected yet: log in from valet's Mail page", acc.name))?;
    if let (Some(a), Some(e)) = (access, expires)
        && e > now() + 120
    {
        return Ok(a);
    }
    token_request(acc, mail, &[("grant_type", "refresh_token"), ("refresh_token", &refresh)], Some(&refresh)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_accounts() {
        let json = r#"[{"id":"work","name":"Work","address":"a@b.fr","imap":"imap.ionos.fr:993","smtp":"smtp.ionos.fr:465","password":"x"},
            {"id":"study","name":"Epitech","address":"s@e.eu","imap":"outlook.office365.com:993","smtp":"smtp.office365.com:587",
             "oauth":{"client_id":"abc","redirect_uri":"https://localhost"}}]"#;
        let a: Vec<Account> = serde_json::from_str(json).unwrap();
        assert_eq!(a[0].history_days, 90);
        assert_eq!(a[1].oauth.as_ref().unwrap().tenant, "common");
        assert!(a[0].sends_as("A@B.fr"));
        let (verifier, url) = authorize_url(&a[1], "https://localhost", "st").unwrap();
        assert!(verifier.len() >= 43 && url.contains("code_challenge_method=S256") && url.contains("client_id=abc"));
        assert_eq!(host_port("smtp.office365.com:587", 465), ("smtp.office365.com".into(), 587));
        assert_eq!(xoauth2("u", "t"), "user=u\x01auth=Bearer t\x01\x01");
    }
}
