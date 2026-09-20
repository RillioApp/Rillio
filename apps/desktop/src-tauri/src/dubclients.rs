//! The dub worker's three sidecar clients (decision D1/D1b): the translator on
//! `llama-server`, the voice on `llama-tts-server`, the transcriber on
//! `whisper-server`. Each is the exact request the Stage-0/1 bench made, so
//! the product measures what the gates measured (R2):
//!   - translate: the G3 prompt verbatim (`stage0-gates/g3/score.py`, one
//!     owner of its wording), temperature 0, thinking off, first line kept;
//!   - synthesize: `/v1/audio/speech` with the line's reference as a 16 kHz
//!     WAV, the G5 line-reference / speaker-turn mechanism;
//!   - transcribe: `/inference` with a 16 kHz WAV, JSON text back;
//!   - the verified take (G5 arm pcsv): synthesize, transcribe back, retry
//!     with a new seed while the heard text drifts, keep the best of three.
//!
//! Blocking API driven by `tauri::async_runtime::block_on`, like
//! `sidecar.rs`: call from the worker's own thread, never from inside the
//! async runtime.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::autosync::{parse_wav_s16, PcmFormat};

/// The G3 prompt, verbatim from `g3/score.py` (`{budget}`, `{lang}`, `{src}`).
const SYSTEM_PROMPT: &str = "You translate film subtitle lines from {src} to {lang}. Output ONLY the \
{lang} translation of the LAST line, nothing else: no quotes, no notes. Keep \
it as short and natural as spoken dialogue; the translation must be readable \
in the same time as the original, so it should not be longer than about \
{budget} syllables ({chars} characters).";
const TRANSLATE_MAX_TOKENS: u32 = 96;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// The bench's take seeds (G5 arm pcsv): a new seed per retry.
const TAKE_SEEDS: [u32; 3] = [42, 7, 1234];
/// A take whose heard text differs from the line by more than this changed
/// the sentence, not just an accent (G5's content-faithful line).
const FAITHFUL_WER: f64 = 0.2;
/// VoxCPM2 generates one step per this much audio; a request's `max_steps`
/// is the runaway guard (D5: the server stops past a bound derived from the
/// room). The server's own default cap is 200 steps = 32 s.
const TTS_STEPS_PER_S: f32 = 6.25;
const TTS_MAX_STEPS: u32 = 200;
const PROMPT_RATE: u32 = 16_000;
const TTS_RATE: u32 = 48_000;

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("dubclients: http client: {e}"))
}

fn post_json(client: &reqwest::Client, url: &str, body: &Value) -> Result<(u16, Vec<u8>), String> {
    tauri::async_runtime::block_on(async {
        let res = client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("dubclients: POST {url}: {e}"))?;
        let status = res.status().as_u16();
        let bytes = res.bytes().await.map_err(|e| format!("dubclients: reading {url}: {e}"))?;
        Ok((status, bytes.to_vec()))
    })
}

/// A PCM WAV (16-bit) for a sidecar request body.
pub(crate) fn wav_bytes(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
    let data = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub struct Translator {
    client: reqwest::Client,
    url: String,
}

impl Translator {
    pub fn new(base_url: &str) -> Result<Self, String> {
        Ok(Self { client: client()?, url: format!("{}/v1/chat/completions", base_url.trim_end_matches('/')) })
    }

    /// One line, with the previous source lines as context and a target-length
    /// budget in syllables (`dubfit::budget_syllables`: the source's spoken
    /// time at the corpus tempo; the prompt states it in characters too).
    /// `about` (the title's name and synopsis) settles ambiguous words: a
    /// homophone misheard by the recognizer ("relic" / "foreign object" in
    /// Japanese) still translates to the story's word.
    pub fn translate(&self, src: &str, context: &[String], src_lang: &str, lang: &str, budget_syllables: usize, about: Option<&str>) -> Result<String, String> {
        let mut system = SYSTEM_PROMPT
            .replace("{budget}", &budget_syllables.to_string())
            .replace("{chars}", &crate::dubfit::budget_chars(budget_syllables).to_string())
            .replace("{lang}", lang)
            .replace("{src}", src_lang);
        if let Some(about) = about {
            system.push_str(&format!(" The lines are from: {about}"));
        }
        let user = format!("Previous lines:\n{}\n\nLine to translate:\n{src}", context.join("\n"));
        let body = json!({
            "model": "dub",
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "temperature": 0,
            "max_tokens": TRANSLATE_MAX_TOKENS,
            "chat_template_kwargs": {"enable_thinking": false},
        });
        let (status, bytes) = post_json(&self.client, &self.url, &body)?;
        if status != 200 {
            return Err(format!("dubclients: translator answered {status}: {}", String::from_utf8_lossy(&bytes)));
        }
        let parsed: Value = serde_json::from_slice(&bytes).map_err(|e| format!("dubclients: translator json: {e}"))?;
        let text = parsed["choices"][0]["message"]["content"]
            .as_str()
            .ok_or("dubclients: translator answer has no choices[0].message.content")?
            .trim();
        // A model that still thinks aloud despite the flag is used for its answer only.
        let text = text.split_once("</think>").map_or(text, |(_, after)| after.trim());
        Ok(text.lines().next().unwrap_or("").trim().to_owned())
    }
}

pub struct Tts {
    client: reqwest::Client,
    url: String,
}

impl Tts {
    pub fn new(base_url: &str) -> Result<Self, String> {
        Ok(Self { client: client()?, url: format!("{}/v1/audio/speech", base_url.trim_end_matches('/')) })
    }

    /// `text` in the voice of `reference_16k` (mono s16 at 16 kHz), 48 kHz mono
    /// f32 back. `style_16k` (the source line's own separated voice, same
    /// format) is the dub model's second prefix segment: the delivery the
    /// take should follow (M1); `None` is the plain clone. `instrument`
    /// (`instrument::Instrument::patches`, M3) is the reference's voice as
    /// pseudo-patches the sidecar prepends to the reference segment, sent as
    /// `instrument_feats`: float32 little-endian, base64. `seed` picks the
    /// take; generation stops at `max_s` of audio.
    pub fn synthesize(&self, text: &str, reference_16k: &[i16], style_16k: Option<&[i16]>, instrument: Option<&[f32]>, seed: u32, max_s: f32) -> Result<Vec<f32>, String> {
        let encode = |pcm: &[i16]| base64::engine::general_purpose::STANDARD.encode(wav_bytes(pcm, PROMPT_RATE, 1));
        let max_steps = ((max_s * TTS_STEPS_PER_S).ceil() as u32).clamp(1, TTS_MAX_STEPS);
        let mut body = json!({"input": text, "reference_audio": encode(reference_16k), "response_format": "wav", "seed": seed, "max_steps": max_steps});
        if let Some(style) = style_16k {
            body["style_audio"] = json!(encode(style));
        }
        if let Some(patches) = instrument {
            let bytes: Vec<u8> = patches.iter().flat_map(|x| x.to_le_bytes()).collect();
            body["instrument_feats"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
        let (status, bytes) = post_json(&self.client, &self.url, &body)?;
        if status != 200 {
            return Err(format!("dubclients: tts answered {status}: {}", String::from_utf8_lossy(&bytes)));
        }
        let samples = parse_wav_s16(&bytes, PcmFormat { rate: TTS_RATE, channels: 1 }).map_err(|e| format!("dubclients: tts wav: {e}"))?;
        Ok(samples.iter().map(|&s| s as f32 / 32768.0).collect())
    }
}

pub struct Asr {
    client: reqwest::Client,
    url: String,
}

/// What the recognizer heard: the text, and the DTW point of every text
/// token (seconds into the audio, ascending; whisper's `t_dtw`, given when
/// the server runs with DTW timestamps, `sidecar::SidecarModels::Asr`).
/// A token point lies inside its word, so a voiced run holding none is a
/// sound without a word (`dubhandle::nonverbal_phrases`). Punctuation
/// tokens carry no word and are left out. `word_starts_s` holds the point
/// of the FIRST token of every whitespace-delimited word, in the order
/// heard: where each word of a take starts (`dubplace`).
#[derive(Debug, Clone, PartialEq)]
pub struct Heard {
    pub text: String,
    pub token_points_s: Vec<f64>,
    pub word_starts_s: Vec<f64>,
}

/// whisper's timestamps are in 10 ms ticks.
const WHISPER_TICKS_PER_S: f64 = 100.0;

impl Asr {
    pub fn new(base_url: &str) -> Result<Self, String> {
        Ok(Self { client: client()?, url: format!("{}/inference", base_url.trim_end_matches('/')) })
    }

    /// What `pcm_16k` (mono s16) says. `language` fixes the decoder's language
    /// (the dub verifier passes "en"; the transcriber passes the stream's).
    /// `prompt` is whisper's initial prompt: names and words of the title,
    /// which bias the decoder toward them (a homophone picks the right
    /// characters when the story's word is in the prompt).
    pub fn transcribe(&self, pcm_16k: &[i16], language: &str, prompt: Option<&str>) -> Result<Heard, String> {
        let mut fields = vec![("language", language)];
        if let Some(prompt) = prompt {
            fields.push(("prompt", prompt));
        }
        let parsed = self.inference(pcm_16k, &fields, "verbose_json")?;
        let text = parsed["text"].as_str().ok_or("dubclients: asr answer has no text")?.trim().to_owned();
        // the text tokens in the order heard: (starts a word, its DTW point)
        let mut tokens: Vec<(bool, f64)> = Vec::new();
        let mut word_open = false; // whether the current word already has a token with a point
        for token in parsed["segments"].as_array().into_iter().flatten().filter_map(|segment| segment["words"].as_array()).flatten() {
            let Some(piece) = token["word"].as_str() else { continue };
            if piece.starts_with(char::is_whitespace) {
                word_open = false;
            }
            if !piece.chars().any(char::is_alphanumeric) {
                continue;
            }
            let Some(t) = token["t_dtw"].as_i64().filter(|&t| t >= 0) else { continue };
            tokens.push((!word_open, t as f64 / WHISPER_TICKS_PER_S));
            word_open = true;
        }
        let word_starts_s = tokens.iter().filter(|(starts, _)| *starts).map(|&(_, t)| t).collect();
        let mut token_points_s: Vec<f64> = tokens.iter().map(|&(_, t)| t).collect();
        token_points_s.sort_by(f64::total_cmp);
        Ok(Heard { text, token_points_s, word_starts_s })
    }

    /// The spoken language of `pcm_16k` as whisper names it in full, lower
    /// case ("japanese"): the server's `detect_language` mode.
    pub fn detect_language(&self, pcm_16k: &[i16]) -> Result<String, String> {
        let parsed = self.inference(pcm_16k, &[("detect_language", "true")], "json")?;
        Ok(parsed["language"].as_str().ok_or("dubclients: asr detection answer has no language")?.trim().to_lowercase())
    }

    fn inference(&self, pcm_16k: &[i16], fields: &[(&str, &str)], response_format: &str) -> Result<Value, String> {
        let boundary = "rillio-dub-boundary";
        let mut body = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        };
        for (name, value) in fields {
            field(name, value);
        }
        field("response_format", response_format);
        field("temperature", "0");
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"take.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(&wav_bytes(pcm_16k, PROMPT_RATE, 1));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = self.url.clone();
        let (status, bytes) = tauri::async_runtime::block_on(async {
            let res = self
                .client
                .post(&url)
                .header("content-type", format!("multipart/form-data; boundary={boundary}"))
                .body(body)
                .send()
                .await
                .map_err(|e| format!("dubclients: POST {url}: {e}"))?;
            let status = res.status().as_u16();
            let bytes = res.bytes().await.map_err(|e| format!("dubclients: reading {url}: {e}"))?;
            Ok::<_, String>((status, bytes.to_vec()))
        })?;
        if status != 200 {
            return Err(format!("dubclients: asr answered {status}: {}", String::from_utf8_lossy(&bytes)));
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("dubclients: asr json: {e}"))
    }
}

/// Word error rate of `heard` against `expected`, lowercase, punctuation
/// stripped, as the bench's jiwer pipeline normalizes.
pub fn wer(expected: &str, heard: &str) -> f64 {
    let words = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '\'')
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let (a, b) = (words(expected), words(heard));
    if a.is_empty() {
        return if b.is_empty() { 0.0 } else { 1.0 };
    }
    // Levenshtein over words.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, wa) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, wb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(wa != wb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()] as f64 / a.len() as f64
}

/// Downsample 48 kHz mono to 16 kHz for the verifier (the bench interpolated
/// the same way; the verifier is a check, not the product audio).
fn to_16k(audio_48k: &[f32]) -> Vec<i16> {
    let ratio = TTS_RATE as f32 / PROMPT_RATE as f32;
    let n = (audio_48k.len() as f32 / ratio) as usize;
    (0..n)
        .map(|i| {
            let x = i as f32 * ratio;
            let (k, frac) = (x as usize, x - x.floor());
            let a = audio_48k[k.min(audio_48k.len() - 1)];
            let b = audio_48k[(k + 1).min(audio_48k.len() - 1)];
            ((a + (b - a) * frac).clamp(-1.0, 1.0) * 32767.0) as i16
        })
        .collect()
}

/// A take the verifier kept: its audio ([TTS_RATE]), its WER against the
/// line, and where the recognizer heard each of its words start (seconds
/// into udio): what dubplace anchors the take's phrases by.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedTake {
    pub audio: Vec<f32>,
    pub wer: f64,
    pub word_starts_s: Vec<f64>,
}

/// The verified take (G5 arm pcsv): first take whose heard text holds, else
/// the best of [`TAKE_SEEDS`]. A take that reaches `max_s` never stopped on
/// its own (a runaway) and is discarded; `None` when every take ran away.
/// `spoken` is what the model is asked for (the line, with the delivery
/// handle in front of it once the app builds one); `text` is the plain line
/// the heard take is scored against. Returns the audio and its WER.
pub fn synthesize_verified(tts: &Tts, asr: &Asr, spoken: &str, text: &str, reference_16k: &[i16], style_16k: Option<&[i16]>, instrument: Option<&[f32]>, max_s: f32) -> Result<Option<VerifiedTake>, String> {
    let mut best: Option<VerifiedTake> = None;
    let cap_samples = (max_s * TTS_RATE as f32) as usize;
    for seed in TAKE_SEEDS {
        let audio = tts.synthesize(spoken, reference_16k, style_16k, instrument, seed, max_s)?;
        if audio.len() >= cap_samples {
            tracing::warn!("dubclients: runaway take ({:.1} s) for {text:?}, seed {seed}", audio.len() as f32 / TTS_RATE as f32);
            continue;
        }
        let heard = asr.transcribe(&to_16k(&audio), "en", None)?;
        let score = wer(text, &heard.text);
        if best.as_ref().map_or(true, |b| score < b.wer) {
            best = Some(VerifiedTake { audio, wer: score, word_starts_s: heard.word_starts_s });
        }
        if score <= FAITHFUL_WER {
            break;
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot stand-in server: records the request, answers with `body`.
    fn serve_once(content_type: &'static str, body: Vec<u8>) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                let head_end = request.windows(4).position(|w| w == b"\r\n\r\n");
                if let Some(end) = head_end {
                    let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if request.len() >= end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            request
        });
        (url, handle)
    }

    #[test]
    fn translator_sends_the_g3_prompt_and_keeps_the_first_line() {
        let answer = json!({"choices": [{"message": {"content": "<think>hmm</think>Let me go!\nsecond line"}}]}).to_string();
        let (url, handle) = serve_once("application/json", answer.into_bytes());
        let out = Translator::new(&url).unwrap().translate("行かせて", &["前の行".into()], "Japanese", "English", 7, Some("a tomb raiding anime")).unwrap();
        assert_eq!(out, "Let me go!");
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(request.contains("about 7 syllables (21 characters)"), "{request}");
        assert!(request.contains("from Japanese to English"));
        assert!(request.contains("The lines are from: a tomb raiding anime"));
        assert!(request.contains("\"enable_thinking\":false"));
        assert!(request.contains("Previous lines:\\n前の行\\n\\nLine to translate:\\n行かせて"));
    }

    #[test]
    fn tts_sends_the_reference_as_wav_and_decodes_48k_mono() {
        let samples: Vec<i16> = (0..480).map(|i| (i * 60) as i16).collect();
        let (url, handle) = serve_once("audio/wav", wav_bytes(&samples, TTS_RATE, 1));
        let audio = Tts::new(&url).unwrap().synthesize("Hello", &[1000, -1000, 500], None, None, 42, 4.0).unwrap();
        assert_eq!(audio.len(), 480);
        assert!((audio[1] - 60.0 / 32768.0).abs() < 1e-6);
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(request.contains("\"seed\":42"));
        assert!(request.contains("\"max_steps\":25"), "{request}");
        let reference = base64::engine::general_purpose::STANDARD.encode(wav_bytes(&[1000, -1000, 500], PROMPT_RATE, 1));
        assert!(request.contains(&reference));
        assert!(!request.contains("style_audio"), "a plain clone sends no style segment: {request}");
    }

    #[test]
    fn tts_sends_the_style_segment_as_its_own_wav() {
        let (url, handle) = serve_once("audio/wav", wav_bytes(&[0i16; 48], TTS_RATE, 1));
        Tts::new(&url).unwrap().synthesize("Hello", &[1000, -1000, 500], Some(&[7, 8, 9, 10]), None, 42, 4.0).unwrap();
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        let reference = base64::engine::general_purpose::STANDARD.encode(wav_bytes(&[1000, -1000, 500], PROMPT_RATE, 1));
        let style = base64::engine::general_purpose::STANDARD.encode(wav_bytes(&[7, 8, 9, 10], PROMPT_RATE, 1));
        assert!(request.contains(&format!("\"reference_audio\":\"{reference}\"")), "{request}");
        assert!(request.contains(&format!("\"style_audio\":\"{style}\"")), "{request}");
        assert!(!request.contains("instrument_feats"), "no instrument sends no field: {request}");
    }

    /// The instrument's patches ride as `instrument_feats`: float32
    /// little-endian, base64, in the order the graph gave them (M3 contract).
    #[test]
    fn tts_sends_the_instrument_patches_as_little_endian_floats() {
        let (url, handle) = serve_once("audio/wav", wav_bytes(&[0i16; 48], TTS_RATE, 1));
        let patches: Vec<f32> = (0..crate::instrument::PATCHES * crate::instrument::PATCH_ELEMENTS).map(|i| i as f32 * 0.5 - 3.0).collect();
        Tts::new(&url).unwrap().synthesize("Hello", &[1000, -1000, 500], None, Some(&patches), 42, 4.0).unwrap();
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        let bytes: Vec<u8> = patches.iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(bytes.len(), 4096);
        let want = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert!(request.contains(&format!("\"instrument_feats\":\"{want}\"")), "{request}");
    }

    #[test]
    fn asr_posts_multipart_and_reads_text_and_token_points() {
        // whisper-server's verbose_json: a token point per text token (t_dtw in
        // 10 ms ticks, -1 when absent); punctuation carries no word
        let answer = br#"{"text": " Hello there. ", "segments": [{"text": " Hello there.", "words": [{"word": " Hello", "t_dtw": 30, "start": 0.0}, {"word": ".", "t_dtw": 45}, {"word": " there", "t_dtw": 41}, {"word": " late", "t_dtw": -1}]}]}"#;
        let (url, handle) = serve_once("application/json", answer.to_vec());
        let heard = Asr::new(&url).unwrap().transcribe(&[0i16; 1600], "en", None).unwrap();
        assert_eq!(heard, Heard { text: "Hello there.".into(), token_points_s: vec![0.30, 0.41], word_starts_s: vec![0.30, 0.41] });
        let request = handle.join().unwrap();
        let head = String::from_utf8_lossy(&request).to_string();
        assert!(head.contains("multipart/form-data; boundary=rillio-dub-boundary"));
        assert!(head.contains("name=\"language\"\r\n\r\nen"));
        assert!(head.contains("name=\"response_format\"\r\n\r\nverbose_json"));
        assert!(request.windows(4).any(|w| w == b"RIFF"));
    }

    #[test]
    fn asr_without_dtw_gives_text_and_no_points() {
        let (url, _handle) = serve_once("application/json", br#"{"text": " Hi. "}"#.to_vec());
        let heard = Asr::new(&url).unwrap().transcribe(&[0i16; 1600], "en", None).unwrap();
        assert_eq!(heard, Heard { text: "Hi.".into(), token_points_s: vec![], word_starts_s: vec![] });
    }

    #[test]
    fn asr_detects_the_language_by_name() {
        let (url, handle) = serve_once("application/json", br#"{"language": "Japanese"}"#.to_vec());
        let lang = Asr::new(&url).unwrap().detect_language(&[0i16; 1600]).unwrap();
        assert_eq!(lang, "japanese");
        let head = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(head.contains("name=\"detect_language\"\r\n\r\ntrue"));
    }

    #[test]
    fn wer_matches_the_bench_normalization() {
        assert_eq!(wer("Let me go!", "let me go"), 0.0);
        assert!((wer("Let me go now", "let me go") - 0.25).abs() < 1e-9);
        assert_eq!(wer("hello", ""), 1.0);
        assert_eq!(wer("", ""), 0.0);
        assert!((wer("a b c", "a x c") - 1.0 / 3.0).abs() < 1e-9);
    }

    /// Bench/prod parity (R2) against the resident sidecars. Run with
    /// `RILLIO_DUB_MT_URL`, `RILLIO_DUB_TTS_URL`, `RILLIO_DUB_ASR_URL` and
    /// `RILLIO_DUB_REFERENCE_WAV` (16 kHz mono s16) set:
    /// `cargo test --lib dubclients::tests::live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_round_trip() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
        let wav = std::fs::read(env("RILLIO_DUB_REFERENCE_WAV")).unwrap();
        let reference = parse_wav_s16(&wav, PcmFormat { rate: PROMPT_RATE, channels: 1 }).unwrap();
        let mt = Translator::new(&env("RILLIO_DUB_MT_URL")).unwrap();
        let tts = Tts::new(&env("RILLIO_DUB_TTS_URL")).unwrap();
        let asr = Asr::new(&env("RILLIO_DUB_ASR_URL")).unwrap();

        let t = std::time::Instant::now();
        let line = mt.translate("待ってください。まだ話は終わっていません。", &["お前は誰だ？".into()], "Japanese", "English", 40, None).unwrap();
        eprintln!("translate {:?}: {line:?}", t.elapsed());
        assert!(!line.is_empty());

        let t = std::time::Instant::now();
        let VerifiedTake { audio, wer: score, .. } = synthesize_verified(&tts, &asr, &line, &line, &reference, None, None, 12.0).unwrap().expect("every take ran away");
        eprintln!("synthesize_verified {:?}: {:.2} s of audio, wer {score:.3}", t.elapsed(), audio.len() as f32 / TTS_RATE as f32);
        assert!(audio.len() > TTS_RATE as usize / 2);
        assert!(score <= FAITHFUL_WER, "take drifted: wer {score}");

        let t = std::time::Instant::now();
        let heard = asr.transcribe(&reference, "ja", None).unwrap();
        eprintln!("transcribe reference {:?}: {heard:?}", t.elapsed());
        assert!(!heard.text.is_empty());
    }

    #[test]
    fn downsampling_keeps_length_and_range() {
        let audio: Vec<f32> = (0..4800).map(|i| ((i as f32) * 0.01).sin()).collect();
        let out = to_16k(&audio);
        assert_eq!(out.len(), 1600);
        // The sine reaches its peak inside the buffer: the s16 peak is close to full scale.
        assert!(out.iter().copied().max().unwrap() > 32_000);
        assert!(out.iter().copied().min().unwrap() < -32_000);
    }
}
