//! Fuzzy matching for library search.
//!
//! Each whitespace-separated query term must match at least one field,
//! either as a subsequence (fzf-style: shortest window, bonuses for word
//! starts and consecutive runs, penalties for gaps) or, for terms of four or
//! more characters, by trigram similarity to some word (typo tolerance:
//! "fishye" still finds "fisheye"). The item score is the mean of the
//! per-term best `field score × field weight`.

/// One searchable field and its weight (titles typically weigh more).
pub type Field<'a> = (&'a str, f32);

const BONUS_BOUNDARY: f32 = 2.0;
const BONUS_CONSECUTIVE: f32 = 1.5;
const BONUS_WHOLE_WORD: f32 = 1.0;
const GAP_PENALTY: f32 = 0.15;
const TRIGRAM_THRESHOLD: f32 = 0.3;

struct Prepared {
    lower: Vec<char>,
    boundary: Vec<bool>,
}

fn prepare(s: &str) -> Prepared {
    let orig: Vec<char> = s.chars().collect();
    let mut lower = Vec::with_capacity(orig.len());
    let mut boundary = Vec::with_capacity(orig.len());
    for (i, &c) in orig.iter().enumerate() {
        let b = if i == 0 {
            true
        } else {
            let p = orig[i - 1];
            (!p.is_alphanumeric() && c.is_alphanumeric())
                || (p.is_lowercase() && c.is_uppercase())
                || (p.is_alphabetic() && c.is_ascii_digit())
                || (p.is_ascii_digit() && c.is_alphabetic())
        };
        lower.extend(c.to_lowercase());
        // to_lowercase may expand; keep vectors aligned.
        while boundary.len() < lower.len() {
            boundary.push(b && boundary.len() + 1 == lower.len());
        }
    }
    Prepared { lower, boundary }
}

/// Subsequence score of `term` (already lowercase) in `hay`, or `None`.
fn subsequence(term: &[char], hay: &Prepared) -> Option<f32> {
    if term.is_empty() {
        return Some(0.0);
    }
    let h = &hay.lower;
    // Forward pass: earliest end of a full match.
    let mut ti = 0;
    let mut end = None;
    for (i, &c) in h.iter().enumerate() {
        if c == term[ti] {
            ti += 1;
            if ti == term.len() {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    // Backward pass from `end`: latest start, giving the shortest window.
    let mut ti = term.len();
    let mut start = end;
    for i in (0..=end).rev() {
        if h[i] == term[ti - 1] {
            ti -= 1;
            if ti == 0 {
                start = i;
                break;
            }
        }
    }
    // Prefer an exact substring occurrence that starts on a word boundary.
    let mut best = score_window(term, hay, start);
    let n = term.len();
    if h.len() >= n {
        for i in 0..=h.len() - n {
            if hay.boundary[i] && h[i..i + n] == *term {
                best = best.max(score_window(term, hay, i));
                break;
            }
        }
    }
    Some(best)
}

fn score_window(term: &[char], hay: &Prepared, start: usize) -> f32 {
    let h = &hay.lower;
    let mut score = 0.0;
    let mut ti = 0;
    let mut prev: Option<usize> = None;
    let mut first = None;
    let mut last = start;
    for (i, &c) in h.iter().enumerate().skip(start) {
        if ti == term.len() {
            break;
        }
        if c != term[ti] {
            continue;
        }
        let mut s = 1.0;
        if hay.boundary[i] {
            s += BONUS_BOUNDARY * if ti == 0 { 1.5 } else { 1.0 };
        }
        if prev == Some(i.wrapping_sub(1)) {
            s += BONUS_CONSECUTIVE;
        } else if let Some(p) = prev {
            score -= GAP_PENALTY * (i - p - 1).min(10) as f32;
        }
        score += s;
        prev = Some(i);
        first.get_or_insert(i);
        last = i;
        ti += 1;
    }
    let first = first.unwrap_or(start);
    // Whole-word match: the matched run is a contiguous word.
    let contiguous = last + 1 - first == term.len();
    let ends_word = h.get(last + 1).is_none_or(|c| !c.is_alphanumeric());
    if contiguous && hay.boundary[first] && ends_word {
        score += BONUS_WHOLE_WORD * term.len() as f32;
    }
    score / term.len() as f32
}

fn trigrams(s: &str) -> Vec<[char; 3]> {
    let padded: Vec<char> = std::iter::once(' ')
        .chain(s.chars().flat_map(char::to_lowercase))
        .chain(std::iter::once(' '))
        .collect();
    let mut v: Vec<[char; 3]> = padded.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Jaccard similarity of the two strings' trigram sets (0..=1).
pub fn trigram_similarity(a: &str, b: &str) -> f32 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < ta.len() && j < tb.len() {
        match ta[i].cmp(&tb[j]) {
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    common as f32 / (ta.len() + tb.len() - common) as f32
}

/// Score `query` against weighted fields. `None` if any term fails to
/// match every field. An empty query scores 0.
pub fn score(query: &str, fields: &[Field<'_>]) -> Option<f32> {
    let terms: Vec<Vec<char>> = query
        .split_whitespace()
        .map(|t| t.chars().flat_map(char::to_lowercase).collect())
        .collect();
    if terms.is_empty() {
        return Some(0.0);
    }
    let prepared: Vec<(Prepared, f32, &str)> =
        fields.iter().map(|(t, w)| (prepare(t), *w, *t)).collect();
    let mut total = 0.0;
    for term in &terms {
        let mut best: Option<f32> = None;
        for (p, w, _) in &prepared {
            if let Some(s) = subsequence(term, p) {
                best = Some(best.map_or(s * w, |b: f32| b.max(s * w)));
            }
        }
        if best.is_none() && term.len() >= 4 {
            let t: String = term.iter().collect();
            for (_, w, raw) in &prepared {
                for word in raw
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.len() >= 3)
                {
                    let sim = trigram_similarity(&t, word);
                    if sim >= TRIGRAM_THRESHOLD {
                        best = Some(best.map_or(sim * w, |b: f32| b.max(sim * w)));
                    }
                }
            }
        }
        total += best?;
    }
    Some(total / terms.len() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(q: &str, h: &str) -> Option<f32> {
        score(q, &[(h, 1.0)])
    }

    #[test]
    fn subsequence_matching() {
        assert!(s("btr", "Beach Trip 2026").is_some());
        assert!(s("xyz", "Beach Trip").is_none());
        assert_eq!(s("", "anything"), Some(0.0));
        // All terms must match.
        assert!(s("beach trip", "Beach_Trip_180_LR.mp4").is_some());
        assert!(s("beach mountain", "Beach_Trip_180_LR.mp4").is_none());
    }

    #[test]
    fn ranking() {
        // Word-start and contiguous matches beat scattered ones.
        let whole = s("trip", "Beach Trip").unwrap();
        let scattered = s("trip", "the rain in pisa").unwrap();
        assert!(whole > scattered, "{whole} vs {scattered}");
        let prefix = s("bea", "Beach").unwrap();
        let middle = s("bea", "Abeam").unwrap();
        assert!(prefix > middle);
        // camelCase boundaries count.
        assert!(s("vt", "VrTour").unwrap() > s("vt", "lavatory").unwrap());
        // Exact boundary substring found even when an earlier scattered match exists.
        assert!(s("180", "a1b8c0 scene_180").unwrap() > s("180", "a1b8c0").unwrap());
    }

    #[test]
    fn weights_and_typos() {
        let title = score("beach", &[("Beach day", 1.5), ("/nas/x", 1.0)]).unwrap();
        let path = score("beach", &[("Holiday", 1.5), ("/nas/beach/x", 1.0)]).unwrap();
        assert!(title > path);
        // Typo tolerance via trigrams.
        assert!(s("fishye", "Scene FISHEYE190").is_some());
        assert!(s("mountian", "Mountain hike").is_some());
        assert!(s("qqqq", "Mountain hike").is_none());
        assert!(trigram_similarity("fisheye", "fisheye") > 0.99);
        assert_eq!(trigram_similarity("", "abc"), 0.0);
    }

    #[test]
    fn unicode_safe() {
        assert!(s("über", "Ein Über Video").is_some());
        assert!(s("İ", "İstanbul").is_some());
    }
}
