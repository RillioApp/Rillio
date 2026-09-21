//! The dub's lines from a LOADED SUBTITLE TRACK instead of the recognizer and
//! the translator (the viewer's choice, "Translation source: Loaded
//! subtitles"). Measured on the fixture episode (2026-09-21) against its own
//! English subtitle track: whisper small heard 2 of 8 checkable lines right,
//! large-v3-turbo 6, Qwen3-ASR 4, and what stays wrong (homophones, names)
//! needs the story, which the subtitles already hold, with times.
//!
//! The web hands over the external track's cues (it parses them to draw them,
//! `withHTMLSubtitles`); embedded tracks expose no cue list through mpv, so
//! they are out of scope here, as they are for `autosync`.
//!
//! Rules:
//! - a cue is cleaned of markup, speaker dashes and captions for the deaf
//!   ("[laughs]", "(sighs)", music); a cue with nothing left to SAY, or one
//!   that is a sign (all capitals, several words), is dropped;
//! - every cue goes to the ONE speaker turn it overlaps most in time, so a
//!   cue over silence (a sign, a title card) reaches no turn;
//! - a turn's line is its cues in order; a turn without a cue keeps the
//!   original voice, as a turn with nothing to say does on the AI path.

use serde::Deserialize;

use crate::dubhandle::{syllables, words_of};

/// One cue as the web hands it over (milliseconds of the stream, the track's
/// delay already applied).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScriptCue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// A sign reads as several words in capitals ("THE VALIANT PIRATE'S SWORD");
/// a shouted single word ("STOP!") is speech.
const SIGN_MIN_WORDS: usize = 2;

/// The loaded track, cleaned: speech cues only, in time order.
#[derive(Debug)]
pub struct Script {
    cues: Vec<ScriptCue>,
    id: String,
}

impl Script {
    pub fn new(cues: Vec<ScriptCue>) -> Self {
        let mut cues: Vec<ScriptCue> = cues
            .into_iter()
            .filter(|cue| cue.end_ms > cue.start_ms)
            .filter_map(|cue| clean(&cue.text).map(|text| ScriptCue { text, ..cue }))
            .collect();
        cues.sort_by_key(|cue| cue.start_ms);
        // What the audio depends on, for the dub's cache key: another track,
        // or the same one re-timed, is another dub.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for cue in &cues {
            for byte in cue.start_ms.to_le_bytes().iter().chain(cue.end_ms.to_le_bytes().iter()).chain(cue.text.as_bytes()) {
                hash = (hash ^ *byte as u64).wrapping_mul(0x0100_0000_01b3);
            }
        }
        Self { id: format!("subtitles:{}:{hash:016x}", cues.len()), cues }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// The line of every turn (`(start_ms, end_ms)` of the stream, in order):
    /// each cue goes to the turn it overlaps most, a turn's cues are joined.
    pub fn lines(&self, turns: &[(i64, i64)]) -> Vec<Option<String>> {
        let mut lines: Vec<Option<String>> = vec![None; turns.len()];
        let (Some(first), Some(last)) = (turns.first(), turns.last()) else { return lines };
        for cue in self.cues.iter().filter(|cue| cue.end_ms > first.0 && cue.start_ms < last.1) {
            let overlap = |turn: &(i64, i64)| cue.end_ms.min(turn.1) - cue.start_ms.max(turn.0);
            let Some((index, _)) = turns.iter().enumerate().map(|(i, turn)| (i, overlap(turn))).filter(|&(_, ms)| ms > 0).max_by_key(|&(i, ms)| (ms, std::cmp::Reverse(i))) else {
                continue;
            };
            match &mut lines[index] {
                Some(line) => {
                    line.push(' ');
                    line.push_str(&cue.text);
                }
                none => *none = Some(cue.text.clone()),
            }
        }
        lines
    }
}

/// English syllables of a line, by the handle's own rule.
pub(crate) fn syllable_count(text: &str) -> usize {
    words_of(text).iter().map(|word| syllables(word)).sum()
}

/// A cue's text as a line to SPEAK, or `None` when nothing is left to say.
pub(crate) fn clean(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    // markup and captions: <i>, {\an8}, [laughs], (sighs)
    let mut closers: Vec<char> = Vec::new();
    for ch in text.chars() {
        match (closers.last(), ch) {
            (Some(&closer), c) if c == closer => {
                closers.pop();
            }
            (_, '<') => closers.push('>'),
            (_, '{') => closers.push('}'),
            (_, '[') => closers.push(']'),
            (_, '(') => closers.push(')'),
            (None, '\u{266a}' | '\u{266b}' | '#') => {}
            (None, c) => out.push(if c == '\n' || c == '\r' { ' ' } else { c }),
            _ => {}
        }
    }
    let line = out
        .split_whitespace()
        // a speaker dash opens a line in a two-speaker cue ("- Hi. - Hello.")
        .filter(|word| *word != "-" && *word != "\u{2013}" && *word != "\u{2014}")
        .map(|word| word.trim_start_matches('-'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let letters: Vec<char> = line.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return None;
    }
    let is_sign = letters.iter().all(|c| !c.is_lowercase()) && line.split_whitespace().count() >= SIGN_MIN_WORDS;
    (!is_sign).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: i64, end_ms: i64, text: &str) -> ScriptCue {
        ScriptCue { start_ms, end_ms, text: text.into() }
    }

    #[test]
    fn a_cue_is_cleaned_to_what_is_spoken() {
        assert_eq!(clean("<i>Show your true form,</i>\nrelic.").as_deref(), Some("Show your true form, relic."));
        assert_eq!(clean("{\\an8}This is humiliating!").as_deref(), Some("This is humiliating!"));
        assert_eq!(clean("- Jump in, Boss!\n- Later!").as_deref(), Some("Jump in, Boss! Later!"));
        assert_eq!(clean("[laughs] Fine. (sighs)").as_deref(), Some("Fine."));
        assert_eq!(clean("STOP!").as_deref(), Some("STOP!"), "one shouted word is speech");
        assert_eq!(clean("EPISODE 9: THE RULER OF RELICS"), None, "a sign is not spoken");
        assert_eq!(clean("\u{266a} \u{266a}"), None);
        assert_eq!(clean("[door slams]"), None);
        assert_eq!(clean("..."), None);
    }

    #[test]
    fn every_cue_goes_to_the_turn_it_overlaps_most() {
        // the fixture episode's shape: a sign and a line over the same turn,
        // a cue over silence, a turn without a cue
        let script = Script::new(vec![
            cue(126_400, 129_400, "THE VALIANT PIRATE'S SWORD"),
            cue(126_700, 128_600, "A pirate's sword, huh?"),
            cue(95_600, 99_600, "EPISODE 9: THE RULER OF RELICS"),
            cue(130_900, 133_200, "You think something happened?"),
            cue(200_000, 201_000, "Nobody is speaking here."),
        ]);
        let turns = [(126_900, 128_400), (129_700, 130_400), (130_400, 134_400), (135_900, 137_900)];
        assert_eq!(
            script.lines(&turns),
            vec![Some("A pirate's sword, huh?".to_owned()), None, Some("You think something happened?".to_owned()), None]
        );
    }

    #[test]
    fn two_cues_of_one_turn_are_joined_in_order_and_a_split_cue_goes_to_one_turn() {
        let script = Script::new(vec![cue(3_000, 4_000, "Second."), cue(1_000, 2_000, "First."), cue(4_500, 7_000, "Mostly the second turn.")]);
        let turns = [(900, 4_800), (4_800, 7_200)];
        assert_eq!(script.lines(&turns), vec![Some("First. Second.".to_owned()), Some("Mostly the second turn.".to_owned())]);
        assert!(script.lines(&[]).is_empty());
    }

    #[test]
    fn the_id_follows_the_text_and_the_timing() {
        let a = Script::new(vec![cue(0, 1_000, "Hello.")]);
        assert_eq!(a.id(), Script::new(vec![cue(0, 1_000, "<b>Hello.</b>")]).id(), "markup is not part of the dub");
        assert_ne!(a.id(), Script::new(vec![cue(100, 1_100, "Hello.")]).id(), "a re-timed track is another dub");
        assert_ne!(a.id(), Script::new(vec![cue(0, 1_000, "Goodbye.")]).id());
        assert_eq!(syllable_count("Show your true form, relic."), 6);
    }
}
