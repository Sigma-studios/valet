//! Spelling and grammar checking of what is written in the composer:
//! LanguageTool's rules run in-process by nlprule (vendor/nlprule, with the
//! fixes for French, English and the speller), on data compiled from
//! LanguageTool 6.5 and embedded zstd-compressed (spell/*.bin.zst).
//!
//! The languages load on a thread at start (about 200 MB of memory each and
//! a second or two), so valet answers at once; until they are loaded a check
//! says so and the editor tries again later. `VALET_SPELL` lists the
//! languages (default `en,fr`; `de` too, about 340 MB more); `off` turns
//! checking off.

use std::sync::OnceLock;

use nlprule::{Rules, Tokenizer};
use serde::Serialize;

struct Lang {
    code: &'static str,
    tokenizer: Tokenizer,
    rules: Rules,
}

/// The languages valet can check, with their compiled data.
const DATA: &[(&str, &[u8], &[u8])] = &[
    ("en", include_bytes!("../spell/en_tokenizer.bin.zst"), include_bytes!("../spell/en_rules.bin.zst")),
    ("fr", include_bytes!("../spell/fr_tokenizer.bin.zst"), include_bytes!("../spell/fr_rules.bin.zst")),
    ("de", include_bytes!("../spell/de_tokenizer.bin.zst"), include_bytes!("../spell/de_rules.bin.zst")),
];

/// Rules switched off: they flag correct text (APOS_ESPACE/40 every "il y
/// a"; the others flag their own correct examples).
const DISABLED: &[(&str, &str)] = &[
    ("fr", "CAT_TYPOGRAPHIE/APOS_ESPACE/40"),
    ("fr", "AGREEMENT/AGREEMENT_POSTPONED_ADJ/0"),
    ("de", "CASING/AERGER_SUBST/2"),
    ("de", "CASING/DAS_BESTE_AM/1"),
    ("de", "CASING/GROSS_KLEIN_EINHEITEN/0"),
    ("de", "CASING/LEID_TUN/2"),
    ("de", "COMPOUNDING/HOCH_ZUSAMMENSCHREIBUNG/0"),
    ("de", "COMPOUNDING/ZU_MAL/0"),
    ("de", "CONFUSED_WORDS/VERSINKEN_VERSENKEN/0"),
    ("de", "GRAMMAR/DER_ABGEORDNETER/0"),
    ("de", "GRAMMAR/HILFSVERB_HABEN_SEIN/0"),
    ("de", "GRAMMAR/ZUM_FEM_NOMEN/1"),
    ("de", "HILFESTELLUNG_KOMMASETZUNG/KOMMA_ZWISCHEN_HAUPT_UND_NEBENSATZ/0"),
    ("de", "IDIOMS/ALLES_MOEGLICHE/0"),
    ("de", "MULTITOKEN_SPELLING/DE_MULTITOKEN_SPELLING_THREE/5"),
    ("de", "PUNCTUATION/APOSTROPH_S/0"),
    ("de", "PUNCTUATION/APOSTROPH_S/3"),
    ("de", "SEMANTICS/GESTERN_MIT_PRAESENS/0"),
    ("de", "TYPOS/DOPPELTES_VERB/0"),
];

/// Longest text checked at once (characters); the editor sends the draft.
pub const MAX_CHARS: usize = 100_000;

static LANGS: OnceLock<Vec<Lang>> = OnceLock::new();
static WANTED: OnceLock<Vec<String>> = OnceLock::new();

/// Starts loading the languages named by `VALET_SPELL` on a thread.
pub fn start() {
    let setting = std::env::var("VALET_SPELL").unwrap_or_else(|_| "en,fr".into());
    let wanted: Vec<String> = setting
        .split(',')
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty() && l != "off")
        .collect();
    let _ = WANTED.set(wanted.clone());
    if wanted.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut langs = Vec::new();
        for &(code, tokenizer, rules) in DATA {
            if !wanted.iter().any(|w| w == code) {
                continue;
            }
            match load(code, tokenizer, rules) {
                Ok(lang) => langs.push(lang),
                Err(e) => tracing::warn!("spelling: {code} not loaded: {e:#}"),
            }
        }
        for w in &wanted {
            if !DATA.iter().any(|(c, _, _)| c == w) {
                tracing::warn!("spelling: no data for language {w:?}");
            }
        }
        // the first check of each language builds lazy state (about a second)
        for lang in &langs {
            let _ = lang.rules.suggest("Hello. Bonjour.", &lang.tokenizer);
        }
        let codes: Vec<&str> = langs.iter().map(|l| l.code).collect();
        tracing::info!("spelling: {} ready in {:.1}s", codes.join(","), started.elapsed().as_secs_f64());
        let _ = LANGS.set(langs);
    });
}

fn load(code: &'static str, tokenizer: &[u8], rules: &[u8]) -> anyhow::Result<Lang> {
    let mut tokenizer = Tokenizer::from_reader(zstd::Decoder::new(tokenizer)?)?;
    match code {
        "en" => tokenizer.set_english_apostrophes(true),
        "fr" => tokenizer.set_french(true),
        _ => {}
    }
    // German's speller runs under de-DE only, as in LanguageTool
    let rules_lang = if code == "de" { "de-DE" } else { code };
    let mut rules = Rules::from_reader_with_lang(zstd::Decoder::new(rules)?, rules_lang)?;
    for (lang, id) in DISABLED {
        if *lang == code {
            let selector: nlprule::rule::id::Selector = (*id).try_into()?;
            rules.set_enabled_matching(&selector, false);
        }
    }
    Ok(Lang { code, tokenizer, rules })
}

/// Whether checking is turned on (it may still be loading).
pub fn enabled() -> bool {
    WANTED.get().is_some_and(|w| !w.is_empty())
}

/// A mistake in the checked text. `start`/`end` count UTF-16 code units, as
/// JavaScript strings do.
#[derive(Serialize, Debug, PartialEq)]
pub struct Mistake {
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub replacements: Vec<String>,
    pub rule: String,
    /// "spelling" or "grammar".
    pub kind: &'static str,
    pub lang: &'static str,
}

/// Checks `text`, each paragraph (line) in its own language. None until the
/// languages are loaded.
pub fn check(text: &str) -> Option<Vec<Mistake>> {
    let langs = LANGS.get()?;
    if langs.is_empty() {
        return Some(Vec::new());
    }
    Some(check_with(langs, text))
}

fn check_with(langs: &[Lang], text: &str) -> Vec<Mistake> {
    let codes: Vec<&str> = langs.iter().map(|l| l.code).collect();
    let fallback = guess(text, &codes, 1).unwrap_or(codes[0]);
    let mut out = Vec::new();
    // UTF-16 offset of the current paragraph's start
    let mut base16 = 0;
    for para in text.split('\n') {
        if para.trim().chars().count() > 1 {
            let code = guess(para, &codes, 2).unwrap_or(fallback);
            let lang = langs.iter().find(|l| l.code == code).expect("guessed among loaded languages");
            // UTF-16 offset of every char of the paragraph, and of its end
            let mut at16: Vec<usize> = Vec::with_capacity(para.len() + 1);
            let mut n = 0;
            for c in para.chars() {
                at16.push(n);
                n += c.len_utf16();
            }
            at16.push(n);
            for s in lang.rules.suggest(para, &lang.tokenizer) {
                let chars = s.span().char();
                let (Some(&start), Some(&end)) = (at16.get(chars.start), at16.get(chars.end)) else { continue };
                let rule = s.source().split('/').nth(1).unwrap_or(s.source()).to_string();
                let spelling = rule.contains("SPELL") || rule.contains("MORFOLOGIK");
                let message = if spelling {
                    match code {
                        "fr" => "Faute d’orthographe possible".to_string(),
                        "de" => "Möglicher Rechtschreibfehler".to_string(),
                        _ => "Possible spelling mistake".to_string(),
                    }
                } else {
                    s.message().to_string()
                };
                out.push(Mistake {
                    start: base16 + start,
                    end: base16 + end,
                    message,
                    replacements: s.replacements().iter().take(5).cloned().collect(),
                    rule,
                    kind: if spelling { "spelling" } else { "grammar" },
                    lang: lang.code,
                });
            }
        }
        base16 += para.chars().map(char::len_utf16).sum::<usize>() + 1;
    }
    out
}

/// The language of `text` among `codes`, from its common words and letters,
/// if one scores at least `min` above the others.
fn guess<'a>(text: &str, codes: &[&'a str], min: i32) -> Option<&'a str> {
    const EN: &[&str] = &[
        "the", "and", "is", "are", "to", "of", "you", "your", "i", "it", "that", "for", "with", "have", "this",
        "we", "be", "will", "on", "not", "can", "my", "me", "please", "thanks", "thank", "would", "hi", "hello",
        "regards", "best", "was", "were", "do", "if", "at", "our", "what", "all", "just", "let", "know",
    ];
    const FR: &[&str] = &[
        "le", "la", "les", "de", "des", "du", "et", "est", "un", "une", "je", "tu", "vous", "nous", "il", "elle",
        "pour", "que", "qui", "pas", "ne", "sur", "avec", "dans", "mais", "merci", "bonjour", "cordialement",
        "au", "aux", "ce", "c", "j", "l", "d", "qu", "n", "s", "on", "sont", "été", "très", "bien", "ou", "si",
    ];
    const DE: &[&str] = &[
        "der", "die", "das", "und", "ist", "nicht", "ich", "sie", "wir", "mit", "für", "auf", "zu", "ein",
        "eine", "den", "dem", "von", "es", "du", "danke", "hallo", "grüße", "bitte", "ihr", "auch", "sind",
    ];
    let mut scores: Vec<(&str, i32)> = codes.iter().map(|&c| (c, 0)).collect();
    for word in text.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()) {
        let w = word.to_lowercase();
        for (code, score) in scores.iter_mut() {
            let list: &[&str] = match *code {
                "en" => EN,
                "fr" => FR,
                "de" => DE,
                _ => &[],
            };
            if list.contains(&w.as_str()) {
                *score += 2;
            }
        }
    }
    for c in text.chars() {
        for (code, score) in scores.iter_mut() {
            match (*code, c) {
                ("fr", 'é' | 'è' | 'ê' | 'à' | 'ç' | 'ù' | 'œ' | 'â' | 'î' | 'ô' | 'û') => *score += 1,
                ("de", 'ä' | 'ö' | 'ü' | 'ß') => *score += 1,
                _ => {}
            }
        }
    }
    scores.sort_by(|a, b| b.1.cmp(&a.1));
    let best = scores[0];
    let second = scores.get(1).map_or(0, |s| s.1);
    (best.1 >= min && best.1 - second >= min).then_some(best.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs() -> Vec<Lang> {
        DATA.iter().filter(|(c, _, _)| *c != "de").map(|&(c, t, r)| load(c, t, r).unwrap()).collect()
    }

    #[test]
    fn german_when_asked() {
        let langs: Vec<Lang> = DATA.iter().map(|&(c, t, r)| load(c, t, r).unwrap()).collect();
        let last = "Vielen Dank für deine Hilfe, ich freue mich auf das Treffen.";
        let text = format!("Hallo Peter,\nwir haben uns lange nicht gesehn, ich hoffe es geht dir gut.\n{last}");
        let found = check_with(&langs, &text);
        let at = |m: &Mistake| String::from_utf16(&text.encode_utf16().skip(m.start).take(m.end - m.start).collect::<Vec<u16>>()).unwrap();
        let sp = found.iter().find(|m| at(m) == "gesehn").expect("German misspelling found");
        assert_eq!((sp.lang, sp.kind), ("de", "spelling"));
        assert_eq!(sp.replacements.first().map(String::as_str), Some("gesehen"));
        let last_line = text.encode_utf16().count() - last.encode_utf16().count();
        assert!(found.iter().all(|m| m.start < last_line), "the correct last line is clean: {found:?}");
    }

    #[test]
    fn guesses_the_language() {
        let codes = ["en", "fr"];
        assert_eq!(guess("Thanks for your message, I will send it to you.", &codes, 2), Some("en"));
        assert_eq!(guess("Merci pour ton message, je te l'envoie demain.", &codes, 2), Some("fr"));
        assert_eq!(guess("Antonin", &codes, 2), None);
    }

    #[test]
    fn finds_mistakes_in_each_language() {
        let langs = langs();
        let text = "Bonjour Marie,\nLes fleurs que j’ai acheté sont belles, il y a du soleil.\nThanks for you're help. I recieve it.\n— Antonin";
        let found = check_with(&langs, text);
        let at = |m: &Mistake| text.encode_utf16().skip(m.start).take(m.end - m.start).collect::<Vec<u16>>();
        let words: Vec<String> = found.iter().map(|m| String::from_utf16(&at(m)).unwrap()).collect();
        assert!(words.contains(&"acheté".to_string()), "{found:?}");
        assert!(words.contains(&"you're".to_string()), "{found:?}");
        assert!(words.contains(&"recieve".to_string()), "{found:?}");
        assert!(!words.iter().any(|w| w.contains("y a")), "il y a flagged: {found:?}");
        let fr = found.iter().find(|m| m.rule.contains("QUE_R_AVOIR") || m.rule.contains("QUE_AVOIR")).unwrap();
        assert_eq!(fr.lang, "fr");
        assert!(fr.replacements.contains(&"achetées".to_string()));
        let sp = found.iter().find(|m| m.kind == "spelling").unwrap();
        assert_eq!(sp.replacements.first().map(String::as_str), Some("receive"));
    }

    #[test]
    fn correct_text_is_clean() {
        let langs = langs();
        let text = "Bonjour Kasia,\nPourrions-nous nous voir jeudi à 15 h ? Dis-moi quand tu es disponible.\nI don't think that that is a problem, it's fine.";
        assert_eq!(check_with(&langs, text), Vec::new());
    }
}
