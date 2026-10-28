//! New mail, told. Each message come into an account's inbox (unread, and
//! not from the folder's first fetch) is an arrival. Browsers where
//! notifications are on get it at once by Web Push (RFC 8030: the payload
//! encrypted for the browser per RFC 8291, valet's request signed per
//! RFC 8292); valet's Android app, whose window can't receive Web Push, asks
//! for the arrivals since it last looked (`/mail/new.json`).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use aws_lc_rs::{aead, agreement, hmac, rand};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::Serialize;

use crate::App;
use crate::mailstore::now;
use crate::store::Push;

/// More at once than this (a phone back online): one note for them all.
const AT_ONCE: usize = 3;

/// A notification, as the service worker and the Android app show it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Note {
    /// `mail-<id>`, or `mail` for several: a later note of the same tag replaces it.
    pub tag: String,
    pub title: String,
    pub body: String,
    /// The account it came to (its name) when there are several, else empty.
    pub account: String,
    /// The page to open: the message beside the inbox.
    pub url: String,
    /// The message (its real row, to mark read), none for several.
    pub message: Option<i64>,
    /// The app's badge after it (see `MailStore::badge`), in pushes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge: Option<u32>,
}

/// Tells of messages just come into an inbox (real rows): noted for the
/// Android app, pushed to the browsers.
pub fn arrived(app: &Arc<App>, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    if let Err(e) = app.mail.add_arrivals(ids) {
        tracing::warn!("arrivals not noted: {e:#}");
    }
    let pushes = app.store.pushes();
    if pushes.is_empty() {
        return;
    }
    let badge = app.mail.badge();
    let notes: Vec<Note> = notes(app, ids).into_iter().map(|n| Note { badge: Some(badge), ..n }).collect();
    let app = app.clone();
    tokio::spawn(async move {
        futures::future::join_all(pushes.iter().map(|p| async {
            for n in &notes {
                if let Err(e) = send(&app, p, n).await {
                    tracing::warn!(device = %device(&p.agent), "push: {e:#}");
                    break;
                }
            }
        }))
        .await;
    });
}

/// What to show of these messages: one note each, or one for them all.
/// Those read or gone since are left out.
pub fn notes(app: &App, ids: &[i64]) -> Vec<Note> {
    let all: Vec<About> = ids.iter().filter_map(|&id| about(app, id)).collect();
    if all.len() <= AT_ONCE {
        return all
            .into_iter()
            .map(|a| Note {
                tag: format!("mail-{}", a.id),
                title: a.from,
                body: if a.snippet.is_empty() { a.subject } else { format!("{}\n{}", a.subject, a.snippet) },
                account: a.account,
                url: a.url,
                message: Some(a.id),
                badge: None,
            })
            .collect();
    }
    let mut body: Vec<String> = all.iter().take(5).map(|a| format!("{}: {}", a.from, a.subject)).collect();
    if all.len() > 5 {
        body.push(format!("…and {} more", all.len() - 5));
    }
    vec![Note { tag: "mail".into(), title: format!("{} new messages", all.len()), body: body.join("\n"), account: String::new(), url: "/inbox".into(), message: None, badge: None }]
}

struct About {
    id: i64,
    from: String,
    subject: String,
    snippet: String,
    account: String,
    url: String,
}

fn about(app: &App, id: i64) -> Option<About> {
    let m = app.mail.message(id)?;
    if m.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Seen")) {
        return None;
    }
    let raw = app.mail.blob(&m.blob)?;
    let p = mail_parser::MessageParser::default().parse(&raw)?;
    let from = p
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.name().filter(|n| !n.trim().is_empty()).or(a.address()))
        .unwrap_or("New message")
        .to_string();
    let subject = p.subject().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(no subject)");
    let account = if app.accounts().len() > 1 {
        app.mail.mailbox(m.mailbox).and_then(|b| b.account).and_then(|a| app.account(&a)).map(|a| a.name).unwrap_or_default()
    } else {
        String::new()
    };
    let url = match app.mail.row_in(app.mail.all_inboxes(), id) {
        Some(row) => format!("/inbox?open={row}"),
        None => format!("/msg/{id}"),
    };
    Some(About { id, from: cut(&from, 80), subject: cut(subject, 200), snippet: p.body_text(0).map(|t| snippet(&t)).unwrap_or_default(), account, url })
}

/// The first words of a message, quotes left out, on one line.
fn snippet(text: &str) -> String {
    let words: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with('>')).flat_map(str::split_whitespace).take(60).collect();
    cut(&words.join(" "), 160)
}

fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_string(),
    }
}

/// "Chrome on Android": a subscription's device, from its User-Agent.
pub fn device(agent: &str) -> String {
    let browser = [("Edg/", "Edge"), ("OPR/", "Opera"), ("Firefox/", "Firefox"), ("Chrome/", "Chrome"), ("Safari/", "Safari")]
        .iter()
        .find(|(k, _)| agent.contains(k))
        .map_or("A browser", |(_, v)| v);
    let os = [("Android", "Android"), ("iPhone", "iPhone"), ("iPad", "iPad"), ("Mac OS X", "macOS"), ("Windows", "Windows"), ("Linux", "Linux")]
        .iter()
        .find(|(k, _)| agent.contains(k))
        .map(|(_, v)| *v);
    match os {
        Some(os) => format!("{browser} on {os}"),
        None => browser.to_string(),
    }
}

// Web Push

/// valet's VAPID key (RFC 8292): made once, kept in the settings.
fn vapid(app: &App) -> &'static EcdsaKeyPair {
    static KEY: OnceLock<EcdsaKeyPair> = OnceLock::new();
    KEY.get_or_init(|| {
        let alg = &ECDSA_P256_SHA256_FIXED_SIGNING;
        let kept = app.store.setting("vapid_key").and_then(|k| B64.decode(k).ok());
        if let Some(k) = kept.and_then(|k| EcdsaKeyPair::from_pkcs8(alg, &k).ok()) {
            return k;
        }
        let doc = EcdsaKeyPair::generate_pkcs8(alg, &rand::SystemRandom::new()).expect("making a key");
        if let Err(e) = app.store.set_setting("vapid_key", &B64.encode(doc.as_ref())) {
            tracing::warn!("push key not kept: {e:#}");
        }
        EcdsaKeyPair::from_pkcs8(alg, doc.as_ref()).expect("reading the key just made")
    })
}

/// What browsers subscribe with (`applicationServerKey`): the VAPID public key.
pub fn public_key(app: &App) -> String {
    B64.encode(vapid(app).public_key().as_ref())
}

/// Who runs this valet, for push services to write to: the user's first
/// address, else the site's address.
fn contact(app: &App) -> String {
    match app.addresses().first() {
        Some(a) => format!("mailto:{a}"),
        None => app.store.setting("push_origin").unwrap_or_else(|| "mailto:valet@localhost".into()),
    }
}

/// A VAPID token: a JWT, signed ES256.
fn jwt(key: &EcdsaKeyPair, aud: &str, sub: &str, exp: i64) -> Result<String> {
    let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = B64.encode(serde_json::json!({ "aud": aud, "exp": exp, "sub": sub }).to_string());
    let signed = format!("{header}.{claims}");
    let sig = key.sign(&rand::SystemRandom::new(), signed.as_bytes()).map_err(|_| anyhow!("signing failed"))?;
    Ok(format!("{signed}.{}", B64.encode(sig.as_ref())))
}

fn hmac256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut ctx = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    for p in parts {
        ctx.update(p);
    }
    ctx.sign().as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// A push message's body (RFC 8291, `aes128gcm`), readable only by the
/// browser with `ua_public` and `auth`. `server` and `salt` are fresh for
/// each message.
fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8], server: &agreement::PrivateKey, salt: &[u8; 16]) -> Result<Vec<u8>> {
    let as_public = server.compute_public_key().map_err(|_| anyhow!("no public key"))?;
    let as_public = as_public.as_ref();
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let ecdh = agreement::agree(server, peer, anyhow!("the browser's key is not a P-256 key"), |s| Ok(s.to_vec()))?;
    let prk_key = hmac256(auth, &[&ecdh]);
    let ikm = hmac256(&prk_key, &[b"WebPush: info\0", ua_public, as_public, &[1]]);
    let prk = hmac256(salt, &[&ikm]);
    let cek = hmac256(&prk, &[b"Content-Encoding: aes128gcm\0", &[1]]);
    let nonce = hmac256(&prk, &[b"Content-Encoding: nonce\0", &[1]]);
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek[..16]).map_err(|_| anyhow!("bad key"))?);
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce[..12]).map_err(|_| anyhow!("bad nonce"))?;
    // One record: the plaintext, then the last record's delimiter.
    let mut data = plaintext.to_vec();
    data.push(2);
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut data).map_err(|_| anyhow!("encryption failed"))?;
    let mut out = Vec::with_capacity(21 + as_public.len() + data.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&4096u32.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&data);
    Ok(out)
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("an HTTP client"))
}

/// Sends `note` to one browser, through its push service. A subscription
/// the service says is gone (the browser dropped it) is forgotten.
pub async fn send(app: &App, p: &Push, note: &Note) -> Result<()> {
    let ua_public = B64.decode(p.p256dh.trim_end_matches('=')).context("the browser's key")?;
    let auth = B64.decode(p.auth.trim_end_matches('=')).context("the browser's secret")?;
    anyhow::ensure!(auth.len() == 16, "the browser's secret is {} bytes, not 16", auth.len());
    let server = agreement::PrivateKey::generate(&agreement::ECDH_P256).map_err(|_| anyhow!("making a key"))?;
    let mut salt = [0u8; 16];
    rand::fill(&mut salt).map_err(|_| anyhow!("no randomness"))?;
    let body = encrypt(&ua_public, &auth, &serde_json::to_vec(note)?, &server, &salt)?;
    let url = reqwest::Url::parse(&p.endpoint)?;
    let token = jwt(vapid(app), &url.origin().ascii_serialization(), &contact(app), now() + 12 * 3600)?;
    let resp = client()
        .post(url)
        // Mail older than six hours isn't news: a device off that long gets the list instead.
        .header("TTL", "21600")
        .header("Urgency", "high")
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", format!("vapid t={token}, k={}", public_key(app)))
        .body(body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    if status.as_u16() == 404 || status.as_u16() == 410 {
        app.store.delete_push(&p.endpoint)?;
        bail!("the browser dropped this subscription ({status}): turn notifications on again there");
    }
    let text = resp.text().await.unwrap_or_default();
    bail!("the push service answered {status}: {}", cut(text.trim(), 300))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    fn b64(s: &str) -> Vec<u8> {
        B64.decode(s.replace(' ', "")).unwrap()
    }

    /// RFC 8291, section 5 and appendix A.
    #[test]
    fn encrypts_as_the_rfc_example() {
        let server = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, &b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let ua_public = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = encrypt(&ua_public, &b64("BTBZMqHH6r4Tts7J_aSIgg"), b"When I grow up, I want to be a watermelon", &server, &salt).unwrap();
        let want = [
            b64("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
            b64("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"),
        ]
        .concat();
        assert_eq!(body, want);
    }

    #[test]
    fn vapid_token_verifies_with_the_public_key() {
        let alg = &ECDSA_P256_SHA256_FIXED_SIGNING;
        let doc = EcdsaKeyPair::generate_pkcs8(alg, &rand::SystemRandom::new()).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(alg, doc.as_ref()).unwrap();
        assert_eq!(key.public_key().as_ref().len(), 65, "uncompressed point, as browsers want");
        let t = jwt(&key, "https://fcm.googleapis.com", "mailto:me@example.com", 1_900_000_000).unwrap();
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value = serde_json::from_slice(&b64(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_900_000_000);
        let signed = format!("{}.{}", parts[0], parts[1]);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_key().as_ref()).verify(signed.as_bytes(), &b64(parts[2])).unwrap();
    }

    #[test]
    fn snippets_and_devices() {
        assert_eq!(snippet("Hi Ann,\n\n> quoted\n> more\nSee you   at 5.\n"), "Hi Ann, See you at 5.");
        assert_eq!(cut("abcdef", 3), "abc…");
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(device("Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/130.0 Mobile Safari/537.36"), "Chrome on Android");
        assert_eq!(device("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1"), "Safari on iPhone");
        assert_eq!(device("Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:131.0) Gecko/20100101 Firefox/131.0"), "Firefox on macOS");
    }
}
