//! Short tones for things that happen while a game has the screen: there's nowhere else to show
//! that a clip saved, or that it didn't.

#[derive(Clone, Copy)]
pub enum Cue {
    ClipSaved,
    RecordingStarted,
    RecordingStopped,
    Failed,
    /// A marker went into the recording.
    Marker,
    /// A link to a clip is on the clipboard.
    LinkCopied,
    /// A key that has nothing to act on right now, like a marker with no recording going.
    Nothing,
}

const RATE: u32 = 44_100;
/// How many kinds of `Cue` there are.
#[cfg_attr(not(windows), allow(dead_code))]
const CUES: usize = 7;
const VOLUME: f32 = 0.16;

/// The clip cue plays over every save, usually in headphones, so it's two low plucked notes that
/// ease in and ring out rather than a beep.
fn chime() -> Vec<i16> {
    pluck(&[(523.25, 0.0), (659.25, 0.11)], 0.4, 0.09) // C5, E5
}

/// Plucked notes as (hertz, start second), each ringing for `ring` seconds.
fn pluck(strikes: &[(f32, f32)], ring: f32, level: f32) -> Vec<i16> {
    const ATTACK: f32 = 0.012;
    let last = strikes.iter().map(|s| s.1).fold(0.0, f32::max);
    let count = ((last + ring) * RATE as f32) as u32;
    (0..count)
        .map(|n| {
            let now = n as f32 / RATE as f32;
            let mix: f32 = strikes
                .iter()
                .filter(|&&(_, start)| now >= start && now < start + ring)
                .map(|&(frequency, start)| {
                    let t = now - start;
                    let rise = (t / ATTACK).min(1.0);
                    let fall = (1.0 - t / ring).powi(3);
                    // A trace of the octave gives it some body without adding any edge.
                    let phase = t * frequency * std::f32::consts::TAU;
                    (phase.sin() + 0.12 * (phase * 2.0).sin()) * rise * rise * fall
                })
                .sum();
            (mix * level * 32767.0) as i16
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
        // One short high pluck, quieter than a save, so it doesn't pull attention mid-game.
        Cue::Marker => pluck(&[(987.77, 0.0)], 0.18, 0.07), // B5
        // Three notes going up, brighter than a save, since it comes a while after one.
        Cue::LinkCopied => pluck(&[(783.99, 0.0), (1046.5, 0.08), (1318.51, 0.16)], 0.35, 0.08), // G5, C6, E6
        // A soft low pluck, nothing like the alarm of a failure.
        Cue::Nothing => pluck(&[(220.0, 0.0)], 0.16, 0.06), // A3
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
    static SOUNDS: [OnceLock<Vec<u8>>; CUES] = [const { OnceLock::new() }; CUES];
    let sound = SOUNDS[cue as usize].get_or_init(|| wav(cue));
    unsafe {
        let _ = PlaySoundW(PCWSTR(sound.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(not(windows))]
pub fn play(cue: Cue) {
    let _ = wav(cue);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cue_is_a_short_sound() {
        let all = [Cue::ClipSaved, Cue::RecordingStarted, Cue::RecordingStopped, Cue::Failed, Cue::Marker, Cue::LinkCopied, Cue::Nothing];
        assert_eq!(all.len(), CUES);
        for (i, cue) in all.into_iter().enumerate() {
            assert_eq!(cue as usize, i, "the sounds are kept by position");
            let data = wav(cue);
            let seconds = (data.len() - 44) as f32 / 2.0 / RATE as f32;
            assert!((0.05..1.0).contains(&seconds), "{i}: {seconds} s");
        }
    }
}
