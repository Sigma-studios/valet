//! The mailboxes valet gathers: their settings (from `VALET_ACCOUNTS`),
//! logging in to their IMAP and SMTP servers, and OAuth for Microsoft 365.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use lettre::AsyncTransport;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::mailstore::{MailStore, now};

#[derive(Clone, Debug, Deserialize)]
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
}

fn default_days() -> u32 {
    90
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
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

    pub fn sends_as(&self, address: &str) -> bool {
        self.address.eq_ignore_ascii_case(address) || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(address))
    }
}

pub fn from_env() -> Result<Vec<Account>> {
    let Ok(json) = std::env::var("VALET_ACCOUNTS") else { return Ok(Vec::new()) };
    let accounts: Vec<Account> = serde_json::from_str(&json).context("VALET_ACCOUNTS is not valid JSON")?;
    for a in &accounts {
        if a.password.is_none() && a.oauth.is_none() {
            bail!("account {} needs a password or oauth", a.id);
        }
        if a.id.is_empty() || a.name.contains('/') {
            bail!("account {}: empty id or a / in its name", a.id);
        }
    }
    Ok(accounts)
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
            client.authenticate("XOAUTH2", XOAuth2(xoauth2(acc.login(), &token))).await.map_err(|(e, _)| e)?
        }
        None => client.login(acc.login(), acc.password.as_deref().unwrap_or("")).await.map_err(|(e, _)| e)?,
    };
    Ok(session)
}

/// Relays a message through the account's SMTP server.
pub async fn send(acc: &Account, mail: &MailStore, from: &str, to: &[String], raw: &[u8]) -> Result<()> {
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
    let transport = match &acc.oauth {
        Some(_) => builder
            .credentials(Credentials::new(acc.login().into(), access_token(acc, mail).await?))
            .authentication(vec![Mechanism::Xoauth2]),
        None => builder.credentials(Credentials::new(acc.login().into(), acc.password.clone().unwrap_or_default())),
    }
    .build();
    let envelope = lettre::address::Envelope::new(
        Some(from.parse()?),
        to.iter().map(|t| t.parse()).collect::<Result<Vec<lettre::Address>, _>>()?,
    )?;
    transport.send_raw(&envelope, raw).await?;
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
