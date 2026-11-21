//! Pluggable speech: speech-to-text and text-to-speech.
//!
//! Devices that can do speech on-device (and stream it) should, and talk to
//! BranchMesh in text. For devices that can't, the server offers audio
//! endpoints backed by a [`SpeechToText`] / [`TextToSpeech`] implementation:
//!
//! * [`OpenRouterSpeech`]: `POST /api/v1/audio/transcriptions` (base64 JSON)
//!   and `POST /api/v1/audio/speech` (streams raw `mp3` / `pcm`).
//! * [`MockSpeech`]: offline; "audio" in format `txt` is treated as the
//!   transcript, and synthesis returns a short generated WAV tone.

use async_trait::async_trait;
use branchmesh_core::{Error, Result};
use futures::stream::{self, BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Per-persona voice settings. Unset fields fall back to server defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VoiceConfig {
    /// Speech-to-text model (e.g. `elevenlabs/scribe-v2`).
    #[serde(default)]
    pub stt_model: Option<String>,
    /// Text-to-speech model (e.g. `elevenlabs/eleven-v4-turbo`).
    #[serde(default)]
    pub tts_model: Option<String>,
    /// Model-specific voice id (e.g. `lily`).
    #[serde(default)]
    pub voice: Option<String>,
    /// ISO-639-1 language hint for transcription.
    #[serde(default)]
    pub language: Option<String>,
    /// Delivery instructions for TTS models that support them.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Tell clients to prefer on-device speech when they have it.
    #[serde(default = "yes")]
    pub prefer_on_device: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    #[serde(default)]
    pub seconds: Option<f64>,
    pub provider: String,
    pub model: String,
}

/// Synthesized audio, streamed as it is generated.
pub struct Synthesized {
    pub content_type: String,
    /// `mp3`, `pcm` (16-bit LE mono, rate depends on model) or `wav`.
    pub format: String,
    pub stream: BoxStream<'static, Result<Vec<u8>>>,
}

impl Synthesized {
    pub async fn collect(self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut s = self.stream;
        while let Some(chunk) = s.next().await {
            out.extend(chunk?);
        }
        Ok(out)
    }
}

#[async_trait]
pub trait SpeechToText: Send + Sync {
    fn name(&self) -> &str;
    async fn transcribe(&self, audio: &[u8], format: &str, cfg: &VoiceConfig) -> Result<Transcript>;
}

#[async_trait]
pub trait TextToSpeech: Send + Sync {
    fn name(&self) -> &str;
    /// `format` is `mp3` or `pcm`; providers may substitute (check `Synthesized::format`).
    async fn synthesize(&self, text: &str, format: &str, cfg: &VoiceConfig) -> Result<Synthesized>;
}

/// The speech stack a runtime uses.
#[derive(Clone)]
pub struct Speech {
    pub stt: Arc<dyn SpeechToText>,
    pub tts: Arc<dyn TextToSpeech>,
}

impl Speech {
    pub fn mock() -> Self {
        Speech { stt: Arc::new(MockSpeech), tts: Arc::new(MockSpeech) }
    }

    /// `BRANCHMESH_SPEECH=mock|openrouter`, else OpenRouter when
    /// `OPENROUTER_API_KEY` is set, else the mock.
    pub fn from_env() -> Self {
        let forced = std::env::var("BRANCHMESH_SPEECH").unwrap_or_default().to_ascii_lowercase();
        if forced != "mock" {
            if let Some(or) = OpenRouterSpeech::from_env() {
                tracing::info!("speech: OpenRouter (stt {}, tts {})", or.default_stt, or.default_tts);
                let or = Arc::new(or);
                return Speech { stt: or.clone(), tts: or };
            }
        }
        tracing::info!("speech: offline mock (set OPENROUTER_API_KEY for server-side speech)");
        Speech::mock()
    }
}

// ------------------------------------------------------------------ OpenRouter

pub struct OpenRouterSpeech {
    http: reqwest::Client,
    base_url: String,
    key: String,
    pub default_stt: String,
    pub default_tts: String,
    pub default_voice: String,
    /// Route only to zero-data-retention endpoints.
    pub zdr: bool,
}

impl OpenRouterSpeech {
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("OPENROUTER_API_KEY").ok().filter(|s| !s.is_empty())?;
        let env = |k: &str, d: &str| std::env::var(k).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| d.to_string());
        Some(OpenRouterSpeech {
            http: reqwest::Client::builder().timeout(Duration::from_secs(90)).build().ok()?,
            base_url: env("OPENROUTER_BASE_URL", "https://openrouter.ai/api"),
            key,
            default_stt: env("BRANCHMESH_STT_MODEL", "elevenlabs/scribe-v2"),
            default_tts: env("BRANCHMESH_TTS_MODEL", "elevenlabs/eleven-v4-turbo"),
            default_voice: env("BRANCHMESH_TTS_VOICE", "lily"),
            zdr: std::env::var("BRANCHMESH_SPEECH_ZDR").map(|v| v == "1" || v == "true").unwrap_or(false),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/{path}", self.base_url.trim_end_matches('/'))
    }

    async fn error(resp: reqwest::Response) -> Error {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Error::Other(format!("openrouter speech error {status}: {body}"))
    }
}

/// Minimal standard base64 encoder (avoids an extra dependency).
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let clean: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c).ok_or_else(|| Error::Invalid("invalid base64".into()))? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[async_trait]
impl SpeechToText for OpenRouterSpeech {
    fn name(&self) -> &str {
        "openrouter"
    }

    async fn transcribe(&self, audio: &[u8], format: &str, cfg: &VoiceConfig) -> Result<Transcript> {
        let model = cfg.stt_model.clone().unwrap_or_else(|| self.default_stt.clone());
        let mut body = json!({
            "model": model,
            "input_audio": { "data": base64_encode(audio), "format": format },
        });
        if let Some(lang) = &cfg.language {
            body["language"] = json!(lang);
        }
        if self.zdr {
            body["provider"] = json!({ "zdr": true });
        }
        let resp = self
            .http
            .post(self.url("audio/transcriptions"))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("transcription request failed: {}", branchmesh_core::error_chain(&e))))?;
        if !resp.status().is_success() {
            return Err(Self::error(resp).await);
        }
        let v: Value = resp.json().await.map_err(|e| Error::Other(e.to_string()))?;
        Ok(Transcript {
            text: v["text"].as_str().unwrap_or("").trim().to_string(),
            seconds: v["usage"]["seconds"].as_f64(),
            provider: "openrouter".into(),
            model,
        })
    }
}

#[async_trait]
impl TextToSpeech for OpenRouterSpeech {
    fn name(&self) -> &str {
        "openrouter"
    }

    async fn synthesize(&self, text: &str, format: &str, cfg: &VoiceConfig) -> Result<Synthesized> {
        let format = if format == "pcm" { "pcm" } else { "mp3" };
        let mut body = json!({
            "model": cfg.tts_model.clone().unwrap_or_else(|| self.default_tts.clone()),
            "input": text,
            "voice": cfg.voice.clone().unwrap_or_else(|| self.default_voice.clone()),
            "response_format": format,
        });
        if let Some(i) = &cfg.instructions {
            body["instructions"] = json!(i);
        }
        if self.zdr {
            body["provider"] = json!({ "zdr": true });
        }
        let resp = self
            .http
            .post(self.url("audio/speech"))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("speech request failed: {}", branchmesh_core::error_chain(&e))))?;
        if !resp.status().is_success() {
            return Err(Self::error(resp).await);
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(if format == "mp3" { "audio/mpeg" } else { "audio/pcm" })
            .to_string();
        let stream = resp
            .bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(|e| Error::Other(format!("audio stream interrupted: {e}"))))
            .boxed();
        Ok(Synthesized { content_type, format: format.into(), stream })
    }
}

// ------------------------------------------------------------------ mock

pub struct MockSpeech;

/// A mono 16 kHz 16-bit WAV containing a soft tone whose length tracks the text.
pub fn tone_wav(text: &str) -> Vec<u8> {
    let rate = 16_000u32;
    let secs = (text.split_whitespace().count() as f32 * 0.12).clamp(0.3, 4.0);
    let n = (rate as f32 * secs) as u32;
    let mut pcm = Vec::with_capacity(n as usize * 2);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let env = (1.0 - (t / secs)).max(0.0);
        let s = (t * 2.0 * std::f32::consts::PI * 523.25).sin() * 0.15 * env;
        pcm.extend_from_slice(&((s * i16::MAX as f32) as i16).to_le_bytes());
    }
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend(pcm);
    w
}

#[async_trait]
impl SpeechToText for MockSpeech {
    fn name(&self) -> &str {
        "mock"
    }
    async fn transcribe(&self, audio: &[u8], format: &str, _cfg: &VoiceConfig) -> Result<Transcript> {
        let text = if matches!(format, "txt" | "text") {
            String::from_utf8_lossy(audio).trim().to_string()
        } else {
            format!("[mock transcript of {} bytes of {format} audio]", audio.len())
        };
        Ok(Transcript { text, seconds: None, provider: "mock".into(), model: "mock".into() })
    }
}

#[async_trait]
impl TextToSpeech for MockSpeech {
    fn name(&self) -> &str {
        "mock"
    }
    async fn synthesize(&self, text: &str, _format: &str, _cfg: &VoiceConfig) -> Result<Synthesized> {
        let wav = tone_wav(text);
        Ok(Synthesized { content_type: "audio/wav".into(), format: "wav".into(), stream: stream::iter(vec![Ok(wav)]).boxed() })
    }
}

/// Split streamed text into speakable chunks (sentences), so TTS can start
/// before the full reply is written. Returns complete chunks and keeps the rest.
pub fn take_sentences(buf: &mut String, min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let bytes = buf.as_bytes();
        let mut cut = None;
        for (i, &b) in bytes.iter().enumerate() {
            if matches!(b, b'.' | b'!' | b'?' | b'\n') && i + 1 >= min_len {
                let next = bytes.get(i + 1).copied();
                if next.map(|c| c == b' ' || c == b'\n').unwrap_or(false) {
                    cut = Some(i + 1);
                    break;
                }
            }
        }
        match cut {
            Some(c) => {
                let s: String = buf.drain(..c).collect();
                let s = s.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"hello world!", &[0u8, 255, 128, 7, 9]] {
            assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn sentence_chunking() {
        let mut buf = String::from("Hi Sam! Look both ways. Then cro");
        assert_eq!(take_sentences(&mut buf, 3), vec!["Hi Sam!", "Look both ways."]);
        assert_eq!(buf, " Then cro");
    }

    #[tokio::test]
    async fn mock_speech() {
        let s = Speech::mock();
        let t = s.stt.transcribe(b"hello there", "txt", &VoiceConfig::default()).await.unwrap();
        assert_eq!(t.text, "hello there");
        let a = s.tts.synthesize("hi there friend", "mp3", &VoiceConfig::default()).await.unwrap();
        assert_eq!(a.format, "wav");
        let bytes = a.collect().await.unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
    }
}
