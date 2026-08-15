//! Buffer-producing speech synthesis, callable from Rust.
//!
//! The command layer of this plugin (`speak` & co.) lets a webview drive a
//! synthesizer that plays straight into the device output. Hosts that own
//! audio output themselves — an app with a Rust playback pipeline — want the
//! opposite shape: *they* hand text in and get PCM back, so system speech can
//! be queued, paused, visualized and replayed exactly like a cloud TTS
//! engine's audio. That is what [`Synthesizer`] is.
//!
//! - iOS / macOS: `AVSpeechSynthesizer.write(_:toBufferCallback:)` over a C
//!   ABI implemented in `apple/TtsStream.m` (see [`crate::apple_stream`]).
//! - Android, Windows, Linux: not implemented; [`Synthesizer::status`]
//!   reports `available == false` so hosts can fall back.
//!
//! The host obtains an instance through [`crate::TtsExt::synthesizer`].

use serde::Serialize;

use crate::models::Voice;

/// What the synthesizer produced, delivered through the callback handed to
/// [`Synthesizer::synthesize`]. Callbacks may run on any thread and must not
/// block.
#[derive(Debug, Clone, PartialEq)]
pub enum SynthesizerEvent {
    /// A chunk of mono float PCM (range `[-1, 1]`) at `sample_rate` Hz. The
    /// rate is whatever the platform voice renders at (22.05/24/48 kHz …) and
    /// can differ between voices; hosts resample as needed.
    Audio { samples: Vec<f32>, sample_rate: u32 },
    /// Synthesis failed. `Ended` follows.
    Error(String),
    /// No further events will be delivered for this job.
    Ended,
}

pub type SynthesizerCallback = Box<dyn Fn(SynthesizerEvent) + Send + Sync + 'static>;

/// What to synthesize.
#[derive(Debug, Clone)]
pub struct SynthesisRequest {
    pub text: String,
    /// Platform voice identifier (see [`Synthesizer::voices`]). `None` or an
    /// unknown id falls back to the best voice for `language`.
    pub voice_id: Option<String>,
    /// BCP-47 tag or bare code ("de-DE", "de") used when `voice_id` does not
    /// resolve. `None` means the system language.
    pub language: Option<String>,
    /// Speech rate, 1.0 = the platform's default rate.
    pub rate: f32,
    /// Pitch multiplier, 1.0 = unchanged (platform clamps, typically 0.5–2.0).
    pub pitch: f32,
    /// Volume 0.0–1.0.
    pub volume: f32,
}

impl SynthesisRequest {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            voice_id: None,
            language: None,
            rate: 1.0,
            pitch: 1.0,
            volume: 1.0,
        }
    }
}

/// A running synthesis job. Audio arrives on the callback given to
/// [`Synthesizer::synthesize`] until `Ended`; [`cancel`](Self::cancel) aborts
/// early (the callback still receives `Ended`). Dropping a job without
/// cancelling lets it run to completion.
pub trait SynthesisJob: Send {
    fn cancel(self: Box<Self>);
}

/// Availability of buffer-producing synthesis on this platform, as the host's
/// settings UI wants to show it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SynthesizerStatus {
    /// Whether [`Synthesizer::synthesize`] can work on this platform at all.
    pub available: bool,
    /// Why not, when `available` is false.
    pub reason: Option<String>,
}

/// Factory for buffer-producing synthesis jobs.
///
/// Registered in Tauri state by [`crate::init`]; hosts reach it through
/// [`crate::TtsExt::synthesizer`].
pub trait Synthesizer: Send + Sync {
    /// Start synthesizing `request.text`. Non-blocking: audio streams in on
    /// `on_event` as it is rendered, followed by exactly one `Ended`.
    fn synthesize(
        &self,
        request: SynthesisRequest,
        on_event: SynthesizerCallback,
    ) -> crate::Result<Box<dyn SynthesisJob>>;

    /// The installed voices. Non-blocking.
    fn voices(&self) -> crate::Result<Vec<Voice>>;

    /// The voice a [`SynthesisRequest`] without a resolvable `voice_id` uses
    /// for `language` (bare code or full tag) — the platform's preferred
    /// voice, so hosts can show what will actually be spoken. `None` when
    /// the platform has no voice at all.
    fn default_voice(&self, language: &str) -> Option<Voice>;

    /// Whether synthesis is available at all. Non-blocking.
    fn status(&self) -> SynthesizerStatus;
}
