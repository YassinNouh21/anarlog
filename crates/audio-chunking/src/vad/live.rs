use std::time::Duration;

use anlg_vad::silero_onnx::CHUNK_SIZE_16KHZ;

use super::chunk_policy::{self, NormalizerState};
use super::session::{VadChunkerConfig, VadSession, VadTransition};
use crate::AudioChunk;

/// Speech chunker for live audio.
///
/// Feed 16 kHz mono samples with `push` as they arrive. It returns each speech
/// chunk when the chunk ends: at a pause, or at `max_chunk` during long speech.
/// Call `finish` at the end to get the speech that is still in progress.
pub struct LiveSpeechChunker {
    session: VadSession,
    normalizer: NormalizerState,
    // The VAD session needs whole frames; this holds the partial tail.
    pending_samples: Vec<f32>,
}

impl LiveSpeechChunker {
    pub fn new(
        redemption_time: Duration,
        target_chunk: Duration,
        max_chunk: Duration,
    ) -> Result<Self, crate::Error> {
        let config = VadChunkerConfig {
            target_chunk_duration: target_chunk,
            max_chunk_duration: max_chunk,
            ..VadChunkerConfig::speech(redemption_time)
        };
        Ok(Self {
            session: VadSession::new(config)?,
            normalizer: NormalizerState::new(redemption_time),
            pending_samples: Vec::with_capacity(CHUNK_SIZE_16KHZ),
        })
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<AudioChunk>, crate::Error> {
        self.pending_samples.extend_from_slice(samples);
        let whole = self.pending_samples.len() / CHUNK_SIZE_16KHZ * CHUNK_SIZE_16KHZ;
        let mut transitions = Vec::new();
        for frame in self.pending_samples[..whole].chunks_exact(CHUNK_SIZE_16KHZ) {
            transitions.extend(self.session.process(frame)?);
        }
        self.pending_samples.drain(..whole);
        Ok(self.normalize(transitions))
    }

    pub fn finish(&mut self) -> Result<Vec<AudioChunk>, crate::Error> {
        let tail = std::mem::take(&mut self.pending_samples);
        let transitions = self.session.finish(&tail)?;
        let mut chunks = self.normalize(transitions);
        chunks.extend(self.normalizer.take_pending());
        Ok(chunks)
    }

    fn normalize(&mut self, transitions: Vec<VadTransition>) -> Vec<AudioChunk> {
        let mut chunks = Vec::new();
        for chunk in chunk_policy::speech_chunks_from_transitions(transitions) {
            self.normalizer.push(chunk, &mut chunks);
        }
        chunks
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;

    fn decode_audio() -> Vec<f32> {
        rodio::Decoder::new(BufReader::new(
            std::fs::File::open(anlg_data::english_1::AUDIO_PATH).unwrap(),
        ))
        .unwrap()
        .collect()
    }

    #[test]
    fn emits_ordered_bounded_chunks_while_audio_arrives() {
        let audio = decode_audio();
        let max_chunk = Duration::from_secs(12);
        let mut chunker = LiveSpeechChunker::new(
            Duration::from_millis(600),
            Duration::from_secs(8),
            max_chunk,
        )
        .unwrap();

        let mut chunks = Vec::new();
        for frame in audio.chunks(1_600) {
            chunks.extend(chunker.push(frame).unwrap());
        }
        let emitted_before_finish = chunks.len();
        chunks.extend(chunker.finish().unwrap());

        assert!(emitted_before_finish > 0, "chunks arrive before the end");
        assert!(!chunks.is_empty());
        let max_samples = (max_chunk.as_secs_f64() * 16_000.0) as usize;
        for pair in chunks.windows(2) {
            assert!(pair[0].sample_start < pair[1].sample_start, "ordered");
        }
        for chunk in &chunks {
            assert_eq!(chunk.samples.len(), chunk.sample_end - chunk.sample_start);
            assert!(chunk.samples.len() <= max_samples, "bounded by max_chunk");
        }
    }

    #[test]
    fn rejects_max_chunk_not_above_target() {
        assert!(matches!(
            LiveSpeechChunker::new(
                Duration::from_millis(600),
                Duration::from_secs(8),
                Duration::from_secs(8)
            ),
            Err(crate::Error::InvalidConfig(_))
        ));
    }
}
