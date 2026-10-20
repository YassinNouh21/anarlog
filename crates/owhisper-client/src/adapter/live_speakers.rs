//! Speaker labels that stay the same across live chunks.
//!
//! The provider labels speakers inside one chunk only, and its numbers restart
//! in every chunk. For each chunk speaker we compute a voice embedding of their
//! words and match it to the speakers heard so far in the session.

use anlg_embedding::EmbeddingExtractor;
use owhisper_interface::stream::Word;

const SAMPLE_RATE: f64 = 16_000.0;
/// Cosine similarity from which two voices count as the same speaker. Tuned
/// on a recorded standup: 0.35-0.4 found the true speaker count with 92-96%
/// of words on the right speaker; 0.5 split speakers in two.
const SAME_SPEAKER_SIMILARITY: f32 = 0.4;
/// Less speech than this gives a noisy embedding, so it joins the closest
/// known speaker instead of starting a new one, and does not move that
/// speaker's voice.
const MIN_RELIABLE_SECONDS: f64 = 1.5;

pub(crate) struct SpeakerTracker {
    extractor: Option<EmbeddingExtractor>,
    /// Sum of the unit embeddings heard for each session speaker.
    voices: Vec<Vec<f32>>,
    last: Option<usize>,
}

impl SpeakerTracker {
    pub(crate) fn new() -> Self {
        let extractor = EmbeddingExtractor::new()
            .inspect_err(|error| tracing::warn!(%error, "live_speaker_embedding_unavailable"))
            .ok();
        Self {
            extractor,
            voices: Vec::new(),
            last: None,
        }
    }

    /// Replaces the chunk speaker on each word (`speaker`, from the provider)
    /// with a session speaker. `samples` is the chunk audio and `offset` its
    /// start on the session clock.
    pub(crate) fn label(&mut self, samples: &[f32], offset: f64, words: &mut [Word]) {
        let mut chunk_speakers: Vec<Option<i32>> = words.iter().map(|word| word.speaker).collect();
        chunk_speakers.sort();
        chunk_speakers.dedup();

        for chunk_speaker in chunk_speakers {
            let audio: Vec<f32> = words
                .iter()
                .filter(|word| word.speaker == chunk_speaker)
                .flat_map(|word| {
                    let start = sample_index(word.start - offset, samples.len());
                    let end = sample_index(word.end - offset, samples.len());
                    &samples[start..end.max(start)]
                })
                .copied()
                .collect();
            let session_speaker = self.match_voice(&audio);
            for word in words
                .iter_mut()
                .filter(|word| word.speaker == chunk_speaker)
            {
                word.speaker = session_speaker.map(|speaker| speaker as i32);
            }
        }
    }

    fn match_voice(&mut self, audio: &[f32]) -> Option<usize> {
        let reliable = audio.len() as f64 / SAMPLE_RATE >= MIN_RELIABLE_SECONDS;
        let embedding = self
            .extractor
            .as_mut()
            .filter(|_| !audio.is_empty())
            .and_then(|extractor| extractor.compute_optional(audio).ok().flatten())
            .map(|embedding| unit(&embedding));
        let Some(embedding) = embedding else {
            return self.last.or_else(|| self.add_voice(None));
        };

        let closest = self
            .voices
            .iter()
            .map(|voice| dot(&unit(voice), &embedding))
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let speaker = match closest {
            Some((speaker, similarity)) if similarity >= SAME_SPEAKER_SIMILARITY || !reliable => {
                if reliable {
                    for (sum, value) in self.voices[speaker].iter_mut().zip(&embedding) {
                        *sum += value;
                    }
                }
                speaker
            }
            _ => self.add_voice(Some(embedding))?,
        };
        self.last = Some(speaker);
        Some(speaker)
    }

    fn add_voice(&mut self, embedding: Option<Vec<f32>>) -> Option<usize> {
        let embedding = embedding?;
        self.voices.push(embedding);
        let speaker = self.voices.len() - 1;
        self.last = Some(speaker);
        Some(speaker)
    }
}

fn sample_index(seconds: f64, len: usize) -> usize {
    ((seconds.max(0.0) * SAMPLE_RATE) as usize).min(len)
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn unit(vector: &[f32]) -> Vec<f32> {
    let norm = dot(vector, vector).sqrt();
    if norm == 0.0 {
        return vector.to_vec();
    }
    vector.iter().map(|value| value / norm).collect()
}
