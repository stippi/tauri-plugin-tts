// Buffer-producing speech synthesis over a C ABI (iOS + macOS).
//
// The host app owns audio output (a Rust playback pipeline). Instead of
// letting AVSpeechSynthesizer play into the device, we drive
// `-[AVSpeechSynthesizer writeUtterance:toBufferCallback:]` and hand every
// PCM buffer to Rust through the `rust_tts_stream_*` callbacks the plugin's
// Rust side exports. No WKWebView, no Tauri invoke bridge, no competing
// audio session configuration — synthesis becomes a pure function from text
// to samples the host can queue, pause, visualize and replay like any other
// TTS engine's audio.
//
// Compiled by the crate's build.rs via `cc` for `target_os = "ios"` and
// `"macos"`; the same source serves both.
//
// Threading: `tts_stream_*` may be called from any thread. Job state is
// serialized on a private serial queue; the synthesizer's buffer callback and
// delegate methods arrive on the main queue (the framework requires a running
// main run loop — a Tauri app always has one) and hop onto that queue. Rust
// callbacks are invoked from that queue and must not block (they push into a
// channel).

#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#include <stdint.h>

// MARK: - Rust callbacks (exported by src/apple_stream.rs)

extern void rust_tts_stream_on_audio(uint64_t job_id, const float *samples, uint32_t count,
                                     double sample_rate);
extern void rust_tts_stream_on_error(uint64_t job_id, const char *message);
extern void rust_tts_stream_on_ended(uint64_t job_id);

// Error codes returned by `tts_stream_start`. Mirrored in Rust.
enum TtsStreamStartError {
    TtsStreamStartOk = 0,
    TtsStreamStartEmptyText = 1,
    TtsStreamStartAlreadyRunning = 2,
    TtsStreamStartNoVoice = 3,
};

// MARK: - Shared state

static dispatch_queue_t tts_stream_queue(void) {
    static dispatch_queue_t queue;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        queue = dispatch_queue_create("com.yellowbites.tts-stream", DISPATCH_QUEUE_SERIAL);
    });
    return queue;
}

@class TtsStreamJob;

/// Live jobs by id. Ids are minted by Rust; a stale id is a no-op.
static NSMutableDictionary<NSNumber *, TtsStreamJob *> *tts_stream_jobs(void) {
    static NSMutableDictionary *jobs;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        jobs = [NSMutableDictionary dictionary];
    });
    return jobs;
}

/// Convert whatever PCM layout the synthesizer produced into mono float32.
/// Returns nil if the buffer's format is not understood.
static NSData *mono_float_samples(AVAudioPCMBuffer *pcm) {
    AVAudioFormat *format = pcm.format;
    AVAudioFrameCount frames = pcm.frameLength;
    AVAudioChannelCount channels = format.channelCount;
    if (channels == 0) {
        return nil;
    }
    BOOL interleaved = format.isInterleaved;
    NSMutableData *mono = [NSMutableData dataWithLength:frames * sizeof(float)];
    float *out = (float *)mono.mutableBytes;

    switch (format.commonFormat) {
    case AVAudioPCMFormatFloat32: {
        float *const *data = pcm.floatChannelData;
        if (!data) return nil;
        for (AVAudioFrameCount f = 0; f < frames; f++) {
            float acc = 0;
            for (AVAudioChannelCount c = 0; c < channels; c++) {
                acc += interleaved ? data[0][f * channels + c] : data[c][f];
            }
            out[f] = acc / (float)channels;
        }
        break;
    }
    case AVAudioPCMFormatInt16: {
        int16_t *const *data = pcm.int16ChannelData;
        if (!data) return nil;
        for (AVAudioFrameCount f = 0; f < frames; f++) {
            float acc = 0;
            for (AVAudioChannelCount c = 0; c < channels; c++) {
                int16_t s = interleaved ? data[0][f * channels + c] : data[c][f];
                acc += (float)s / 32768.0f;
            }
            out[f] = acc / (float)channels;
        }
        break;
    }
    case AVAudioPCMFormatInt32: {
        int32_t *const *data = pcm.int32ChannelData;
        if (!data) return nil;
        for (AVAudioFrameCount f = 0; f < frames; f++) {
            float acc = 0;
            for (AVAudioChannelCount c = 0; c < channels; c++) {
                int32_t s = interleaved ? data[0][f * channels + c] : data[c][f];
                acc += (float)s / 2147483648.0f;
            }
            out[f] = acc / (float)channels;
        }
        break;
    }
    default:
        return nil;
    }
    return mono;
}

// MARK: - One synthesis job

@interface TtsStreamJob : NSObject <AVSpeechSynthesizerDelegate>
@property(nonatomic, readonly) uint64_t jobId;
@property(nonatomic, strong) AVSpeechSynthesizer *synthesizer;
@property(nonatomic, assign) BOOL ended;
@property(nonatomic, assign) BOOL cancelled;
- (instancetype)initWithId:(uint64_t)jobId;
- (void)startWithUtterance:(AVSpeechUtterance *)utterance;
- (void)cancel;
@end

@implementation TtsStreamJob

- (instancetype)initWithId:(uint64_t)jobId {
    self = [super init];
    if (self) {
        _jobId = jobId;
        _synthesizer = [[AVSpeechSynthesizer alloc] init];
        _synthesizer.delegate = self;
        _ended = NO;
        _cancelled = NO;
    }
    return self;
}

- (void)startWithUtterance:(AVSpeechUtterance *)utterance {
    __weak TtsStreamJob *weakSelf = self;
    // The framework hands us one buffer at a time and — per the docs — an
    // empty buffer once the utterance is complete. The buffer is only
    // guaranteed valid during the callback, so convert here and hand the copy
    // to the stream queue.
    [self.synthesizer writeUtterance:utterance
                    toBufferCallback:^(AVAudioBuffer *_Nonnull buffer) {
                        AVAudioPCMBuffer *pcm = [buffer isKindOfClass:[AVAudioPCMBuffer class]]
                                                    ? (AVAudioPCMBuffer *)buffer
                                                    : nil;
                        NSString *failure = nil;
                        NSData *mono = nil;
                        double sampleRate = pcm ? pcm.format.sampleRate : 0;
                        BOOL finished = NO;
                        if (!pcm) {
                            failure = @"Synthesizer produced a non-PCM buffer";
                        } else if (pcm.frameLength == 0) {
                            finished = YES;
                        } else {
                            mono = mono_float_samples(pcm);
                            if (!mono) {
                                failure = @"Synthesizer produced an unsupported PCM format";
                            }
                        }
                        dispatch_async(tts_stream_queue(), ^{
                            TtsStreamJob *job = weakSelf;
                            if (!job || job.ended) return;
                            if (failure) {
                                [job failWithMessage:failure];
                            } else if (finished) {
                                [job finish];
                            } else {
                                uint32_t count = (uint32_t)(mono.length / sizeof(float));
                                rust_tts_stream_on_audio(job.jobId, (const float *)mono.bytes,
                                                         count, sampleRate);
                            }
                        });
                    }];
}

- (void)cancel {
    dispatch_async(tts_stream_queue(), ^{
        if (self.ended) return;
        self.cancelled = YES;
        [self.synthesizer stopSpeakingAtBoundary:AVSpeechBoundaryImmediate];
        [self finish];
    });
}

// MARK: Helpers (on the stream queue)

- (void)failWithMessage:(NSString *)message {
    if (self.ended) return;
    rust_tts_stream_on_error(self.jobId, message.UTF8String);
    [self finish];
}

- (void)finish {
    if (self.ended) return;
    self.ended = YES;
    rust_tts_stream_on_ended(self.jobId);
    // Release the registry's reference outside the current callback frame so
    // the synthesizer never deallocates while one of its callbacks is on the
    // stack.
    uint64_t jobId = self.jobId;
    dispatch_async(tts_stream_queue(), ^{
        [tts_stream_jobs() removeObjectForKey:@(jobId)];
    });
}

// MARK: AVSpeechSynthesizerDelegate (main queue)

// Belt and braces: some OS versions have been seen to skip the terminating
// empty buffer. The delegate's finish/cancel notifications end the job too;
// whichever arrives first wins, the other is a no-op.
- (void)speechSynthesizer:(AVSpeechSynthesizer *)synthesizer
    didFinishSpeechUtterance:(AVSpeechUtterance *)utterance {
    dispatch_async(tts_stream_queue(), ^{
        [self finish];
    });
}

- (void)speechSynthesizer:(AVSpeechSynthesizer *)synthesizer
    didCancelSpeechUtterance:(AVSpeechUtterance *)utterance {
    dispatch_async(tts_stream_queue(), ^{
        if (!self.cancelled && !self.ended) {
            [self failWithMessage:@"Synthesis was cancelled by the system"];
        } else {
            [self finish];
        }
    });
}

@end

// MARK: - C ABI (called from Rust)

static AVSpeechSynthesisVoice *tts_stream_pick_voice(const char *voiceId, const char *language) {
    if (voiceId && voiceId[0] != '\0') {
        NSString *wanted = [NSString stringWithUTF8String:voiceId];
        for (AVSpeechSynthesisVoice *voice in [AVSpeechSynthesisVoice speechVoices]) {
            if ([voice.identifier isEqualToString:wanted]) {
                return voice;
            }
        }
        NSLog(@"[TtsStream] voice %@ not installed, falling back to language default", wanted);
    }
    if (language && language[0] != '\0') {
        NSString *lang = [NSString stringWithUTF8String:language];
        AVSpeechSynthesisVoice *voice = [AVSpeechSynthesisVoice voiceWithLanguage:lang];
        if (voice) return voice;
        // Bare code ("de"): take the first installed voice of that language.
        NSString *prefix = [lang lowercaseString];
        for (AVSpeechSynthesisVoice *candidate in [AVSpeechSynthesisVoice speechVoices]) {
            if ([[candidate.language lowercaseString] hasPrefix:prefix]) {
                return candidate;
            }
        }
    }
    return [AVSpeechSynthesisVoice voiceWithLanguage:[AVSpeechSynthesisVoice currentLanguageCode]];
}

/// Start a synthesis job. Returns 0 on success or a `TtsStreamStartError`.
///
/// `rate` is the host's speed (1.0 = the platform's default rate), mapped
/// onto AVSpeechUtterance's [min, max] around `AVSpeechUtteranceDefaultSpeechRate`.
/// `voice_id`/`language` may be NULL or empty; the voice falls back to the
/// language, then to the system language.
int32_t tts_stream_start(uint64_t job_id, const char *text, const char *voice_id,
                         const char *language, float rate, float pitch, float volume) {
    if (!text || text[0] == '\0') {
        return TtsStreamStartEmptyText;
    }
    NSString *string = [NSString stringWithUTF8String:text];
    if (string.length == 0) {
        return TtsStreamStartEmptyText;
    }

    AVSpeechSynthesisVoice *voice = tts_stream_pick_voice(voice_id, language);
    if (!voice) {
        return TtsStreamStartNoVoice;
    }

    AVSpeechUtterance *utterance = [AVSpeechUtterance speechUtteranceWithString:string];
    utterance.voice = voice;
    float mapped = AVSpeechUtteranceDefaultSpeechRate * (rate > 0 ? rate : 1.0f);
    utterance.rate = fminf(fmaxf(mapped, AVSpeechUtteranceMinimumSpeechRate),
                           AVSpeechUtteranceMaximumSpeechRate);
    utterance.pitchMultiplier = fminf(fmaxf(pitch > 0 ? pitch : 1.0f, 0.5f), 2.0f);
    utterance.volume = fminf(fmaxf(volume, 0.0f), 1.0f);

    __block int32_t result = TtsStreamStartOk;
    dispatch_sync(tts_stream_queue(), ^{
        NSMutableDictionary *jobs = tts_stream_jobs();
        if (jobs[@(job_id)] != nil) {
            result = TtsStreamStartAlreadyRunning;
            return;
        }
        TtsStreamJob *job = [[TtsStreamJob alloc] initWithId:job_id];
        jobs[@(job_id)] = job;
        [job startWithUtterance:utterance];
    });
    return result;
}

void tts_stream_cancel(uint64_t job_id) {
    __block TtsStreamJob *job = nil;
    dispatch_sync(tts_stream_queue(), ^{
        job = tts_stream_jobs()[@(job_id)];
    });
    [job cancel];
}

/// Enumerate the installed voices. `visit` is called once per voice with
/// (ctx, identifier, name, language, quality) where quality is 1 default,
/// 2 enhanced, 3 premium (0 unknown).
void tts_stream_list_voices(void *ctx, void (*visit)(void *ctx, const char *identifier,
                                                     const char *name, const char *language,
                                                     int32_t quality)) {
    for (AVSpeechSynthesisVoice *voice in [AVSpeechSynthesisVoice speechVoices]) {
        int32_t quality = 0;
        switch (voice.quality) {
        case AVSpeechSynthesisVoiceQualityDefault:
            quality = 1;
            break;
        case AVSpeechSynthesisVoiceQualityEnhanced:
            quality = 2;
            break;
        default:
            // AVSpeechSynthesisVoiceQualityPremium (iOS 16 / macOS 13+) — compare by
            // raw value so the file still compiles against older SDKs.
            quality = voice.quality == 3 ? 3 : 0;
            break;
        }
        visit(ctx, voice.identifier.UTF8String, voice.name.UTF8String, voice.language.UTF8String,
              quality);
    }
}

/// The voice `tts_stream_start` falls back to for `language` when no voice id
/// resolves. Calls `visit` once (or never, if the platform has no voice at all).
void tts_stream_default_voice(const char *language, void *ctx,
                              void (*visit)(void *ctx, const char *identifier, const char *name,
                                            const char *language, int32_t quality)) {
    AVSpeechSynthesisVoice *voice = tts_stream_pick_voice(NULL, language);
    if (!voice) return;
    visit(ctx, voice.identifier.UTF8String, voice.name.UTF8String, voice.language.UTF8String, 0);
}

/// Test/example helper: pump the main run loop for `seconds`. AVSpeechSynthesizer
/// delivers its buffers via the main queue, which a plain command-line process
/// (cargo example) has to drain explicitly; a Tauri app's main loop already does.
void tts_stream_pump_main_run_loop(double seconds) {
    [[NSRunLoop mainRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:seconds]];
}
