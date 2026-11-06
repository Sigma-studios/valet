//! Port of LanguageTool's `FrenchWordTokenizer` (LT 6.5).
//!
//! A word chunk keeps the apostrophes between letters and its hyphens, then is
//! split the way LT does: elisions (`j'ai` -> `j'` `ai`), verb + pronoun suffixes
//! (`Pourrions-nous` -> `Pourrions` `-nous`, `a-t-il` -> `a` `-t-il`), and any
//! other hyphenated word stays whole only if the dictionary knows it
//! (`grand-mère`, `peut-être`), else splits at its hyphens.
//!
//! Every returned token is a slice of the input, so spans stay exact.

const ELISIONS: &[&str] = &[
    "c'", "ç'", "j'", "n'", "m'", "t'", "s'", "l'", "d'", "qu'", "jusqu'", "lorsqu'", "puisqu'",
    "quoiqu'",
];

const SUFFIXES: &[&str] = &[
    "-ce", "-elle", "-t-elle", "-elles", "-t-elles", "-en", "-il", "-t-il", "-ils", "-t-ils",
    "-je", "-la", "-le", "-les", "-leur", "-lui", "-moi", "-nous", "-on", "-t-on", "-toi", "-tu",
    "-vous", "-vs", "-y",
];

const T_SUFFIXES: &[&str] = &["-t-elle", "-t-elles", "-t-il", "-t-ils", "-t-on"];

const WHOLE: &[&str] = &[
    "c'te", "c't", "m'as-tu-vu", "c'est-à-dire", "add-on", "add-ons", "rendez-vous",
    "garde-à-vous", "chez-eux", "chez-moi", "chez-nous", "chez-soi", "chez-toi", "chez-vous",
];

const DO_NOT_SPLIT: &[&str] = &[
    "mers-cov", "mcgraw-hill", "sars-cov-2", "sars-cov", "ph-metre", "ph-metres", "anti-ivg",
    "anti-uv", "anti-vih", "al-qaïda", "c'est-à-dire", "add-on", "add-ons", "rendez-vous",
    "garde-à-vous", "chez-eux", "chez-moi", "chez-nous", "chez-soi", "chez-toi", "chez-vous",
    "m'as-tu-vu",
];

/// Lowercase with typographic apostrophes as typewriter ones (the form LT's
/// patterns and dictionary use). Same byte length per char is NOT guaranteed,
/// so callers compare whole strings only, never index across the two.
fn norm(s: &str) -> String {
    s.to_lowercase().replace('\u{2019}', "'")
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c,
            '§' | '©' | '@' | '€' | '£' | '$' | '_' | '-' | '\u{00A8}' | '°' | '%' | '‰' | '‱'
            | '&' | '\u{FFFD}' | '\u{00AD}' | '\u{00AC}' | '\u{2010}' | '\u{2011}')
        || ('\u{0300}'..='\u{036F}').contains(&c)
        || ('\u{2070}'..='\u{209F}').contains(&c)
}

/// Split a sentence (or a part of it) into token slices, LT French style.
pub(crate) fn split_text<'t>(text: &'t str, known: &dyn Fn(&str) -> bool) -> Vec<&'t str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut run_start: Option<usize> = None;
    for (k, &(i, c)) in chars.iter().enumerate() {
        let prev = if k > 0 { Some(chars[k - 1].1) } else { None };
        let next = chars.get(k + 1).map(|x| x.1);
        let in_word = is_word_char(c)
            || ((c == '\'' || c == '\u{2019}')
                && prev.map_or(false, char::is_alphabetic)
                && next.map_or(false, |n| n.is_alphabetic() || matches!(n, '1' | '"' | '‘' | '“' | '«')))
            || ((c == '.' || c == ',')
                && prev.map_or(false, |p| p.is_ascii_digit())
                && next.map_or(false, |n| n.is_ascii_digit()));
        if in_word {
            if run_start.is_none() {
                run_start = Some(i);
            }
            continue;
        }
        if let Some(s) = run_start.take() {
            split_word(&text[s..i], known, &mut out);
        }
        if !c.is_whitespace() {
            out.push(&text[i..i + c.len_utf8()]);
        }
    }
    if let Some(s) = run_start {
        split_word(&text[s..], known, &mut out);
    }
    out
}

fn split_word<'t>(word: &'t str, known: &dyn Fn(&str) -> bool, out: &mut Vec<&'t str>) {
    let mut s = word;
    while s.len() > 1 && s.starts_with('-') {
        out.push(&s[..1]);
        s = &s[1..];
    }
    let mut trailing = 0;
    while s.len() > 1 && s.ends_with('-') {
        s = &s[..s.len() - 1];
        trailing += 1;
    }
    match match_patterns(s) {
        Some(groups) => {
            for g in groups {
                words_to_add(g, known, out);
            }
        }
        None => words_to_add(s, known, out),
    }
    for t in 0..trailing {
        out.push(&word[word.len() - trailing + t..word.len() - trailing + t + 1]);
    }
}

/// The elision prefix of `s` (byte length in `s`), if any. Longest first so
/// `jusqu'` wins over nothing; all forms end with an apostrophe.
fn elision_len(s: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if c == '\'' || c == '\u{2019}' {
            let end = i + c.len_utf8();
            if ELISIONS.contains(&norm(&s[..end]).as_str()) {
                best = Some(end);
            }
            break; // elisions end at the first apostrophe
        }
    }
    best
}

fn is_suffix(s: &str) -> bool {
    SUFFIXES.contains(&norm(s).as_str())
}

/// LT patterns[0..6] in order; returns the capture groups as slices.
fn match_patterns(s: &str) -> Option<Vec<&str>> {
    let lower = norm(s);
    // [0] words never split
    if WHOLE.contains(&lower.as_str()) {
        return Some(vec![s]);
    }
    let el = elision_len(s);
    // [1] elision + middle without hyphen + one pronoun suffix ("l'a-t-il")
    if let Some(e) = el {
        let rest = &s[e..];
        if let Some(h) = rest.find('-') {
            if is_suffix(&rest[h..]) {
                return Some(vec![&s[..e], &rest[..h], &rest[h..]]);
            }
        }
    }
    // [2] elision + rest not starting with an apostrophe or hyphen ("j'ai")
    if let Some(e) = el {
        let rest = &s[e..];
        if let Some(first) = rest.chars().next() {
            if !matches!(first, '\'' | '\u{2019}' | '-') {
                return Some(vec![&s[..e], rest]);
            }
        }
    }
    // [3] base without hyphen/digit + two pronoun suffixes ("donne-le-moi")
    if let Some(h) = s.find('-') {
        let base = &s[..h];
        if !base.is_empty() && !base.chars().any(|c| c.is_ascii_digit()) {
            let rest = &s[h..];
            for (j, c) in rest.char_indices().skip(1) {
                if c == '-' && is_suffix(&rest[..j]) && is_suffix(&rest[j..]) {
                    return Some(vec![base, &rest[..j], &rest[j..]]);
                }
            }
        }
    }
    // [4] base + "-t"/"-m" + "'en"/"'y" ("va-t'en")
    if let Some(h) = s.find('-') {
        let base = &s[..h];
        let rest = &s[h..];
        for mid in ["-t", "-m"] {
            if rest.len() > 2 && rest.get(..2).map_or(false, |p| p.eq_ignore_ascii_case(mid)) {
                let tail = &rest[2..];
                if matches!(norm(tail).as_str(), "'en" | "'y") {
                    return Some(vec![base, &rest[..2], tail]);
                }
            }
        }
    }
    // [5] (.*)(-t-il|...) and [6] (.*)(pronoun suffix): the greedy prefix
    // means the shortest matching suffix wins
    for list in [T_SUFFIXES, SUFFIXES] {
        let mut hyphens: Vec<usize> = s.match_indices('-').map(|(i, _)| i).collect();
        hyphens.reverse();
        for h in hyphens {
            if list.contains(&norm(&s[h..]).as_str()) {
                return Some(vec![&s[..h], &s[h..]]);
            }
        }
    }
    None
}

/// LT `wordsToAdd`: a hyphenated word is kept whole if tagged (or listed),
/// else split at every hyphen, keeping the hyphens as tokens.
fn words_to_add<'t>(s: &'t str, known: &dyn Fn(&str) -> bool, out: &mut Vec<&'t str>) {
    if s.is_empty() {
        return;
    }
    if !s.contains('-') {
        out.push(s);
        return;
    }
    let normalized = s.replace('\u{00AD}', "").replace('\u{2019}', "'");
    if known(&normalized) || DO_NOT_SPLIT.contains(&s.to_lowercase().as_str()) {
        out.push(s);
        return;
    }
    let mut start = 0;
    for (i, _) in s.match_indices('-') {
        if i > start {
            out.push(&s[start..i]);
        }
        out.push(&s[i..i + 1]);
        start = i + 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &str) -> Vec<&str> {
        let known = |w: &str| {
            matches!(w.to_lowercase().as_str(), "grand-mère" | "peut-être" | "-nous" | "-moi" | "-ce" | "-t-il" | "-le" | "-t")
        };
        split_text(s, &known)
    }

    #[test]
    fn french_tokens() {
        assert_eq!(tok("Les fleurs que j'ai achetées."), ["Les", "fleurs", "que", "j'", "ai", "achetées", "."]);
        assert_eq!(tok("j’ai"), ["j’", "ai"]);
        assert_eq!(tok("Pourrions-nous partir ?"), ["Pourrions", "-nous", "partir", "?"]);
        assert_eq!(tok("Est-ce que"), ["Est", "-ce", "que"]);
        assert_eq!(tok("a-t-il"), ["a", "-t-il"]);
        assert_eq!(tok("l'a-t-il vu"), ["l'", "a", "-t-il", "vu"]);
        assert_eq!(tok("donne-le-moi"), ["donne", "-le", "-moi"]);
        assert_eq!(tok("va-t'en"), ["va", "-t", "'en"]);
        assert_eq!(tok("ma grand-mère"), ["ma", "grand-mère"]);
        assert_eq!(tok("anti-gaspi"), ["anti", "-", "gaspi"]);
        assert_eq!(tok("aujourd'hui"), ["aujourd'hui"]);
        assert_eq!(tok("rendez-vous"), ["rendez-vous"]);
        assert_eq!(tok("3,5 kg"), ["3,5", "kg"]);
        assert_eq!(tok("« Oui »"), ["«", "Oui", "»"]);
        assert_eq!(tok("l'"), ["l", "'"]);
        assert_eq!(tok("les-États-Unis"), ["les", "-", "États", "-", "Unis"]);
        assert_eq!(tok("-États-Unis"), ["-", "États", "-", "Unis"]);
        assert_eq!(tok("Pourrions-NOUS"), ["Pourrions", "-NOUS"]);
    }
}
