//! Phrase-anchored placement of a dub take: the port of the engine's
//! `corpus/placement.py`, held to it by the golden fixture
//! `dubplace_fixture.json` (`corpus/placement_fixture.py` writes it).
//!
//! Measured on the engine's reel (2026-09-20): the model speaks at the asked
//! tempo but collapses the handle's pause and `[nv]` slots, so a take ends
//! before 80 % of the actor's span on a third of the lines and its later
//! phrases land early. The model's timing inside a phrase is good; between
//! phrases the plan (the handle) is the authority:
//!
//! - the take is cut between the plan's phrases at the quietest frame of the
//!   second half of the stretch between the last word start of one phrase
//!   and the first word start of the next (word starts are all the
//!   recognizer gives; the quiet frame is what is left of the pause), but
//!   only where that frame is a real pause of the take: where the take speaks
//!   through the plan's rest the two phrases stay one chunk, since a cut in
//!   running speech is heard as an abruption;
//! - every phrase's first word is placed on its planned time, the audio
//!   before it kept before it; a phrase never starts before the previous one
//!   ended, never before the onset.
//!
//! `None` when the heard words do not match the plan's, or there is a single
//! phrase at the onset: the caller keeps the block placement.

use crate::dubhandle::Entry;

const FADE_MS: usize = 10;
/// The energy frame the cut is searched on.
const FRAME_MS: usize = 20;
/// A cut is made only in a real pause of the TAKE: the quietest frame's mean
/// energy under this share of the take's own (-30 dB). Without it a cut
/// landed in running speech wherever the model spoke through the plan's rest
/// (first app test, 2026-09-21: four cuts measured at -5 to -14 dB, heard as
/// abruptions). Relative, so it holds at any take level. The literal is the
/// owner's (`placement.py`), so both sides compare the same f64.
const PAUSE_RATIO: f64 = 0.001;

/// One entry of the plan: a word or a rest, and when it starts (seconds from
/// the take's onset).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PlanItem {
    pub word: bool,
    pub t: f64,
}

/// One phrase of the take, in samples: the cut `[from, to)` and where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placed {
    pub from: usize,
    pub to: usize,
    pub start: usize,
}

/// The handle's entries as plan items: every entry starts where the ones
/// before it end (the handle is written at the source's tempo).
pub(crate) fn plan_items(entries: &[Entry]) -> Vec<PlanItem> {
    let mut t = 0.0;
    entries
        .iter()
        .map(|entry| {
            let (word, dur) = match entry {
                Entry::Word { notes } => (true, notes.iter().sum::<f64>()),
                Entry::Pause { dur, .. } => (false, *dur),
            };
            let item = PlanItem { word, t };
            t += dur;
            item
        })
        .collect()
}

/// Runs of word indices between the plan's rests.
fn phrase_groups(plan: &[PlanItem]) -> Vec<Vec<usize>> {
    let (mut groups, mut current, mut index) = (Vec::new(), Vec::new(), 0usize);
    for item in plan {
        if item.word {
            current.push(index);
            index += 1;
        } else if !current.is_empty() {
            groups.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// The quietest frame of a stretch: the sample index of its centre, its
/// energy (`None` when the stretch holds no whole frame) and its length.
struct QuietFrame {
    centre: usize,
    energy: Option<f64>,
    len: usize,
}

/// The frame is a real pause: its mean energy is under [`PAUSE_RATIO`] of the
/// take's own (the owner's `is_pause`, same operation order).
fn is_pause(frame_energy: f64, frame_len: usize, take_energy: f64, take_len: usize) -> bool {
    frame_energy * take_len as f64 <= (take_energy * frame_len as f64) * PAUSE_RATIO
}

/// Sum of squares of the whole take, as one running f64 sum.
fn take_energy(take: &[f32]) -> f64 {
    take.iter().fold(0.0f64, |sum, &x| sum + x as f64 * x as f64)
}

/// The quietest frame in `[a_s, b_s)`. The energies come from one running
/// f64 sum, the order the Python owner adds in.
fn quietest_frame(take: &[f32], rate: u32, a_s: f64, b_s: f64) -> QuietFrame {
    let n = (FRAME_MS * rate as usize / 1000).max(1);
    let a = (a_s * rate as f64) as usize;
    let b = ((b_s * rate as f64) as usize).max(a + n);
    let window = &take[a.min(take.len())..b.min(take.len())];
    let mut running = Vec::with_capacity(window.len() + 1);
    running.push(0.0f64);
    for &x in window {
        running.push(running[running.len() - 1] + x as f64 * x as f64);
    }
    let (mut best, mut best_energy) = (a, None::<f64>);
    for k in 0..window.len() / n {
        let energy = running[(k + 1) * n] - running[k * n];
        if best_energy.map_or(true, |e| energy < e) {
            best = a + k * n;
            best_energy = Some(energy);
        }
    }
    QuietFrame { centre: (best + n / 2).min(take.len()), energy: best_energy, len: n }
}

/// Where each phrase of the take goes (see the module doc).
pub(crate) fn layout(take: &[f32], rate: u32, word_starts: &[f64], plan: &[PlanItem]) -> Option<Vec<Placed>> {
    let planned: Vec<f64> = plan.iter().filter(|item| item.word).map(|item| item.t).collect();
    if word_starts.is_empty() || word_starts.len() != planned.len() {
        return None;
    }
    let total = take_energy(take);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cuts = vec![0usize];
    for group in phrase_groups(plan) {
        if let Some(previous) = groups.last_mut() {
            let a = word_starts[*previous.last()?];
            let b = word_starts[group[0]].max(a);
            let quiet = quietest_frame(take, rate, (a + b) / 2.0, b);
            if !quiet.energy.is_some_and(|energy| is_pause(energy, quiet.len, total, take.len())) {
                // the take speaks through the plan's rest: one chunk, no cut
                previous.extend(group);
                continue;
            }
            cuts.push(quiet.centre);
        }
        groups.push(group);
    }
    cuts.push(take.len());
    if groups.len() < 2 && planned[0] == 0.0 {
        return None;
    }
    let mut cursor = 0usize;
    let placed = groups
        .iter()
        .enumerate()
        .map(|(k, group)| {
            let lead_in = word_starts[group[0]] - cuts[k] as f64 / rate as f64;
            let start = cursor.max(((planned[group[0]] - lead_in).max(0.0) * rate as f64) as usize);
            cursor = start + cuts[k + 1] - cuts[k];
            Placed { from: cuts[k], to: cuts[k + 1], start }
        })
        .collect();
    Some(placed)
}

/// The placed take: the chunks, faded at the cuts, summed at their starts.
pub(crate) fn apply(take: &[f32], rate: u32, placed: &[Placed]) -> Vec<f32> {
    let fade = FADE_MS * rate as usize / 1000;
    let last = placed.last().expect("a layout holds at least one phrase");
    let mut out = vec![0.0f32; last.start + last.to - last.from];
    for p in placed {
        let mut chunk = take[p.from..p.to].to_vec();
        if chunk.len() > 2 * fade && fade > 1 {
            let n = chunk.len();
            for i in 0..fade {
                let g = i as f32 / (fade - 1) as f32;
                chunk[i] *= g;
                chunk[n - 1 - i] *= g;
            }
        }
        for (o, x) in out[p.start..].iter_mut().zip(&chunk) {
            *o += x;
        }
    }
    out
}

/// How loud the take is at each inner cut, in dB against the take's own RMS
/// (diagnostic): a cut in a real pause reads far below 0, a cut in running
/// speech reads near it and is heard as an abruption.
pub(crate) fn cut_levels_db(take: &[f32], rate: u32, placed: &[Placed]) -> Vec<f32> {
    let rms = |x: &[f32]| (x.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / x.len().max(1) as f64).sqrt().max(1e-9);
    let whole = rms(take);
    let half = FRAME_MS * rate as usize / 2000;
    placed
        .iter()
        .skip(1)
        .map(|p| (20.0 * (rms(&take[p.from.saturating_sub(half)..(p.from + half).min(take.len())]) / whole).log10()) as f32)
        .collect()
}

/// The take with its phrases on the plan, or `None` (block placement): the
/// engine's `anchor_phrases`, kept whole for the laws below (the pipeline
/// calls `layout` and `apply` itself to log what the layout did).
#[cfg(test)]
pub(crate) fn anchor_phrases(take: &[f32], rate: u32, word_starts: &[f64], entries: &[Entry]) -> Option<Vec<f32>> {
    layout(take, rate, word_starts, &plan_items(entries)).map(|placed| apply(take, rate, &placed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_take(case: &serde_json::Value, rate: u32) -> Vec<f32> {
        let mut take = vec![0.0f32; (case["total_s"].as_f64().unwrap() * rate as f64) as usize];
        for segment in case["segments"].as_array().unwrap() {
            let (a, b, amp) = (segment[0].as_f64().unwrap(), segment[1].as_f64().unwrap(), segment[2].as_f64().unwrap() as f32);
            let (a, b) = ((a * rate as f64) as usize, ((b * rate as f64) as usize).min(take.len()));
            take[a..b].fill(amp);
        }
        take
    }

    #[test]
    fn layout_matches_the_python_owner_on_the_golden_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!("dubplace_fixture.json")).unwrap();
        let rate = fixture["rate"].as_u64().unwrap() as u32;
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 30, "the fixture holds the owner's cases");
        let mut anchored = 0;
        for (i, case) in cases.iter().enumerate() {
            let take = fixture_take(case, rate);
            let starts: Vec<f64> = case["word_starts"].as_array().unwrap().iter().map(|t| t.as_f64().unwrap()).collect();
            let plan: Vec<PlanItem> = case["plan"].as_array().unwrap().iter().map(|p| PlanItem { word: p[0].as_bool().unwrap(), t: p[1].as_f64().unwrap() }).collect();
            let expected: Option<Vec<Placed>> = case["layout"].as_array().map(|rows| {
                rows.iter().map(|r| Placed { from: r[0].as_u64().unwrap() as usize, to: r[1].as_u64().unwrap() as usize, start: r[2].as_u64().unwrap() as usize }).collect()
            });
            assert_eq!(layout(&take, rate, &starts, &plan), expected, "case {i}");
            anchored += expected.is_some() as usize;
        }
        assert!(anchored >= 10, "the fixture exercises the anchoring, not only the refusals");
    }

    #[test]
    fn a_collapsed_pause_is_restored_and_phrases_never_overlap() {
        let rate = 1000;
        // two words, 0.1 s of pause left, two words; the plan wants 1.0 s
        let mut take = vec![0.0f32; 1300];
        for (a, b) in [(0, 300), (300, 600), (700, 1000), (1000, 1300)] {
            take[a..b].fill(0.5);
        }
        let entries = vec![
            Entry::Word { notes: vec![0.3] },
            Entry::Word { notes: vec![0.3] },
            Entry::Pause { dur: 1.0, nonverbal: false },
            Entry::Word { notes: vec![0.3] },
            Entry::Word { notes: vec![0.3] },
        ];
        let placed = anchor_phrases(&take, rate, &[0.0, 0.3, 0.7, 1.0], &entries).unwrap();
        assert!(placed[700..1500].iter().all(|&x| x == 0.0), "the pause is silence again");
        assert!(placed[1600] > 0.0, "the second phrase speaks at its planned time");
        // heard words that do not match the plan keep the block placement
        assert_eq!(anchor_phrases(&take, rate, &[0.0, 0.3, 0.7], &entries), None);
    }

    #[test]
    fn a_take_that_speaks_through_the_rest_is_not_cut() {
        let rate = 1000;
        // four words with no pause at all between the second and the third
        let take = vec![0.5f32; 1200];
        let entries = vec![
            Entry::Word { notes: vec![0.3] },
            Entry::Word { notes: vec![0.3] },
            Entry::Pause { dur: 1.0, nonverbal: false },
            Entry::Word { notes: vec![0.3] },
            Entry::Word { notes: vec![0.3] },
        ];
        // one phrase at the onset is left: nothing to anchor, the take stays whole
        assert_eq!(anchor_phrases(&take, rate, &[0.0, 0.3, 0.6, 0.9], &entries), None);
    }

    #[test]
    fn plan_items_accumulate_the_entries() {
        let entries = vec![Entry::Pause { dur: 0.4, nonverbal: true }, Entry::Word { notes: vec![0.2, 0.1] }, Entry::Pause { dur: 0.5, nonverbal: false }, Entry::Word { notes: vec![0.3] }];
        let plan = plan_items(&entries);
        assert_eq!(plan.iter().map(|p| p.word).collect::<Vec<_>>(), vec![false, true, false, true]);
        assert!((plan[3].t - 1.2).abs() < 1e-9);
    }
}
