//! Serializable data used by the Java-class runtime filters
//! ([crate::rule::filter_java]): speller dictionaries, multitoken
//! suggestion tables, confusion pairs and compound part lists.
//!
//! All of this is built at compile time from LanguageTool's resources and
//! stored once per language (on [crate::rules::Rules]), mirroring how LT
//! keeps these as static instances in the filter classes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A speller vocabulary: an FST over lowercased word forms mapping to the
/// index of one original-cased form. Approximates LT's `MorfologikSpeller`.
/// The speller's forms, packed in one string (serialized as the list of
/// them, so binaries are unchanged): a `Vec<String>` costs ~40 bytes a form.
#[derive(Default, Clone, Debug)]
pub struct Forms {
    text: String,
    ends: Vec<u32>,
}

impl Forms {
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&str> {
        let end = *self.ends.get(i)? as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        self.text.get(start..end)
    }

    /// Forms `first..first + count` (fewer past the end).
    pub fn range(&self, first: usize, count: usize) -> Vec<&str> {
        (first..first + count).map_while(|i| self.get(i)).collect()
    }
}

impl From<Vec<String>> for Forms {
    fn from(list: Vec<String>) -> Forms {
        let mut f = Forms { text: String::with_capacity(list.iter().map(String::len).sum()), ends: Vec::with_capacity(list.len()) };
        for w in &list {
            f.text.push_str(w);
            f.ends.push(f.text.len() as u32);
        }
        f
    }
}

impl Serialize for Forms {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.len()))?;
        for i in 0..self.len() {
            seq.serialize_element(self.get(i).unwrap_or(""))?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Forms {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Forms, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Forms;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of forms")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Forms, A::Error> {
                // one form at a time: no Vec<String> of them all at once
                let mut f = Forms { text: String::new(), ends: Vec::with_capacity(seq.size_hint().unwrap_or(0)) };
                while let Some(w) = seq.next_element::<std::borrow::Cow<'de, str>>()? {
                    f.text.push_str(&w);
                    f.ends.push(f.text.len() as u32);
                }
                f.text.shrink_to_fit();
                Ok(f)
            }
        }
        d.deserialize_seq(V)
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct SpellerDict {
    /// `fst::Map` data: lowercased form -> `(index << 5) | frequency`,
    /// where frequency is morfologik's 0..=25 rank (higher = more frequent).
    /// For case-sensitive dictionaries (pl: `fsa.dict.encoder=none`, no
    /// conversion) the keys keep their original case instead.
    pub map_bytes: Vec<u8>,
    /// Original-cased forms (all case variants kept: "pero", "Pero").
    pub forms: Forms,
    /// morfologik frequency rank (0..=25) per form, parallel to `forms`.
    pub freqs: Vec<u8>,
    /// LT dictionaries without case conversion (pl_PL) keep proper-noun
    /// casing: "Bledem" is a key but "bledem" must still be misspelled.
    /// Not serialized (bincode has no field versioning): the loader sets it
    /// from the language code via [Rules::with_lang].
    #[serde(skip)]
    pub case_sensitive: bool,
    /// Speller options from the morfologik .info, loaded at runtime from
    /// the <lang>_speller_meta.txt sidecar (no bin reformat):
    /// equivalent-chars substitute at cost 0, replacement-pairs are a
    /// single-edit block substitution.
    #[serde(skip)]
    pub eq_pairs: Vec<(char, char)>,
    #[serde(skip)]
    pub rp_pairs: Vec<(String, String)>,
    /// fsa.dict.speller.convert-case: suggestions are re-cased to the
    /// misspelled word's casing (HATSAP → HAFSA, Hatsap → Hafsa).
    #[serde(skip)]
    pub convert_case: bool,
    /// fsa.dict.speller.ignore-diacritics: candidate comparison runs on
    /// diacritics-stripped forms ('peroo' finds 'Perón', 'peroró').
    #[serde(skip)]
    pub ignore_diacritics: bool,
    /// LT hunspell-dictionary languages (da/sv/eo/km/ar/gl): the exact
    /// `.aff`/`.dic` LT ships, embedded at compile time. When the system
    /// libhunspell is loadable, `is_known` and suggestions delegate to it
    /// (LT's own speller behavior); otherwise the FST approximation runs.
    #[serde(default)]
    pub hunspell: Option<crate::hunspell::HunspellData>,
}

impl SpellerDict {
    pub fn is_empty(&self) -> bool {
        self.forms.is_empty()
    }

    fn map(&self) -> fst::Map<&[u8]> {
        fst::Map::new(&self.map_bytes[..]).expect("speller FST must be valid")
    }

    /// Whether `word` is in the speller vocabulary (case-insensitively).
    pub fn is_known(&self, word: &str) -> bool {
        if word.is_empty() {
            return false;
        }
        if self.case_sensitive {
            // exact-case lookup first; capitalized common words at sentence
            // start still resolve via the lowercase fallback
            if self.map().get(word).is_some() {
                return true;
            }
            let lower = word.to_lowercase();
            lower != word && self.map().get(&lower).is_some()
        } else {
            let lower = word.to_lowercase();
            self.map().get(&lower).is_some()
        }
    }

    /// The original-cased dictionary forms sharing `word`'s lowercase key.
    pub fn variants(&self, word: &str) -> Vec<&str> {
        match self.map().get(word.to_lowercase()) {
            Some(value) => {
                let (first, count) = Self::unpack(value);
                self.forms.range(first, count)
            }
            None => Vec::new(),
        }
    }

    /// The frequency rank (0..=25) of an exact dictionary form, 0 if absent.
    pub fn freq_of(&self, form: &str) -> u8 {
        match self.map().get(form.to_lowercase()) {
            Some(value) => {
                let (first, count) = Self::unpack(value);
                (first..first + count)
                    .find(|&i| self.forms.get(i).map_or(false, |f| f == form))
                    .and_then(|i| self.freqs.get(i).copied())
                    .unwrap_or(0)
            }
            None => 0,
        }
    }

    /// Case-aware lookup like hunspell's: the exact form, a capitalized
    /// or all-caps spelling of a known form ("Haben", "HABEN"), but not a
    /// lowercase spelling of a capitalized-only form ("rechnung"). German
    /// nouns are capitalized, so the case-insensitive [Self::is_known] is
    /// too lenient there.
    pub fn is_known_cased(&self, word: &str) -> bool {
        let variants = self.variants(word);
        if variants.iter().any(|v| *v == word) {
            return true;
        }
        let mut chars = word.chars();
        let first_upper = chars.next().map_or(false, char::is_uppercase);
        let rest: String = chars.collect();
        let all_caps = first_upper && word.chars().count() > 1 && !word.chars().any(char::is_lowercase);
        if all_caps {
            return !variants.is_empty();
        }
        // capitalized word: a form whose first letter is lowercase
        // ("haben" -> "Haben"), with the rest of the word in the same case
        first_upper
            && variants.iter().any(|v| {
                let mut vc = v.chars();
                vc.next().map_or(false, char::is_lowercase) && vc.as_str() == rest
            })
    }

    fn unpack(value: u64) -> (usize, usize) {
        (((value >> 10) as usize), ((value >> 5) & 0x1F) as usize)
    }

    /// Finds words within `max_distance` Damerau-Levenshtein (OSA) edits of
    /// `word`, ordered by (distance, form). Mirrors `MorfologikSpeller.findSimilarWords`.
    ///
    /// Candidates come from the FST's Levenshtein automaton (a superset of the
    /// Damerau neighborhood; each candidate is verified with the true
    /// restricted-Damerau distance). Ordered like morfologik's `Speller`:
    /// by edit distance, then by descending dictionary frequency; ties keep
    /// the (alphabetical) automaton order.
    pub fn find_similar(&self, word: &str, max_distance: usize) -> Vec<String> {
        self.find_similar_opt(word, max_distance, false)
    }

    /// Like [SpellerDict::find_similar] with morfologik's `ignore-diacritics`
    /// option: comparisons run on diacritics-stripped forms.
    pub fn find_similar_ignoring_diacritics(
        &self,
        word: &str,
        max_distance: usize,
    ) -> Vec<String> {
        self.find_similar_opt(word, max_distance, true)
    }

    fn find_similar_opt(
        &self,
        word: &str,
        max_distance: usize,
        ignore_diacritics: bool,
    ) -> Vec<String> {
        use fst::automaton::Levenshtein;
        use fst::{Automaton, IntoStreamer, Streamer};

        let lower = if ignore_diacritics {
            remove_diacritics(&word.to_lowercase())
        } else {
            word.to_lowercase()
        };
        let mut out: Vec<(usize, u8, String)> = Vec::new();
        let map = self.map();

        let extended = !self.eq_pairs.is_empty() || !self.rp_pairs.is_empty();
        let precomputed_pairs: Vec<(Vec<char>, Vec<char>)> = self
            .rp_pairs
            .iter()
            .map(|(p, q)| (p.chars().collect(), q.chars().collect()))
            .collect();
        let eq_pairs = self.eq_pairs.clone();
        // case-sensitive dictionaries (pt-PT-45, pl): candidate matching is
        // case-insensitive over the original-cased keys ('uv' finds 'UV' at
        // d1) while is_known stays exact-case — mirrors LT's behaviour
        let case_sensitive = self.case_sensitive;
        let dist = move |a: &str, b: &str| -> usize {
            if case_sensitive {
                let la = a.to_lowercase();
                let lb = b.to_lowercase();
                if extended {
                    return osa_distance_ext(&la, &lb, &eq_pairs, &precomputed_pairs);
                }
                return osa_distance(&la, &lb);
            }
            let (a2, b2): (std::borrow::Cow<str>, std::borrow::Cow<str>) = if ignore_diacritics {
                (
                    std::borrow::Cow::Owned(remove_diacritics(a)),
                    std::borrow::Cow::Owned(remove_diacritics(b)),
                )
            } else {
                (std::borrow::Cow::Borrowed(a), std::borrow::Cow::Borrowed(b))
            };
            if extended {
                osa_distance_ext(&a2, &b2, &eq_pairs, &precomputed_pairs)
            } else {
                osa_distance(&a2, &b2)
            }
        };

        // the fst automaton is byte-based and matches the char-level
        // neighborhood for ASCII queries; long words at distance 3 can
        // exceed the automaton's state cap -> fall back to the full scan.
        // With equivalent-chars/replacement-pairs the plain byte automaton
        // would miss candidates whose extended distance is lower — full scan.
        // fst's Levenshtein automaton cannot substitute one non-ASCII char
        // for another ('fenètre' never reaches 'fenêtre'), but it is exact
        // for ASCII queries. Accented queries search with their stripped
        // form and the distance widened by the number of non-ASCII chars (a
        // superset of the true neighborhood: d(strip(q), k) <= n + d(q, k));
        // every candidate is then verified with the real distance below.
        // Full scan only when the automaton cannot be built.
        let non_ascii = lower.chars().filter(|c| !c.is_ascii()).count();
        let query = if non_ascii == 0 { lower.clone() } else { remove_diacritics(&lower) };
        // a transposition is one OSA edit but two Levenshtein edits: widen
        // the first tier by one so 'acceuil' reaches 'accueil'. Capped at 2:
        // a distance-3 automaton over a long word takes ~100 ms to build, and
        // candidates that far away are useless suggestions anyway
        let slack = if max_distance == 1 { 1 } else { 0 };
        let automaton = if query.is_ascii() && !ignore_diacritics && !extended && !case_sensitive {
            Levenshtein::new(&query, (max_distance + non_ascii + slack).min(2) as u32).ok()
        } else {
            None
        };

        if let Some(automaton) = automaton {
            debug_assert!(!ignore_diacritics || lower.chars().all(|c| c.is_ascii()));
            let mut stream = map.search(automaton).into_stream();
            while let Some((key, value)) = stream.next() {
                let key = String::from_utf8_lossy(key).into_owned();
                let d = dist(&lower, &key);
                // d == 0 with folding means the form differs only in
                // diacritics from the word ('miercoles' vs 'miércoles') —
                // LT still suggests it (the word itself is excluded by the
                // exact-string check, not by the folded distance)
                let different_word = key != lower;
                if d <= max_distance && (d > 0 || different_word) {
                    let (first, count) = Self::unpack(value);
                    for k in 0..count {
                        if let Some(form) = self.forms.get(first + k) {
                            // exclude only the exact original word
                            if form == word {
                                continue;
                            }
                            let freq = self.freqs.get(first + k).copied().unwrap_or(0);
                            out.push((d, freq, form.to_string()));
                        }
                    }
                }
            }
        } else {
            // non-ASCII queries (or diacritics-insensitive comparisons):
            // full scan with a cheap length pre-filter
            let lower_len = lower.chars().count();
            let mut stream = map.into_stream();
            while let Some((key, value)) = stream.next() {
                let key = String::from_utf8_lossy(key).into_owned();
                if key.chars().count().abs_diff(lower_len) > max_distance {
                    continue;
                }
                let d = dist(&lower, &key);
                // d == 0 with folding means the form differs only in
                // diacritics from the word ('miercoles' vs 'miércoles') —
                // LT still suggests it (the word itself is excluded by the
                // exact-string check, not by the folded distance)
                let different_word = key != lower;
                if d <= max_distance && (d > 0 || different_word) {
                    let (first, count) = Self::unpack(value);
                    for k in 0..count {
                        if let Some(form) = self.forms.get(first + k) {
                            // exclude only the exact original word
                            if form == word {
                                continue;
                            }
                            let freq = self.freqs.get(first + k).copied().unwrap_or(0);
                            out.push((d, freq, form.to_string()));
                        }
                    }
                }
            }
        }

        // stable sort: distance asc, frequency desc, then the original-cased
        // form byte order (LT's stable sort keeps the dictionary iteration
        // order for (d, freq) ties — probed 'hatsap': Hafsa < Hassan < Hasán
        // < WHATSAPP < WhatsApp < hatea)
        // within a distance tier, forms that differ from the word only by
        // diacritics come first ('recu' -> 'reçu' before 'rebu', 'recru')
        let folded = remove_diacritics(&word.to_lowercase());
        let accent_only = |form: &str| remove_diacritics(&form.to_lowercase()) == folded;
        // a lowercase word prefers lowercase suggestions over proper nouns
        // and acronyms ('tems' -> 'temps' before 'TMS', 'Tess')
        let word_lower = word == word.to_lowercase();
        let case_fits = |form: &str| {
            if word_lower {
                form == form.to_lowercase()
            } else {
                !(form.chars().count() > 1 && form == form.to_uppercase())
            }
        };
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| accent_only(&b.2).cmp(&accent_only(&a.2)))
                .then_with(|| case_fits(&b.2).cmp(&case_fits(&a.2)))
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.as_bytes().cmp(b.2.as_bytes()))
        });
        out.dedup_by(|a, b| a.2 == b.2);
        out.into_iter().map(|(_, _, form)| form).collect()
    }
}

/// OSA distance with morfologik speller options: equivalent chars (from
/// .info `equivalent-chars`, e.g. es b↔v) substitute at cost 0, and
/// replacement pairs (.info `replacement-pairs`, e.g. es ke↔que) are a
/// single-edit block substitution in either direction.
pub fn osa_distance_ext(
    a: &str,
    b: &str,
    eq_pairs: &[(char, char)],
    precomputed_pairs: &[(Vec<char>, Vec<char>)],
) -> usize {
    if eq_pairs.is_empty() && precomputed_pairs.is_empty() {
        return osa_distance(a, b);
    }
    let pairs = precomputed_pairs;
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=m {
        dp[0][j] = j;
    }
    let equiv = |x: char, y: char| -> bool {
        if x == y {
            return true;
        }
        eq_pairs.iter().any(|&(p, q)| (p == x && q == y) || (p == y && q == x))
    };
    for i in 1..=n {
        for j in 1..=m {
            let cost = if equiv(a[i - 1], b[j - 1]) { 0 } else { 1 };
            let mut d = (dp[i][j - 1] + 1)
                .min(dp[i - 1][j] + 1)
                .min(dp[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(dp[i - 2][j - 2] + 1);
            }
            // replacement pairs as a block substitution (cost 1)
            for (pc, qc) in pairs.iter() {
                let (pi, qj) = (pc.len(), qc.len());
                if i >= pi && j >= qj && a[i - pi..i] == pc[..] && b[j - qj..j] == qc[..] {
                    d = d.min(dp[i - pi][j - qj] + 1);
                }
                // both directions
                if i >= qj && j >= pi && a[i - qj..i] == qc[..] && b[j - pi..j] == pc[..] {
                    d = d.min(dp[i - qj][j - pi] + 1);
                }
            }
            dp[i][j] = d;
        }
    }
    dp[n][m]
}

/// Restricted Damerau-Levenshtein (optimal string alignment) distance.
pub fn osa_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());

    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }

    let mut prev_prev = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];

    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut d = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev_prev[j - 2] + 1);
            }
            curr[j] = d;
        }
        prev_prev.copy_from_slice(&prev);
        prev.copy_from_slice(&curr);
    }

    prev[m]
}

/// Plain Levenshtein distance (as used by `MultitokenSpeller`).
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());

    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }

    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];

    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[m]
}

/// Mirrors LT's `MultitokenSpeller`: a naive multiword speller that provides
/// suggestions for multitoken expressions from word lists.
#[derive(Serialize, Deserialize, Default)]
pub struct MultitokenSuggester {
    /// first char of the normalized key -> (normalized key -> original lines)
    pub by_char: HashMap<char, HashMap<String, Vec<String>>>,
    /// normalized key without spaces -> original lines
    pub no_spaces: HashMap<String, Vec<String>>,
}

impl MultitokenSuggester {
    pub fn is_empty(&self) -> bool {
        self.no_spaces.is_empty() && self.by_char.is_empty()
    }

    /// Normalization used for keys: lowercase, remove diacritics, `-` -> space.
    pub fn normalize_key(word: &str) -> String {
        remove_diacritics(&word.to_lowercase()).replace('-', " ")
    }
}

/// Removes common Latin diacritics (mirrors `StringTools.removeDiacritics`
/// closely enough for key normalization).
pub fn remove_diacritics(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'ā' => "a".chars().collect::<Vec<_>>(),
            'é' | 'è' | 'ê' | 'ë' | 'ē' => "e".chars().collect(),
            'í' | 'ì' | 'î' | 'ï' | 'ī' => "i".chars().collect(),
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ō' => "o".chars().collect(),
            'ú' | 'ù' | 'û' | 'ü' | 'ū' => "u".chars().collect(),
            'ý' | 'ÿ' => "y".chars().collect(),
            'ñ' => "n".chars().collect(),
            'ç' => "c".chars().collect(),
            'š' => "s".chars().collect(),
            'ž' => "z".chars().collect(),
            'Á' | 'À' | 'Â' | 'Ä' | 'Ã' => "a".chars().collect(),
            'É' | 'È' | 'Ê' | 'Ë' => "e".chars().collect(),
            'Í' | 'Ì' | 'Î' | 'Ï' => "i".chars().collect(),
            'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' => "o".chars().collect(),
            'Ú' | 'Ù' | 'Û' | 'Ü' => "u".chars().collect(),
            'Ý' => "y".chars().collect(),
            'Ñ' => "n".chars().collect(),
            'Ç' => "c".chars().collect(),
            'Š' => "s".chars().collect(),
            'Ž' => "z".chars().collect(),
            _ => c.to_string().chars().collect(),
        })
        .collect()
}

/// Whether the string contains any character with a diacritic.
pub fn has_diacritics(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower != remove_diacritics(&lower)
}

/// All auxiliary data the Java filters need, built per language at compile time.
#[derive(Serialize, Deserialize, Default)]
pub struct FilterData {
    pub speller: Option<SpellerDict>,
    pub multitoken: Option<MultitokenSuggester>,
    /// `wrong-form` (lowercased) -> `[(correct form, postag)]`
    pub confusion_pairs: Option<HashMap<String, Vec<(String, String)>>>,
    /// German `addedCompound.txt`: part1 (lowercase) -> [part2 (lowercase)]
    pub added_compound: Option<HashMap<String, Vec<String>>>,
    /// Whether the MultitokenSpellerFilter should additionally check the
    /// plain speller (LT does this for en/de/pt/nl).
    #[serde(default)]
    pub multitoken_speller_check: bool,
    /// LT `AbstractSimpleReplaceRule` tables (`filters/replace*.txt`, one
    /// per replace file the language runs — e.g. ga: GA_REPLACE,
    /// GA_FGB_EQ_REPLACE, GA_PRESTANDARD_REPLACE; nl: NL_SIMPLE_REPLACE
    /// with sub-rule ids; en-US variant table EN_US_SIMPLE_REPLACE).
    /// Phrase-level `wrong=correct` pairs, matched case-sensitively.
    #[serde(default)]
    pub simple_replace: Option<Vec<SimpleReplaceTable>>,
}

/// Wrong phrases indexed by their first word, with the id prefix the
/// language's SimpleReplaceRule uses (`NL_SIMPLE_REPLACE` for nl).
fn default_true() -> bool { true }

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct SimpleReplaceTable {
    pub prefix: String,
    /// Whether sub-rule ids get the wrong-phrase suffix (nl's
    /// AbstractSimpleReplaceRule2 does, en's older one does not)
    #[serde(default = "default_true")]
    pub sub_ids: bool,
    /// first word of the wrong phrase -> [(wrong phrase, correct phrases)]
    pub by_first_word: HashMap<String, Vec<(String, Vec<String>)>>,
}
