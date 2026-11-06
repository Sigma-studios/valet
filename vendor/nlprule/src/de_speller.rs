//! German spelling (`GERMAN_SPELLER_RULE`), closer to LT's GermanSpellerRule
//! than the generic word-list speller:
//!
//! - detection: LT's own hunspell dictionary (`de_DE.aff`/`.dic`, with its
//!   compound flags) checked by the pure-Rust `spellbook` crate, plus the
//!   case-aware word list (de_DE.dict dump + LT's spelling lists). The word
//!   list alone is case-insensitive and can't tell real compounds from
//!   typos ("gesehn" = "ge" + "sehn"), so it accepted almost everything.
//! - two-part noun compounds hunspell doesn't know are accepted like LT's
//!   compound splitter does ("Verspätungsgrund"), capitalized words only.
//! - suggestions: case fixes first ("rechnung" -> "Rechnung"), then LT's
//!   de_DE.info replacement pairs ("grosse" -> "große"), then edit-distance
//!   candidates ranked by frequency.
//!
//! The hunspell files come from the rules binary (compiled in from
//! `filters/hunspell/`) or, for older binaries, from a `de_hunspell/`
//! directory next to it (LT's de_DE.aff/.dic, ignore.txt, spelling.txt,
//! prohibit.txt). Without them the case-aware word list is used alone.

use crate::rule::filter_data::SpellerDict;
use once_cell::sync::OnceCell;

struct DeDict {
    dict: spellbook::Dictionary,
    /// prohibit.txt `.*stem` patterns: any compound ending in `stem`
    /// (inflected with a usual noun ending) is an error
    prohibited_suffixes: Vec<String>,
    /// prohibit.txt words: hunspell's own compounding would accept many
    /// of them
    prohibited_words: std::collections::HashSet<String>,
}

static DICT: OnceCell<Option<DeDict>> = OnceCell::new();

fn load(speller: &SpellerDict) -> Option<&'static DeDict> {
    DICT.get_or_init(|| {
        let data = speller.hunspell.as_ref().filter(|d| d.is_present())?;
        let aff = std::str::from_utf8(&data.aff).ok()?;
        let dic = std::str::from_utf8(&data.dic).ok()?;
        let mut dict = spellbook::Dictionary::new(aff, dic).ok()?;
        let mut prohibited_suffixes = Vec::new();
        let mut prohibited_words = std::collections::HashSet::new();
        for entry in &data.extra_words {
            if let Some(p) = entry.strip_prefix('!') {
                if let Some(stem) = p.strip_prefix(".*") {
                    let stem = stem.split('/').next().unwrap_or("");
                    if !stem.is_empty() && !stem.contains(|c: char| "[]().*?+|\\".contains(c)) {
                        prohibited_suffixes.push(stem.to_string());
                    }
                } else if !p.contains(|c: char| "[]()*?+|\\".contains(c)) {
                    // the stem only: guessing the forms of its affix flags
                    // prohibits real words ("Buchstab/EN" is not "Buchstabe")
                    let stem = p.split('/').next().unwrap_or(p);
                    prohibited_words.insert(stem.to_string());
                }
            } else if !entry.contains('*') {
                let _ = dict.add(entry);
            }
        }
        Some(DeDict { dict, prohibited_suffixes, prohibited_words })
    })
    .as_ref()
}

/// Parse the hunspell dictionary now (~0.3 s with LT's word lists) rather
/// than on the first check. Returns whether it is available.
pub(crate) fn warm_up(speller: &SpellerDict) -> bool {
    load(speller).is_some()
}

/// LT's German speller files from a `hunspell/` directory: de_DE.aff and
/// .dic, plus the words LT adds (ignore.txt, spelling.txt,
/// spelling_custom.txt, flags kept for hunspell) and the prohibited ones
/// (prohibit.txt, prohibit_custom.txt), stored with a leading '!'.
pub fn hunspell_data_from_dir(dir: &std::path::Path) -> Option<crate::hunspell::HunspellData> {
    let aff = std::fs::read(dir.join("de_DE.aff")).ok()?;
    let dic = std::fs::read(dir.join("de_DE.dic")).ok()?;
    let mut extra_words = Vec::new();
    let lists = [
        ("ignore.txt", ""),
        ("spelling.txt", ""),
        ("spelling_custom.txt", ""),
        ("prohibit.txt", "!"),
        ("prohibit_custom.txt", "!"),
    ];
    for (name, prefix) in lists {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            for line in text.lines() {
                let w = line.split('#').next().unwrap_or("").trim();
                if !w.is_empty() {
                    extra_words.push(format!("{prefix}{w}"));
                }
            }
        }
    }
    Some(crate::hunspell::HunspellData { aff, dic, extra_words })
}

/// de_DE.info `fsa.dict.speller.replacement-pairs` (LT 6.5): one pair
/// counts as a single edit in morfologik's suggestion search.
const REPLACEMENT_PAIRS: &[(&str, &str)] = &[
    ("ß", "ss"), ("ss", "ß"), ("B", "ß"), ("s", "ß"), ("ß", "s"), ("z", "ß"), ("z", "s"),
    ("s", "z"), ("ai", "ei"), ("ei", "ai"), ("oi", "eu"), ("oi", "äu"), ("eu", "oi"),
    ("eu", "äu"), ("äu", "eu"), ("a", "ä"), ("o", "ö"), ("u", "ü"), ("ae", "ä"), ("e", "ä"),
    ("ä", "e"), ("e", "é"), ("ee", "é"), ("é", "ee"), ("oe", "ö"), ("ö", "oe"), ("o", "oo"),
    ("ue", "ü"), ("Ae", "Ä"), ("Oe", "Ö"), ("Ue", "Ü"), ("q", "qu"), ("Rh", "R"), ("R", "Rh"),
    ("rh", "r"), ("th", "t"), ("t", "th"), ("T", "Th"), ("Th", "T"), ("g", "ch"), ("ch", "g"),
    ("f", "ph"), ("ph", "f"), ("Ph", "F"), ("F", "Ph"), ("f", "pf"), ("pf", "f"), ("Pf", "F"),
    ("F", "Pf"), ("mft", "nft"), ("F", "V"), ("W", "V"), ("f", "v"), ("v", "f"), ("w", "v"),
    ("i", "y"), ("j", "y"), ("ü", "y"), ("J", "Y"), ("x", "chs"), ("sch", "chs"), ("sch", "g"),
    ("Sch", "G"), ("scht", "st"), ("Scht", "St"), ("schp", "sp"), ("schp", "Sp"),
    ("Schp", "Sp"), ("schb", "sp"), ("schb", "Sp"), ("Schb", "Sp"), ("d", "t"), ("t", "d"),
    ("b", "p"), ("p", "b"), ("g", "k"), ("k", "g"), ("gt", "ckt"), ("z", "c"), ("c", "z"),
    ("k", "c"), ("c", "k"), ("k", "ck"), ("ck", "k"), ("ck", "kk"), ("kk", "k"), ("ks", "x"),
    ("x", "ks"), ("xi", "kti"), ("kz", "kti"), ("o", "oh"), ("oh", "o"), ("u", "ou"),
    ("ou", "u"), ("ug", "ung"),
];

/// Words after which a capitalized unknown word is taken for a name in
/// mail ("Hallo Kasia", "Frau Wojciechowska"). LT flags such names.
const NAME_CUES: &[&str] = &[
    "Hallo", "hallo", "Hi", "hi", "Hey", "hey", "Liebe", "liebe", "Lieber", "lieber", "Liebes",
    "Herr", "Herrn", "Frau", "Dr", "Prof", "Moin", "moin", "Servus", "Grüß", "Ciao", "Gruß",
    "Grüße", "Dank", "Danke", "danke",
];

fn uppercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn lowercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

fn starts_upper(s: &str) -> bool {
    s.chars().next().map_or(false, char::is_uppercase)
}

/// A capitalized dictionary form (a noun, in German).
fn known_noun(word: &str, speller: &SpellerDict) -> bool {
    starts_upper(word) && speller.variants(word).iter().any(|v| *v == word)
}

/// LT's GermanSpellerRule accepts noun compounds its splitter can cut into
/// a known noun (optionally with a linking s/es/n/en/e) and a known noun
/// ("Verspätungs|grund", "Treff|punkt"). Both parts need 3+ letters; only
/// capitalized words, and only one cut (hunspell covers longer compounds).
fn noun_compound(word: &str, speller: &SpellerDict) -> bool {
    if !starts_upper(word) || word.chars().count() < 7 || word.contains('-') {
        return false;
    }
    let idx: Vec<usize> = word.char_indices().map(|(i, _)| i).collect();
    for k in 3..idx.len().saturating_sub(3) {
        let (head, tail) = word.split_at(idx[k]);
        if tail.chars().next().map_or(true, |c| !c.is_lowercase()) {
            continue;
        }
        if !known_noun(&uppercase_first(tail), speller) {
            continue;
        }
        if known_noun(head, speller) {
            return true;
        }
        for linker in ["s", "es", "n", "en", "e", "er"] {
            if let Some(h) = head.strip_suffix(linker) {
                if h.chars().count() >= 3 && known_noun(h, speller) {
                    return true;
                }
            }
        }
        // verb stem as first part ("Schreib|tisch", "Treff|punkt"): the
        // stem + "en" is a known lowercase infinitive
        let lower = lowercase_first(head);
        if speller.variants(&format!("{lower}en")).iter().any(|v| *v == format!("{lower}en")) {
            return true;
        }
    }
    false
}

fn prohibited(word: &str, de: &DeDict) -> bool {
    de.prohibited_words.contains(word)
        || de.prohibited_suffixes.iter().any(|stem| {
        word.len() > stem.len()
            && ["", "e", "en", "n", "s", "es", "er", "ern"].iter().any(|end| {
                word.strip_suffix(end).map_or(false, |w| w.ends_with(stem.as_str()))
            })
    })
}

/// Whether `word` is spelled correctly. With LT's hunspell files, hunspell
/// decides (like LT; the word list also holds forms LT rejects, e.g.
/// lowercase "bescheid"); without them, the case-aware word list.
pub(crate) fn is_correct(word: &str, speller: &SpellerDict) -> bool {
    match load(speller) {
        Some(de) => {
            if prohibited(word, de) {
                return false;
            }
            if de.dict.check(word) {
                return true;
            }
        }
        None => {
            if speller.is_known_cased(word) {
                return true;
            }
        }
    }
    // gender-inclusive plurals: "MitarbeiterInnen", "Kolleg*innen" and
    // "Kolleg:innen" when the tokenizer keeps them whole
    for suffix in ["Innen", "In", "*innen", "*in", ":innen", ":in", "_innen", "_in"] {
        if let Some(stem) = word.strip_suffix(suffix) {
            if !stem.is_empty() && (speller.is_known_cased(stem) || noun_compound(stem, speller)) {
                return true;
            }
        }
    }
    noun_compound(word, speller)
}

/// Whether a capitalized unknown word right after `prev` is a name.
pub(crate) fn is_name_position(word: &str, prev: Option<&str>) -> bool {
    starts_upper(word) && prev.map_or(false, |p| NAME_CUES.contains(&p.trim_end_matches('.')))
}

/// Ranked corrections for a misspelled word. `similar` gives the
/// edit-distance candidates of one tier (1, 2).
pub(crate) fn suggestions(
    word: &str,
    speller: &SpellerDict,
    similar: impl Fn(usize) -> Vec<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |out: &mut Vec<String>, w: String| {
        if w != word && !w.contains('/') && !w.starts_with('#') && !out.contains(&w) {
            out.push(w);
        }
    };
    // 1. the same word in another case ("rechnung" -> "Rechnung")
    for v in speller.variants(word) {
        if v != word && is_correct(v, speller) {
            push(&mut out, v.to_string());
        }
    }
    // candidates are checked without the compound heuristic (cheap)
    let de = load(speller);
    let known = |w: &str| match de {
        Some(de) => !prohibited(w, de) && de.dict.check(w),
        None => speller.is_known_cased(w),
    };
    // 2. the usual German typos first, ranked by frequency: LT's
    // replacement pairs ("grosse" -> "große", "Packet" -> "Paket"), one or
    // (short words) two of them, and a missing or extra double letter
    // ("Kanst" -> "Kannst", "Vieleicht" -> "Vielleicht")
    let mut typo_hits: Vec<String> = Vec::new();
    let mut frontier = vec![word.to_string()];
    for round in 0..2 {
        let mut next: Vec<String> = Vec::new();
        for w in &frontier {
            for (from, to) in REPLACEMENT_PAIRS {
                for (i, _) in w.match_indices(from) {
                    let cand = format!("{}{}{}", &w[..i], to, &w[i + from.len()..]);
                    if cand != word && !next.contains(&cand) {
                        next.push(cand);
                    }
                }
            }
            if round == 0 {
                let chars: Vec<(usize, char)> = w.char_indices().collect();
                for (k, &(i, c)) in chars.iter().enumerate() {
                    if !c.is_alphabetic() {
                        continue;
                    }
                    let end = i + c.len_utf8();
                    if chars.get(k + 1).map_or(false, |&(_, n)| n == c) {
                        next.push(format!("{}{}", &w[..i], &w[end..]));
                    } else if k > 0 || !c.is_uppercase() {
                        next.push(format!("{}{}{}", &w[..end], c, &w[end..]));
                    }
                }
            }
        }
        for cand in &next {
            if !typo_hits.contains(cand) && known(cand) {
                typo_hits.push(cand.clone());
            }
        }
        if !typo_hits.is_empty() || word.chars().count() > 12 {
            break;
        }
        frontier = next;
    }
    // pre-1996 spelling ("daß", "wußte"): the ss form first
    let reformed = word.replace('ß', "ss");
    typo_hits.sort_by_key(|w| (*w != reformed, std::cmp::Reverse(speller.freq_of(w))));
    for w in typo_hits {
        push(&mut out, w);
    }
    // 3. edit distance 1 (2 if nothing else was found), by frequency
    let mut tier: Vec<String> = Vec::new();
    for d in 1..=2 {
        tier = similar(d);
        if !tier.is_empty() || !out.is_empty() {
            break;
        }
    }
    let mut tier: Vec<(usize, String)> = tier
        .into_iter()
        .filter(|w| de.map_or(true, |de| !prohibited(w, de)))
        .enumerate()
        .collect();
    tier.sort_by_key(|(i, w)| (std::cmp::Reverse(speller.freq_of(w)), *i));
    for (_, w) in tier {
        push(&mut out, w);
    }
    out.truncate(8);
    out
}
