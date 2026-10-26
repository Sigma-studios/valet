//! Mail submission (RFC 6409) for mail apps: they hand valet a message, valet
//! relays it through the account it is from. Plain TCP: façade adds TLS.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::App;
use crate::accounts;
use crate::mime;

const MAX_SIZE: usize = 50 * 1024 * 1024;

pub async fn serve(app: Arc<App>, listener: TcpListener) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = session(app, stream).await {
                tracing::debug!(%peer, "smtp connection ended: {e:#}");
            }
        });
    }
}

/// A header value as an RFC 2047 encoded word when it isn't plain ASCII.
pub fn encode_header(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?utf-8?B?{}?=", base64::engine::general_purpose::STANDARD.encode(s))
    }
}

fn angle(arg: &str) -> String {
    let a = arg.trim();
    let a = a.split_once('>').map(|(x, _)| x).unwrap_or(a);
    a.trim_start_matches('<').trim().to_string()
}

/// The message without its Bcc header (recipients come from the envelope).
pub fn strip_bcc(raw: &[u8]) -> Vec<u8> {
    let p = mime::parse(raw);
    let mut out = Vec::with_capacity(raw.len());
    let mut pos = 0;
    for (name, s, e) in &p.fields {
        if name.eq_ignore_ascii_case("Bcc") {
            out.extend_from_slice(&raw[pos..*s]);
            pos = *e;
        }
    }
    out.extend_from_slice(&raw[pos..]);
    out
}

async fn session(app: Arc<App>, stream: TcpStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut authed = false;
    let mut from: Option<String> = None;
    let mut rcpts: Vec<String> = Vec::new();
    w.write_all(b"220 valet ESMTP ready\r\n").await?;
    macro_rules! reply {
        ($($t:tt)*) => { w.write_all(format!("{}\r\n", format!($($t)*)).as_bytes()).await? };
    }
    let read_line = async |r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>| -> Result<Option<String>> {
        let mut l = String::new();
        let n = tokio::time::timeout(Duration::from_secs(300), r.read_line(&mut l)).await??;
        Ok((n > 0).then(|| l.trim_end_matches(['\r', '\n']).to_string()))
    };
    while let Some(line) = read_line(&mut r).await? {
        let (verb, arg) = line.split_once(' ').unwrap_or((&line, ""));
        match verb.to_ascii_uppercase().as_str() {
            "EHLO" => {
                reply!("250-valet");
                reply!("250-AUTH PLAIN LOGIN");
                reply!("250-SIZE {MAX_SIZE}");
                reply!("250-PIPELINING");
                reply!("250 8BITMIME");
            }
            "HELO" => reply!("250 valet"),
            "AUTH" => {
                let (mech, initial) = arg.split_once(' ').unwrap_or((arg, ""));
                let (user, pass) = match mech.to_ascii_uppercase().as_str() {
                    "PLAIN" => {
                        let data = if initial.is_empty() {
                            reply!("334 ");
                            read_line(&mut r).await?.unwrap_or_default()
                        } else {
                            initial.to_string()
                        };
                        let d = base64::engine::general_purpose::STANDARD.decode(data.trim()).unwrap_or_default();
                        let parts: Vec<String> = d.split(|&b| b == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
                        match parts.as_slice() {
                            [_, u, p] => (u.clone(), p.clone()),
                            _ => Default::default(),
                        }
                    }
                    "LOGIN" => {
                        let b64 = |s: String| String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(s.trim()).unwrap_or_default()).into_owned();
                        let user = if initial.is_empty() {
                            reply!("334 VXNlcm5hbWU6");
                            b64(read_line(&mut r).await?.unwrap_or_default())
                        } else {
                            b64(initial.to_string())
                        };
                        reply!("334 UGFzc3dvcmQ6");
                        (user, b64(read_line(&mut r).await?.unwrap_or_default()))
                    }
                    _ => {
                        reply!("504 5.5.4 use PLAIN or LOGIN");
                        continue;
                    }
                };
                if app.config.check_password(&user, &pass) {
                    authed = true;
                    reply!("235 2.7.0 Authenticated");
                } else {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    reply!("535 5.7.8 Wrong user name or password");
                }
            }
            "MAIL" if !authed => reply!("530 5.7.0 Log in first"),
            "MAIL" => {
                let a = arg.split_once(':').map(|(_, a)| a).unwrap_or("");
                from = Some(angle(a));
                rcpts.clear();
                reply!("250 2.1.0 OK");
            }
            "RCPT" => {
                if from.is_none() {
                    reply!("503 5.5.1 MAIL first");
                    continue;
                }
                let a = angle(arg.split_once(':').map(|(_, a)| a).unwrap_or(""));
                if a.is_empty() {
                    reply!("501 5.1.3 bad recipient");
                } else {
                    rcpts.push(a);
                    reply!("250 2.1.5 OK");
                }
            }
            "DATA" => {
                if from.is_none() || rcpts.is_empty() {
                    reply!("503 5.5.1 MAIL and RCPT first");
                    continue;
                }
                reply!("354 Go ahead, end with <CRLF>.<CRLF>");
                let mut data = Vec::new();
                loop {
                    let mut l = Vec::new();
                    let n = tokio::time::timeout(Duration::from_secs(300), r.read_until(b'\n', &mut l)).await??;
                    if n == 0 {
                        bail!("closed during DATA");
                    }
                    if l == b".\r\n" || l == b".\n" {
                        break;
                    }
                    let l = if l.starts_with(b"..") { &l[1..] } else { &l[..] };
                    if data.len() + l.len() <= MAX_SIZE {
                        data.extend_from_slice(l);
                    }
                }
                let envelope_from = from.take().unwrap_or_default();
                let to = std::mem::take(&mut rcpts);
                match relay(&app, &envelope_from, &to, &data).await {
                    Ok(name) => reply!("250 2.0.0 Sent through {name}"),
                    Err(e) => {
                        tracing::warn!("relay failed: {e:#}");
                        reply!("554 5.0.0 {}", format!("{e:#}").replace(['\r', '\n'], " "));
                    }
                }
            }
            "RSET" => {
                from = None;
                rcpts.clear();
                reply!("250 2.0.0 OK");
            }
            "NOOP" => reply!("250 2.0.0 OK"),
            "QUIT" => {
                reply!("221 2.0.0 Bye");
                return Ok(());
            }
            "STARTTLS" => reply!("502 5.5.1 TLS is handled in front of valet"),
            _ => reply!("502 5.5.2 Unknown command"),
        }
    }
    Ok(())
}

/// Picks the account the message is from (envelope sender, else the From
/// header) and relays it there. Returns the account's name.
async fn relay(app: &App, envelope_from: &str, to: &[String], data: &[u8]) -> Result<String> {
    let header_from = {
        let p = mime::parse(data);
        p.header(data, "From").and_then(|f| mime::addresses(&f).into_iter().next()).map(|(_, m, h)| format!("{m}@{h}"))
    };
    let candidates = [Some(envelope_from.to_string()), header_from];
    let accounts = app.accounts();
    let (acc, sender) = candidates
        .iter()
        .flatten()
        .find_map(|addr| accounts.iter().find(|a| a.sends_now(addr)).or_else(|| accounts.iter().find(|a| a.sends_as(addr))).map(|a| (a, addr.clone())))
        .ok_or_else(|| anyhow::anyhow!("no account sends as {envelope_from}: add it to an account's aliases on valet's Mail page"))?;
    accounts::send(acc, &app.mail, &sender, to, &strip_bcc(data)).await?;
    Ok(acc.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(angle("<a@b.c> SIZE=100"), "a@b.c");
        assert_eq!(angle("a@b.c"), "a@b.c");
        assert_eq!(strip_bcc(b"From: a\r\nBcc: x@y,\r\n z@w\r\nTo: b\r\n\r\nhi"), b"From: a\r\nTo: b\r\n\r\nhi");
        assert_eq!(encode_header("Réunion"), "=?utf-8?B?UsOpdW5pb24=?=");
    }
}
