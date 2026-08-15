use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

pub use models::*;

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

#[cfg(any(target_os = "ios", target_os = "macos"))]
mod apple_stream;
mod commands;
mod error;
mod models;
pub mod synthesizer;

pub use error::{Error, Result};
pub use synthesizer::{
    SynthesisJob, SynthesisRequest, Synthesizer, SynthesizerCallback, SynthesizerEvent,
    SynthesizerStatus,
};

/// The Apple synthesizer without a Tauri app around it (examples, tests).
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn apple_synthesizer() -> std::sync::Arc<dyn Synthesizer> {
    std::sync::Arc::new(apple_stream::AppleSynthesizer)
}
#[cfg(any(target_os = "ios", target_os = "macos"))]
#[doc(hidden)]
pub use apple_stream::pump_main_run_loop;

#[cfg(desktop)]
use desktop::Tts;
#[cfg(mobile)]
use mobile::Tts;

/// Shared handle to the platform's buffer-producing [`Synthesizer`].
#[derive(Clone)]
pub struct SharedSynthesizer(std::sync::Arc<dyn Synthesizer>);

impl std::ops::Deref for SharedSynthesizer {
    type Target = dyn Synthesizer;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl SharedSynthesizer {
    pub fn into_arc(self) -> std::sync::Arc<dyn Synthesizer> {
        self.0
    }
}

/// Extensions to [`tauri::App`], [`tauri::AppHandle`] and [`tauri::Window`] to access the tts APIs.
pub trait TtsExt<R: Runtime> {
    fn tts(&self) -> &Tts<R>;
    /// The buffer-producing synthesizer for hosts that own audio output themselves.
    fn synthesizer(&self) -> SharedSynthesizer;
}

impl<R: Runtime, T: Manager<R>> crate::TtsExt<R> for T {
    fn tts(&self) -> &Tts<R> {
        self.state::<Tts<R>>().inner()
    }
    fn synthesizer(&self) -> SharedSynthesizer {
        self.state::<SharedSynthesizer>().inner().clone()
    }
}

/// Platforms without a buffer-producing binding still register a synthesizer
/// so hosts can branch on `status().available`.
#[cfg(not(any(target_os = "ios", target_os = "macos")))]
struct UnsupportedSynthesizer;

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
impl Synthesizer for UnsupportedSynthesizer {
    fn synthesize(
        &self,
        _request: SynthesisRequest,
        _on_event: SynthesizerCallback,
    ) -> Result<Box<dyn SynthesisJob>> {
        Err(Error::OperationFailed(
            "Buffer-producing synthesis is not implemented on this platform".to_string(),
        ))
    }
    fn voices(&self) -> Result<Vec<Voice>> {
        Ok(Vec::new())
    }
    fn status(&self) -> SynthesizerStatus {
        SynthesizerStatus {
            available: false,
            reason: Some(
                "Buffer-producing synthesis is not implemented on this platform".to_string(),
            ),
        }
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("tts")
        .invoke_handler(tauri::generate_handler![
            commands::speak,
            commands::stop,
            commands::get_voices,
            commands::is_speaking,
            commands::is_initialized,
            commands::pause_speaking,
            commands::resume_speaking,
            commands::preview_voice,
            commands::set_background_behavior,
            commands::register_listener
        ])
        .setup(|app, api| {
            #[cfg(mobile)]
            {
                let tts = mobile::init(app, api)?;
                app.manage(tts);
            }
            #[cfg(desktop)]
            {
                let tts = desktop::init(app, api)?;
                app.manage(tts);
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            let synthesizer =
                SharedSynthesizer(std::sync::Arc::new(apple_stream::AppleSynthesizer));
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            let synthesizer = SharedSynthesizer(std::sync::Arc::new(UnsupportedSynthesizer));
            app.manage(synthesizer);
            Ok(())
        })
        .build()
}
