//! iOS / macOS [`Synthesizer`]: `AVSpeechSynthesizer.write(_:toBufferCallback:)`
//! over the C ABI implemented in `apple/TtsStream.m` (compiled by build.rs).
//!
//! Rust mints job ids and registers the host's callback under them; the
//! Objective-C side reports back through the `rust_tts_stream_on_*` exports
//! below. Callbacks for ids that are no longer registered (ended, cancelled)
//! are ignored, so a late framework callback can never reach a dead job.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::models::Voice;
use crate::synthesizer::{
    SynthesisJob, SynthesisRequest, Synthesizer, SynthesizerCallback, SynthesizerEvent,
    SynthesizerStatus,
};

type VoiceVisitor = unsafe extern "C" fn(
    ctx: *mut c_void,
    identifier: *const c_char,
    name: *const c_char,
    language: *const c_char,
    quality: i32,
);

extern "C" {
    fn tts_stream_start(
        job_id: u64,
        text: *const c_char,
        voice_id: *const c_char,
        language: *const c_char,
        rate: f32,
        pitch: f32,
        volume: f32,
    ) -> i32;
    fn tts_stream_cancel(job_id: u64);
    fn tts_stream_list_voices(ctx: *mut c_void, visit: VoiceVisitor);
    fn tts_stream_default_voice(language: *const c_char, ctx: *mut c_void, visit: VoiceVisitor);
}

/// Visitor that appends every reported voice to the `Vec<Voice>` behind `ctx`.
unsafe extern "C" fn collect_voice(
    ctx: *mut c_void,
    identifier: *const c_char,
    name: *const c_char,
    language: *const c_char,
    _quality: i32,
) {
    // SAFETY: `ctx` is the `Vec<Voice>` the caller passed, valid for the
    // duration of the enumerating FFI call.
    let out = &mut *(ctx as *mut Vec<Voice>);
    out.push(Voice {
        id: c_string_arg(identifier),
        name: c_string_arg(name),
        language: c_string_arg(language),
    });
}

// Start error codes (mirror `TtsStreamStartError` in TtsStream.m).
fn describe_start_error(code: i32) -> String {
    match code {
        1 => "Nothing to synthesize (empty text)".to_string(),
        2 => "Job id already running".to_string(),
        3 => "No speech voice installed".to_string(),
        other => format!("Speech synthesizer failed to start (code {other})"),
    }
}

type SharedCallback = Arc<SynthesizerCallback>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn jobs() -> &'static Mutex<HashMap<u64, SharedCallback>> {
    static JOBS: OnceLock<Mutex<HashMap<u64, SharedCallback>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look the callback up without holding the lock while calling it — the
/// host's callback may itself touch this plugin.
fn callback_for(id: u64) -> Option<SharedCallback> {
    jobs().lock().ok()?.get(&id).cloned()
}

fn c_string_arg(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the C side passes a NUL-terminated string that lives for the
    // duration of the call.
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

/// # Safety
/// Called from `TtsStream.m` with `samples` pointing at `count` floats that
/// stay valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn rust_tts_stream_on_audio(
    id: u64,
    samples: *const f32,
    count: u32,
    sample_rate: f64,
) {
    let Some(cb) = callback_for(id) else { return };
    if samples.is_null() || count == 0 {
        return;
    }
    let samples = std::slice::from_raw_parts(samples, count as usize).to_vec();
    cb(SynthesizerEvent::Audio {
        samples,
        sample_rate: sample_rate.round() as u32,
    });
}

#[no_mangle]
pub extern "C" fn rust_tts_stream_on_error(id: u64, message: *const c_char) {
    if let Some(cb) = callback_for(id) {
        cb(SynthesizerEvent::Error(c_string_arg(message)));
    }
}

#[no_mangle]
pub extern "C" fn rust_tts_stream_on_ended(id: u64) {
    // Unregister first so nothing after `Ended` can reach the host.
    let cb = jobs().lock().ok().and_then(|mut s| s.remove(&id));
    if let Some(cb) = cb {
        cb(SynthesizerEvent::Ended);
    }
}

struct AppleJob {
    id: u64,
}

impl SynthesisJob for AppleJob {
    fn cancel(self: Box<Self>) {
        // SAFETY: plain FFI call with a value argument. A stale id is a no-op
        // on the ObjC side; the registry entry goes when `Ended` arrives.
        unsafe { tts_stream_cancel(self.id) };
    }
}

/// Buffer-producing synthesizer backed by AVSpeechSynthesizer.
pub struct AppleSynthesizer;

fn optional_c_string(value: &Option<String>) -> crate::Result<CString> {
    CString::new(value.clone().unwrap_or_default())
        .map_err(|_| crate::Error::OperationFailed("Interior NUL in argument".to_string()))
}

impl Synthesizer for AppleSynthesizer {
    fn synthesize(
        &self,
        request: SynthesisRequest,
        on_event: SynthesizerCallback,
    ) -> crate::Result<Box<dyn SynthesisJob>> {
        let text = CString::new(request.text)
            .map_err(|_| crate::Error::OperationFailed("Interior NUL in text".to_string()))?;
        let voice_id = optional_c_string(&request.voice_id)?;
        let language = optional_c_string(&request.language)?;

        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        jobs()
            .lock()
            .map_err(|_| crate::Error::MutexPoisoned)?
            .insert(id, Arc::new(on_event));

        // SAFETY: all strings outlive the call; ObjC copies them into
        // NSStrings before returning.
        let code = unsafe {
            tts_stream_start(
                id,
                text.as_ptr(),
                voice_id.as_ptr(),
                language.as_ptr(),
                request.rate,
                request.pitch,
                request.volume,
            )
        };
        if code != 0 {
            if let Ok(mut s) = jobs().lock() {
                s.remove(&id);
            }
            return Err(crate::Error::OperationFailed(describe_start_error(code)));
        }
        Ok(Box::new(AppleJob { id }))
    }

    fn voices(&self) -> crate::Result<Vec<Voice>> {
        let mut voices: Vec<Voice> = Vec::new();
        // SAFETY: the visitor only runs during this call and only touches
        // `voices` through the context pointer.
        unsafe {
            tts_stream_list_voices(&mut voices as *mut Vec<Voice> as *mut c_void, collect_voice)
        };
        Ok(voices)
    }

    fn default_voice(&self, language: &str) -> Option<Voice> {
        let language = CString::new(language).ok()?;
        let mut voices: Vec<Voice> = Vec::new();
        // SAFETY: as in `voices`; `language` outlives the call.
        unsafe {
            tts_stream_default_voice(
                language.as_ptr(),
                &mut voices as *mut Vec<Voice> as *mut c_void,
                collect_voice,
            )
        };
        voices.into_iter().next()
    }

    fn status(&self) -> SynthesizerStatus {
        SynthesizerStatus {
            available: true,
            reason: None,
        }
    }
}

/// Pump the process's main run loop for `seconds`. AVSpeechSynthesizer
/// delivers buffers through the main queue: a Tauri app drains it anyway, a
/// bare command-line process (the `synth_probe` example) has to do so itself.
#[doc(hidden)]
pub fn pump_main_run_loop(seconds: f64) {
    extern "C" {
        fn tts_stream_pump_main_run_loop(seconds: f64);
    }
    // SAFETY: plain FFI call; must run on the main thread to be useful.
    unsafe { tts_stream_pump_main_run_loop(seconds) }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// End-to-end synthesis needs the main run loop, which the test harness
    /// does not run on the main thread — `cargo run --example synth_probe`
    /// exercises the real synthesizer instead. What can be checked here is
    /// the voice enumeration (synchronous) and cancel bookkeeping.
    #[test]
    fn lists_installed_voices() {
        let voices = AppleSynthesizer.voices().unwrap();
        assert!(!voices.is_empty());
        assert!(voices.iter().all(|v| !v.id.is_empty() && !v.language.is_empty()));
    }

    #[test]
    fn default_voice_matches_the_language() {
        let voice = AppleSynthesizer.default_voice("de").expect("a German voice");
        assert!(voice.language.to_ascii_lowercase().starts_with("de"), "{voice:?}");
        assert!(AppleSynthesizer.default_voice("en-US").is_some());
    }

    #[test]
    fn empty_text_is_rejected_without_registering_a_job() {
        let err = AppleSynthesizer
            .synthesize(SynthesisRequest::new(""), Box::new(|_| {}))
            .err()
            .expect("empty text must fail");
        assert!(err.to_string().contains("empty text"), "{err}");
        assert!(jobs().lock().unwrap().is_empty());
    }
}
