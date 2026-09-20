//! The delivery handle at inference: MIDI for speech, written in front of the
//! English line for the dub model (engine repo, `corpus/score_text.py` and
//! `corpus/source_handle.py`; this file is their port, byte for byte, pinned
//! by the golden fixture `dubhandle_fixture.json` that the Python owner
//! writes).
//!
//! ```text
//! [line 300][w 1b 1b][w 1b][p 2.25b][w 1.5b] Silver Axe! Blast the guy!
//! ```
//!
//! The source line's PHRASES (spoken runs between pauses of at least
//! [`PAUSE_MIN_S`]) keep their durations and their pauses; the English
//! words are dealt to the phrases by clause punctuation when the clause
//! count matches, else by syllable share; inside a phrase every syllable
//! gets the same beats, so a phrase's beats sum to its source duration at
//! the line's tempo; the tempo is what fills the source's spoken time. A
//! word is one note per syllable, a syllable is a vowel group of the word
//! (the corpus rule, not a dictionary). Values are binned to the corpus
//! bins and written exactly (no jitter: that is training's).
//!
//! The app has no word alignment of the source at runtime: it has the line's
//! voiced runs on the VAD grid (`turns::phrases`) and the recognizer's token
//! points (whisper's DTW timestamp per text token, `dubclients::Heard`). A
//! token point lies inside its word (probed 2026-09-18 on the reel's 40 cues
//! against the MMS alignment: the first point 0.02 to 0.34 s after the first
//! word's start, the last point within 0.18 s of the last word's end on 9 of
//! 10 cues, and never inside a leading gasp), so a run holding no token point
//! is a sound the text has no word for (a gasp, a breath, a laugh) and
//! becomes the corpus's `[nv]` rest ([`nonverbal_phrases`]).

/// A gap between phrases counts as a pause from this length (corpus/frames.py).
pub(crate) const PAUSE_MIN_S: f64 = 0.15;
/// A token point may trail its word's end by this much (the probe above: the
/// last point's error, 90th percentile), so a run claims the points up to
/// this far past its end.
const TOKEN_POINT_LAG_S: f64 = 0.18;
/// A run with no token point longer than this is speech the recognizer
/// missed, not a sound: the corpus's `[nv]` slots run to 1.75 s at their
/// 99th percentile (train.jsonl, 19,712 slots, 2026-09-18; median 0.25 s).
pub(crate) const NONVERBAL_MAX_S: f64 = 1.75;
/// The corpus bins (score_text.py): tempo, beats.
const SPM_BIN: f64 = 30.0;
const BEAT_BIN: f64 = 0.25;
const PACE_RANGE_SPM: (f64, f64) = (60.0, 900.0);
const BEATS_RANGE: (f64, f64) = (0.25, 16.0);
/// A clause may take a syllable share this far from its phrase's duration share.
const CLAUSE_SHARE_TOL: f64 = 0.2;
const CLAUSE_END: &[char] = &[',', '.', ';', ':', '!', '?', '、', '。', '，', '！', '？'];

/// One entry of the handle: a word with its notes, or a rest.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Entry {
    Word { notes: Vec<f64> },
    /// A rest; `nonverbal` when it holds a sound the text has no word for
    /// (written `[nv]`: a slot the model fills with a sound of its own).
    Pause { dur: f64, nonverbal: bool },
}

/// The handle for a line: its entries, where the take is placed and how
/// long it should be (seconds on the source line's clock), and the tempo.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Handle {
    pub entries: Vec<Entry>,
    pub onset_s: f64,
    pub span_s: f64,
    /// The tempo asked for: syllables per minute over the source's spoken time.
    pub tempo_spm: u32,
    /// The tempo the score is WRITTEN at (score_text.line_spm): syllables over
    /// the emitted word durations, which differ from the spoken time when
    /// phrases merged (the swallowed gap joins a word's time).
    line_spm: u32,
}

impl Handle {
    /// The handle as text, the line after it.
    pub fn spoken(&self, text: &str) -> String {
        let prefix = self.prefix();
        if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix} {text}")
        }
    }

    /// The bracket score alone (score_text.score_prefix with every level present).
    pub fn prefix(&self) -> String {
        let tempo = self.line_spm as f64;
        if tempo == 0.0 {
            return String::new();
        }
        let mut out = format!("[line {}]", value(tempo, SPM_BIN, PACE_RANGE_SPM) as i64);
        for entry in &self.entries {
            match entry {
                Entry::Pause { dur, nonverbal } => out.push_str(&format!("[{} {}]", if *nonverbal { "nv" } else { "p" }, beats(dur * tempo / 60.0))),
                Entry::Word { notes } => {
                    let notes: Vec<String> = notes.iter().map(|d| beats(d * tempo / 60.0)).collect();
                    out.push_str(&format!("[w {}]", notes.join(" ")));
                }
            }
        }
        out
    }
}

/// Python's `round(x, 3)` and `round(x / step)`: half to even.
fn round3(x: f64) -> f64 {
    (x * 1000.0).round_ties_even() / 1000.0
}

fn value(x: f64, step: f64, range: (f64, f64)) -> f64 {
    ((x / step).round_ties_even() * step).clamp(range.0, range.1)
}

/// `f"{x:g}b"` for a binned beat count: `1b`, `1.5b`, `0.75b`, `16b`.
fn beats(x: f64) -> String {
    let x = value(x, BEAT_BIN, BEATS_RANGE);
    if x.fract() == 0.0 {
        format!("{}b", x as i64)
    } else {
        format!("{x}b")
    }
}

/// The word units of a line (whitespace-separated, punctuation kept).
pub(crate) fn words_of(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Syllables of a Latin-script word by the corpus rule: the vowel groups
/// (`[aeiouy]+`, accents folded), at least one.
pub(crate) fn syllables(word: &str) -> usize {
    let mut groups = 0;
    let mut in_group = false;
    for ch in word.chars() {
        if is_vowel(ch) {
            if !in_group {
                groups += 1;
                in_group = true;
            }
        } else {
            in_group = false;
        }
    }
    groups.max(1)
}

fn is_vowel(ch: char) -> bool {
    matches!(
        ch.to_ascii_lowercase(),
        'a' | 'e' | 'i' | 'o' | 'u' | 'y'
            | 'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' | 'è' | 'é' | 'ê' | 'ë' | 'ì' | 'í' | 'î' | 'ï'
            | 'ò' | 'ó' | 'ô' | 'ö' | 'õ' | 'ù' | 'ú' | 'û' | 'ü' | 'ý' | 'ÿ'
            | 'À' | 'Á' | 'Â' | 'Ä' | 'Ã' | 'Å' | 'È' | 'É' | 'Ê' | 'Ë' | 'Ì' | 'Í' | 'Î' | 'Ï'
            | 'Ò' | 'Ó' | 'Ô' | 'Ö' | 'Õ' | 'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ý'
    )
}

/// Greedy deal of the words into `shares.len()` groups whose syllable shares
/// follow `shares` (the phrases' duration shares).
fn by_share(counts: &[usize], shares: &[f64]) -> Vec<Vec<usize>> {
    let total = counts.iter().sum::<usize>() as f64;
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let (mut start, mut acc, mut k) = (0usize, 0usize, 0usize);
    let mut target = shares[0] * total;
    for (i, &c) in counts.iter().enumerate() {
        acc += c;
        if k < shares.len() - 1 && acc as f64 >= target {
            groups.push((start..=i).collect());
            start = i + 1;
            k += 1;
            target += shares[k] * total;
        }
    }
    groups.push((start..counts.len()).collect());
    groups.retain(|g| !g.is_empty());
    groups
}

/// The words dealt into `shares.len()` groups: by clause punctuation when the
/// clause count matches and each clause's syllable share is within
/// [`CLAUSE_SHARE_TOL`] of its phrase's duration share, else by share.
fn deal_words(words: &[&str], shares: &[f64]) -> (Vec<Vec<usize>>, Vec<usize>) {
    let counts: Vec<usize> = words.iter().map(|w| syllables(w)).collect();
    if shares.len() == 1 {
        return (vec![(0..words.len()).collect()], counts);
    }
    let mut clause_ends: Vec<usize> = words.iter().enumerate().filter(|(_, w)| w.ends_with(CLAUSE_END)).map(|(i, _)| i).collect();
    if let Some(&last) = clause_ends.last() {
        if last != words.len() - 1 {
            clause_ends.push(words.len() - 1);
        }
    }
    if clause_ends.len() == shares.len() {
        let mut groups = Vec::new();
        let mut start = 0;
        for &b in &clause_ends {
            groups.push((start..=b).collect::<Vec<usize>>());
            start = b + 1;
        }
        let total = counts.iter().sum::<usize>() as f64;
        let fits = groups.iter().zip(shares).all(|(g, s)| (g.iter().map(|&i| counts[i]).sum::<usize>() as f64 / total - s).abs() <= CLAUSE_SHARE_TOL);
        if fits {
            return (groups, counts);
        }
    }
    (by_share(&counts, shares), counts)
}

/// Which of the line's voiced `runs` (start, end seconds) are non-verbal:
/// those holding none of the recognizer's `token_points_s` (up to
/// [`TOKEN_POINT_LAG_S`] past their end) and no longer than
/// [`NONVERBAL_MAX_S`]. Without token points (a recognizer that gives none)
/// every run is speech.
pub(crate) fn nonverbal_phrases(runs: &[(f64, f64)], token_points_s: &[f64]) -> Vec<bool> {
    let flags: Vec<bool> = runs
        .iter()
        .map(|&(s, e)| e - s <= NONVERBAL_MAX_S && !token_points_s.iter().any(|&t| t >= s && t <= e + TOKEN_POINT_LAG_S))
        .collect();
    // Points that land in no run at all (or no points) say nothing about
    // the runs: the words were heard somewhere, so the instrument missed,
    // and no run is called a sound on its word.
    if flags.iter().all(|&nv| nv) {
        return vec![false; runs.len()];
    }
    flags
}

/// The handle of `text` over the source line's voiced `runs` (start, end
/// seconds on the line's clock, gaps of at least [`PAUSE_MIN_S`] between
/// them), `nonverbal` flagging the runs that are a sound without a word: a
/// leading sound is a rest the take starts with (placed at the sound), a
/// sound between spoken runs or after the last one lives in the rest after
/// the run. `tempo_scale` above 1 asks for a faster line (every syllable
/// and pause gets 1/scale of its source time). `None` when there is
/// nothing to deal.
pub(crate) fn source_handle(runs: &[(f64, f64)], nonverbal: &[bool], text: &str, tempo_scale: f64) -> Option<Handle> {
    assert_eq!(runs.len(), nonverbal.len(), "one non-verbal flag per run");
    let target_words = words_of(text);
    let spoken: Vec<usize> = (0..runs.len()).filter(|&i| !nonverbal[i]).collect();
    if spoken.is_empty() || target_words.is_empty() {
        return None;
    }
    // phrases_of: (start, end, pause_after, nv_after) over the spoken runs
    let mut phrases: Vec<(f64, f64, f64, bool)> = spoken
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let (s, e) = runs[i];
            let (pause, nv) = match spoken.get(k + 1) {
                Some(&j) => (round3(runs[j].0 - e), j > i + 1),
                None if i + 1 < runs.len() => (round3(runs[runs.len() - 1].1 - e), true),
                None => (0.0, false),
            };
            // the owner's arithmetic, bit for bit: a run is a word at
            // round(start) lasting round(end - start), its end the sum (a
            // share can sit exactly on a deal boundary)
            let start = round3(s);
            (start, start + round3(e - s), if pause >= PAUSE_MIN_S || nv { pause } else { 0.0 }, nv)
        })
        .collect();
    let spoken_s: f64 = phrases.iter().map(|&(s, e, _, _)| e - s).sum();
    let shares: Vec<f64> = phrases.iter().map(|&(s, e, _, _)| (e - s) / spoken_s).collect();
    let (groups, counts) = deal_words(&target_words, &shares);
    if groups.len() < phrases.len() {
        // fewer target words than phrases: the last phrases merge into the final group's time
        let last = phrases[groups.len() - 1];
        let tail = phrases[phrases.len() - 1];
        phrases.truncate(groups.len() - 1);
        phrases.push((last.0, tail.1, tail.2, tail.3));
    }
    let total_syl: usize = counts.iter().sum();
    let mut onset = phrases[0].0;
    let mut entries = Vec::new();
    let mut t = 0.0f64;
    let mut span = 0.0f64;
    let mut word_seconds = 0.0f64;
    if spoken[0] > 0 {
        // the source starts with a sound: the take does too, placed at the sound
        let lead = round3(runs[0].0);
        onset = lead;
        let dur = (phrases[0].0 - lead) / tempo_scale;
        span = span.max(round3(dur));
        entries.push(Entry::Pause { dur: round3(dur), nonverbal: true });
        t += dur;
    }
    for (&(start, end, pause, nv), group) in phrases.iter().zip(&groups) {
        let phrase_syl: usize = group.iter().map(|&i| counts[i]).sum();
        let per_syl_s = (end - start) / phrase_syl as f64 / tempo_scale;
        let pause = pause / tempo_scale;
        for &i in group {
            let notes = vec![round3(per_syl_s); counts[i]];
            let dur = round3(counts[i] as f64 * per_syl_s);
            word_seconds += dur;
            span = span.max(round3(t) + dur);
            entries.push(Entry::Word { notes });
            t += counts[i] as f64 * per_syl_s;
        }
        if pause >= PAUSE_MIN_S || nv {
            let dur = round3(pause);
            span = span.max(round3(t) + dur);
            entries.push(Entry::Pause { dur, nonverbal: nv });
            t += pause;
        }
    }
    let tempo_spm = (60.0 * total_syl as f64 / spoken_s * tempo_scale).round_ties_even() as u32;
    let line_spm = if word_seconds > 0.0 { (60.0 * total_syl as f64 / word_seconds).round_ties_even() as u32 } else { 0 };
    Some(Handle { entries, onset_s: onset, span_s: span * tempo_scale, tempo_spm, line_spm })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syllables_follow_the_corpus_rule() {
        for (word, n) in [("Silver", 2), ("Axe!", 2), ("guy", 1), ("Uaah!", 1), ("letting", 2), ("I", 1), ("42", 1), ("won't", 1), ("beautiful", 3)] {
            assert_eq!(syllables(word), n, "{word}");
        }
    }

    #[test]
    fn beats_are_written_like_python() {
        assert_eq!(beats(1.0), "1b");
        assert_eq!(beats(1.5), "1.5b");
        assert_eq!(beats(0.74), "0.75b");
        assert_eq!(beats(0.1), "0.25b");
        assert_eq!(beats(40.0), "16b");
        assert_eq!(beats(2.125), "2b"); // half to even, as Python's round
    }

    /// The golden fixture: the Python owner's handles for phrase sets and
    /// lines, byte for byte, with the placement and the tempo.
    #[test]
    fn handles_match_the_python_owner() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("dubhandle_fixture.json")).unwrap();
        assert!(cases.len() >= 50);
        for case in &cases {
            let phrases: Vec<(f64, f64)> = case["phrases"].as_array().unwrap().iter().map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap())).collect();
            let sounds: Vec<usize> = case["nonverbal"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
            let nonverbal: Vec<bool> = (0..phrases.len()).map(|i| sounds.contains(&i)).collect();
            let text = case["text"].as_str().unwrap();
            let scale = case["tempo_scale"].as_f64().unwrap();
            let expected_syllables: Vec<usize> = case["syllables"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
            assert_eq!(words_of(text).iter().map(|w| syllables(w)).collect::<Vec<_>>(), expected_syllables, "{text}");
            let handle = source_handle(&phrases, &nonverbal, text, scale).expect("a handle");
            assert_eq!(handle.spoken(text), case["handle"].as_str().unwrap(), "{phrases:?} nv {sounds:?} x{scale} {text}");
            assert!((handle.onset_s - case["onset_s"].as_f64().unwrap()).abs() < 1e-6);
            assert!((handle.span_s - case["span_s"].as_f64().unwrap()).abs() < 2e-3, "span {} vs {}", handle.span_s, case["span_s"]);
            assert_eq!(handle.tempo_spm as u64, case["tempo_spm"].as_u64().unwrap());
        }
        assert!(cases.iter().filter(|c| !c["nonverbal"].as_array().unwrap().is_empty()).count() >= 20, "the fixture covers non-verbal runs");
    }

    /// A run with no token point is a sound; the lag past the run's end and
    /// the length bound keep a late point and a missed line from turning
    /// speech into a sound.
    #[test]
    fn a_run_without_a_token_point_is_non_verbal() {
        let runs = [(0.1, 0.4), (0.7, 1.9), (2.1, 2.5), (2.8, 3.1)];
        // points: inside the second run, one trailing its end within the lag, none in the first or last
        assert_eq!(nonverbal_phrases(&runs, &[0.9, 1.5, 2.0]), vec![true, false, true, true]);
        // the third run's point 0.1 s past its end still counts for it
        assert_eq!(nonverbal_phrases(&runs, &[0.9, 2.6]), vec![true, false, false, true]);
        // no points at all, or points in no run: everything is speech
        assert_eq!(nonverbal_phrases(&runs, &[]), vec![false; 4]);
        assert_eq!(nonverbal_phrases(&runs, &[0.62, 3.5]), vec![false; 4]);
        // a long run without points is a missed line, not a sound
        assert_eq!(nonverbal_phrases(&[(0.0, 2.0), (2.5, 3.0)], &[3.4]), vec![false, true]);
    }

    #[test]
    fn all_sounds_and_no_words_give_no_handle() {
        assert!(source_handle(&[(0.1, 0.4)], &[true], "Hey", 1.0).is_none());
        assert!(source_handle(&[(0.1, 0.4)], &[false], "", 1.0).is_none());
    }
}
