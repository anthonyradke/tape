//! Short tones for things that happen while a game has the screen: there's nowhere else to show
//! that a clip saved, or that it didn't.

#[derive(Clone, Copy)]
pub enum Cue {
    ClipSaved,
    RecordingStarted,
    RecordingStopped,
    Failed,
}

const RATE: u32 = 44_100;
const VOLUME: f32 = 0.16;

/// The clip cue plays over every save, usually in headphones, so it's two low plucked notes that
/// ease in and ring out rather than a beep.
fn chime() -> Vec<i16> {
    const STRIKES: [(f32, f32); 2] = [(523.25, 0.0), (659.25, 0.11)]; // C5, E5: (hertz, start second)
    const ATTACK: f32 = 0.012;
    const RING: f32 = 0.4;
    const LEVEL: f32 = 0.09;
    let count = ((0.11 + RING) * RATE as f32) as u32;
    (0..count)
        .map(|n| {
            let now = n as f32 / RATE as f32;
            let mix: f32 = STRIKES
                .iter()
                .filter(|&&(_, start)| now >= start && now < start + RING)
                .map(|&(frequency, start)| {
                    let t = now - start;
                    let rise = (t / ATTACK).min(1.0);
                    let fall = (1.0 - t / RING).powi(3);
                    // A trace of the octave gives it some body without adding any edge.
                    let phase = t * frequency * std::f32::consts::TAU;
                    (phase.sin() + 0.12 * (phase * 2.0).sin()) * rise * rise * fall
                })
                .sum();
            (mix * LEVEL * 32767.0) as i16
        })
        .collect()
}

/// (frequency, milliseconds) pairs, played in turn. Frequency 0 is a rest.
fn tones(notes: &[(f32, u32)]) -> Vec<i16> {
    let mut samples: Vec<i16> = Vec::new();
    for &(frequency, ms) in notes {
        let count = RATE * ms / 1000;
        for n in 0..count {
            let t = n as f32 / RATE as f32;
            // Short fades, so the tones don't click.
            let edge = (n.min(count - n) as f32 / (RATE as f32 * 0.005)).min(1.0);
            let value = (t * frequency * std::f32::consts::TAU).sin() * VOLUME * edge;
            samples.push(if frequency == 0.0 { 0 } else { (value * 32767.0) as i16 });
        }
    }
    samples
}

/// A complete 16-bit mono WAV file in memory.
fn wav(cue: Cue) -> Vec<u8> {
    let samples = match cue {
        Cue::ClipSaved => chime(),
        Cue::RecordingStarted => tones(&[(660.0, 90)]),
        Cue::RecordingStopped => tones(&[(440.0, 90)]),
        Cue::Failed => tones(&[(196.0, 140), (0.0, 60), (196.0, 140)]),
    };
    let data = samples.len() as u32 * 2;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(windows)]
pub fn play(cue: Cue) {
    use std::sync::OnceLock;
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

    // Async playback reads from this memory after the call returns, so it lives forever.
    static SOUNDS: [OnceLock<Vec<u8>>; 4] = [OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let sound = SOUNDS[cue as usize].get_or_init(|| wav(cue));
    unsafe {
        let _ = PlaySoundW(PCWSTR(sound.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(not(windows))]
pub fn play(cue: Cue) {
    let _ = wav(cue);
}
