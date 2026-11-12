//! Microphone noise suppression for the recording pipeline.
//!
//! [`MicDenoiser`] puts either engine behind one interface: in-place
//! processing of the pipeline's byte format — interleaved 48 kHz stereo f32
//! samples (±1.0). SpeexDSP runs through the vendored C preprocessor
//! ([`super::speex`]); RNNoise through the pure-Rust `nnnoiseless` crate.
//!
//! Both engines work on 480-frame (10 ms) blocks, which is also what the
//! mixer hands over during a recording (960-frame chunks = 2 blocks). Any
//! trailing partial block — which only ever occurs in a recording's final
//! drain — passes through unprocessed.

use nnnoiseless::{DenoiseState, FRAME_SIZE};

use super::speex::SpeexDenoiser;
use crate::capture::audio::CHANNELS;
use crate::config::NoiseSuppressionMethod;

/// Bytes per interleaved stereo f32 frame.
const STRIDE: usize = CHANNELS as usize * std::mem::size_of::<f32>();

/// 480 frames = 10 ms at 48 kHz — the native block size of both engines.
const FRAMES: usize = 480;

// The two engines must agree on the block size for `process` to be exact.
const _: () = assert!(FRAME_SIZE == FRAMES, "nnnoiseless frame size changed");

enum Engine {
    /// SpeexDSP C preprocessor, one state per channel.
    Speex(SpeexDenoiser),
    /// RNNoise (`nnnoiseless`), one state per channel.
    Rnnoise(Vec<Box<DenoiseState<'static>>>),
}

impl Engine {
    /// Processes exactly [`FRAMES`] interleaved stereo f32 frames.
    fn process(&mut self, data: &[u8]) -> Vec<u8> {
        debug_assert_eq!(data.len(), FRAMES * STRIDE);
        match self {
            Engine::Speex(denoiser) => denoiser.process(data),
            Engine::Rnnoise(states) => {
                // nnnoiseless expects samples in the 16-bit range (±32768),
                // not ±1.0; scale in and out.
                let scale = i16::MAX as f32;
                let mut input = vec![0.0f32; FRAMES];
                let mut output_samples = vec![0.0f32; FRAMES];
                let mut output = vec![0u8; FRAMES * STRIDE];
                for (channel, state) in states.iter_mut().enumerate() {
                    let offset = channel * 4;
                    for (index, frame) in data.as_chunks::<STRIDE>().0.iter().enumerate() {
                        let value = f32::from_le_bytes(
                            frame[offset..offset + 4].try_into().expect("stride slice"),
                        );
                        input[index] = value * scale;
                    }
                    state.process_frame(&mut output_samples, &input);
                    for (index, sample) in output_samples.iter().enumerate() {
                        let out = index * STRIDE + offset;
                        output[out..out + 4]
                            .copy_from_slice(&(sample / scale).to_le_bytes());
                    }
                }
                output
            }
        }
    }
}

/// Microphone noise suppression, engine-agnostic.
///
/// Lives on the audio writer thread (one owner, exclusive access); stateful
/// across chunks, which is what makes spectral suppression smooth.
pub struct MicDenoiser {
    engine: Engine,
}

impl MicDenoiser {
    /// Builds the denoiser for `method`. `level_db` is only meaningful for
    /// SpeexDSP (maximum attenuation in dB, −60 strongest .. 0 weakest);
    /// RNNoise has no user-adjustable strength.
    pub fn new(method: NoiseSuppressionMethod, level_db: i32) -> std::io::Result<Self> {
        let engine = match method {
            NoiseSuppressionMethod::Speex => Engine::Speex(SpeexDenoiser::new(level_db)?),
            NoiseSuppressionMethod::Rnnoise => {
                Engine::Rnnoise((0..CHANNELS).map(|_| DenoiseState::new()).collect())
            }
        };
        Ok(Self { engine })
    }

    /// Denoises interleaved stereo f32 PCM in place, in 480-frame blocks.
    ///
    /// A trailing partial block (fewer than 10 ms, only in the final drain)
    /// is left untouched rather than padded: padding would inject a silence
    /// discontinuity into the denoiser's spectral state for no audible gain.
    pub fn process(&mut self, data: &mut [u8]) {
        let block = FRAMES * STRIDE;
        for chunk in data.chunks_mut(block) {
            if chunk.len() == block {
                let processed = self.engine.process(chunk);
                chunk.copy_from_slice(&processed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white-noise sample in ±`amp`, from the same LCG the
    /// other engine tests use.
    fn white(seed: &mut u32, amp: f32) -> f32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        ((*seed >> 8) as f32 / 16_777_215.0 - 0.5) * 2.0 * amp
    }

    /// Pink noise (1/f power spectrum) via Paul Kellet's standard filter.
    ///
    /// RNNoise suppresses colored noise — room tone, fans, HVAC hiss — far
    /// more than spectrally flat white noise, whose energy sits in the 0–4
    /// kHz speech bands the model is trained to protect (~2 dB attenuation
    /// there is inherent, not a bug). Mic recordings are colored noise, so
    /// this is the realistic test signal.
    struct PinkNoise {
        b: [f32; 7],
    }

    impl PinkNoise {
        fn next(&mut self, seed: &mut u32) -> f32 {
            let w = white(seed, 0.5);
            self.b[0] = 0.99886 * self.b[0] + w * 0.0555179;
            self.b[1] = 0.99332 * self.b[1] + w * 0.0750759;
            self.b[2] = 0.96900 * self.b[2] + w * 0.1538520;
            self.b[3] = 0.86650 * self.b[3] + w * 0.3104856;
            self.b[4] = 0.55000 * self.b[4] + w * 0.5329522;
            self.b[5] = -0.7616 * self.b[5] - w * 0.0168980;
            let p = self.b[0]
                + self.b[1]
                + self.b[2]
                + self.b[3]
                + self.b[4]
                + self.b[5]
                + self.b[6]
                + w * 0.5362;
            self.b[6] = w * 0.115926;
            // Normalize RMS to roughly the white-noise level the other tests
            // use, so the engines are compared on similarly loud input.
            p * (0.02 / 0.12)
        }
    }

    /// Energy ratio (processed / raw) of channel 0 over `blocks` blocks.
    fn energy_ratio(
        denoiser: &mut MicDenoiser,
        blocks: usize,
        mut sample: impl FnMut() -> f32,
    ) -> f64 {
        let mut raw = 0.0_f64;
        let mut clean = 0.0_f64;
        for _ in 0..blocks {
            let mut block = vec![0u8; FRAMES * STRIDE];
            for frame in block.as_chunks_mut::<STRIDE>().0 {
                let value = sample();
                frame[..4].copy_from_slice(&value.to_le_bytes());
                frame[4..8].copy_from_slice(&value.to_le_bytes());
            }
            let saved = block.clone();
            denoiser.process(&mut block);
            for (r, c) in saved.as_chunks::<8>().0.iter().zip(block.as_chunks::<8>().0) {
                raw += f64::from(f32::from_le_bytes(r[..4].try_into().unwrap())).powi(2);
                clean += f64::from(f32::from_le_bytes(c[..4].try_into().unwrap())).powi(2);
            }
        }
        clean / raw
    }

    #[test]
    fn rnnoise_quiets_pink_noise_across_blocks() {
        let mut denoiser = MicDenoiser::new(NoiseSuppressionMethod::Rnnoise, -30).unwrap();
        let mut seed = 7_u32;
        let mut pink = PinkNoise { b: [0.0; 7] };
        let ratio = energy_ratio(&mut denoiser, 50, || pink.next(&mut seed));
        assert!(
            ratio < 0.5,
            "rnnoise should suppress pink noise: ratio={ratio}"
        );
    }

    #[test]
    fn rnnoise_preserves_tonal_content() {
        let mut denoiser = MicDenoiser::new(NoiseSuppressionMethod::Rnnoise, -30).unwrap();
        // 440 Hz sine, well inside the speech band RNNoise is trained to keep.
        let step = std::f32::consts::TAU * 440.0 / crate::capture::audio::SAMPLE_RATE as f32;
        let mut phase = 0.0_f32;
        let ratio = energy_ratio(&mut denoiser, 50, || {
            let value = phase.sin() * 0.1;
            phase += step;
            value
        });
        assert!(ratio > 0.9, "rnnoise should keep tonal content: ratio={ratio}");
    }

    #[test]
    fn partial_trailing_block_passes_through() {
        let mut denoiser = MicDenoiser::new(NoiseSuppressionMethod::Rnnoise, -30).unwrap();
        let mut data = vec![1.5f32.to_le_bytes(), 2.5f32.to_le_bytes()].concat();
        denoiser.process(&mut data);
        assert_eq!(data, vec![1.5f32.to_le_bytes(), 2.5f32.to_le_bytes()].concat());
    }
}
