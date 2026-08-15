//! Renders one sentence through the buffer-producing synthesizer and reports
//! what came back. Run on macOS: `cargo run --example synth_probe -- "Hallo Welt" de-DE`.

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use tauri_plugin_tts::{SynthesisRequest, SynthesizerEvent};

    let mut args = std::env::args().skip(1);
    let text = args.next().unwrap_or_else(|| "Hello there, this is a test.".to_string());
    let language = args.next().unwrap_or_else(|| "en-US".to_string());

    let synth = tauri_plugin_tts::apple_synthesizer();
    let voices = synth.voices().unwrap();
    println!("{} voices installed, e.g. {:?}", voices.len(), voices.first().map(|v| &v.id));

    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    let _job = synth
        .synthesize(
            SynthesisRequest {
                language: Some(language),
                ..SynthesisRequest::new(text)
            },
            Box::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .expect("job starts");

    let mut samples = 0usize;
    let mut rate = 0u32;
    let mut peak = 0f32;
    let deadline = Instant::now() + Duration::from_secs(15);
    'outer: while Instant::now() < deadline {
        tauri_plugin_tts::pump_main_run_loop(0.05);
        while let Ok(event) = rx.try_recv() {
            match event {
                SynthesizerEvent::Audio { samples: s, sample_rate } => {
                    samples += s.len();
                    rate = sample_rate;
                    peak = s.iter().fold(peak, |p, x| p.max(x.abs()));
                }
                SynthesizerEvent::Error(e) => panic!("synthesis error: {e}"),
                SynthesizerEvent::Ended => break 'outer,
            }
        }
    }
    println!(
        "ended after {:?}: {} samples @ {} Hz = {:.2} s, peak {:.3}",
        started.elapsed(),
        samples,
        rate,
        samples as f32 / rate.max(1) as f32,
        peak
    );
    assert!(samples > 0, "no audio");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("synth_probe only runs on macOS");
}
