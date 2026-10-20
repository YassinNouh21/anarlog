//! Live transcription for providers that only have a batch API (for example
//! OpenRouter).
//!
//! Each audio channel is cut into speech chunks at pauses. Every chunk is sent
//! to the provider's batch API as a short WAV file, and the text comes back as
//! final transcript segments, always in chunk order.

use std::time::Duration;

use anlg_audio_chunking::{AudioChunk, LiveSpeechChunker};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use owhisper_interface::stream::StreamResponse;
use owhisper_interface::{ControlMessage, ListenParams, MixedMessage};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};

use super::parsing::WordBuilder;
use super::{BatchSttAdapter, segment};
use crate::local_soniqo_live::i16_bytes_to_f32;
use crate::{BatchClient, FinalizeHandle, ListenClientDualInput, ListenClientInput};

const SAMPLE_RATE: u32 = 16_000;
const REDEMPTION_TIME: Duration = Duration::from_millis(600);
const TARGET_CHUNK: Duration = Duration::from_secs(8);
const MAX_CHUNK: Duration = Duration::from_secs(12);
/// Chunks uploaded at the same time per channel. Results still come out in order.
const PARALLEL_UPLOADS: usize = 3;
const UPLOAD_ATTEMPTS: u32 = 3;

pub type ChunkedLiveStream = ReceiverStream<Result<StreamResponse, crate::Error>>;

pub struct ChunkedLiveClient<A: BatchSttAdapter> {
    batch: BatchClient<A>,
    allowed_scripts: Vec<Script>,
}

impl<A: BatchSttAdapter> ChunkedLiveClient<A> {
    pub fn new(api_base: String, api_key: String, params: ListenParams) -> Self {
        Self {
            allowed_scripts: allowed_scripts(&params.languages),
            batch: BatchClient::new(api_base, api_key, params),
        }
    }

    pub fn from_realtime_audio_single(
        self,
        stream: impl Stream<Item = ListenClientInput> + Send + Unpin + 'static,
    ) -> (ChunkedLiveStream, ChunkedLiveHandle) {
        self.start(
            1,
            stream.map(|message| match message {
                MixedMessage::Audio(audio) => MixedMessage::Audio(vec![audio]),
                MixedMessage::Control(control) => MixedMessage::Control(control),
            }),
        )
    }

    pub fn from_realtime_audio_dual(
        self,
        stream: impl Stream<Item = ListenClientDualInput> + Send + Unpin + 'static,
    ) -> (ChunkedLiveStream, ChunkedLiveHandle) {
        self.start(
            2,
            stream.map(|message| match message {
                MixedMessage::Audio((mic, speaker)) => MixedMessage::Audio(vec![mic, speaker]),
                MixedMessage::Control(control) => MixedMessage::Control(control),
            }),
        )
    }

    fn start(
        self,
        channels: usize,
        stream: impl Stream<Item = MixedMessage<Vec<Bytes>, ControlMessage>> + Send + Unpin + 'static,
    ) -> (ChunkedLiveStream, ChunkedLiveHandle) {
        let (response_tx, response_rx) = mpsc::channel(32);
        let (finalize_tx, finalize_rx) = mpsc::channel(1);

        let mut audio_txs = Vec::with_capacity(channels);
        for channel in 0..channels {
            let (audio_tx, audio_rx) = mpsc::unbounded_channel();
            let (chunk_tx, chunk_rx) = mpsc::unbounded_channel();
            audio_txs.push(audio_tx);
            // The VAD runs ONNX inference, so it gets its own blocking thread.
            tokio::task::spawn_blocking(move || chunk_audio(audio_rx, chunk_tx));
            tokio::spawn(upload_chunks(
                self.batch.clone(),
                self.allowed_scripts.clone(),
                channel,
                channels,
                chunk_rx,
                response_tx.clone(),
            ));
        }
        tokio::spawn(forward_audio(stream, audio_txs, finalize_rx));

        (
            ReceiverStream::new(response_rx),
            ChunkedLiveHandle {
                finalize_tx,
                expected_finalize_count: channels,
            },
        )
    }
}

pub struct ChunkedLiveHandle {
    finalize_tx: mpsc::Sender<()>,
    expected_finalize_count: usize,
}

impl FinalizeHandle for ChunkedLiveHandle {
    async fn finalize(&self) {
        let _ = self.finalize_tx.send(()).await;
    }

    fn expected_finalize_count(&self) -> usize {
        self.expected_finalize_count
    }
}

/// Passes audio to the chunkers without waiting, so the recorder is never
/// slowed down by uploads. Dropping the senders ends the chunkers.
async fn forward_audio(
    mut stream: impl Stream<Item = MixedMessage<Vec<Bytes>, ControlMessage>> + Send + Unpin,
    audio_txs: Vec<mpsc::UnboundedSender<Vec<f32>>>,
    mut finalize_rx: mpsc::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = finalize_rx.recv() => return,
            message = stream.next() => match message {
                Some(MixedMessage::Audio(channels)) => {
                    for (audio_tx, audio) in audio_txs.iter().zip(channels) {
                        let _ = audio_tx.send(i16_bytes_to_f32(&audio));
                    }
                }
                Some(MixedMessage::Control(ControlMessage::KeepAlive)) => {}
                Some(MixedMessage::Control(
                    ControlMessage::Finalize | ControlMessage::CloseStream,
                ))
                | None => return,
            },
        }
    }
}

fn chunk_audio(
    mut audio_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    chunk_tx: mpsc::UnboundedSender<Result<AudioChunk, String>>,
) {
    let mut chunker = match LiveSpeechChunker::new(REDEMPTION_TIME, TARGET_CHUNK, MAX_CHUNK) {
        Ok(chunker) => chunker,
        Err(error) => {
            let _ = chunk_tx.send(Err(format!("speech_chunker_failed: {error}")));
            return;
        }
    };

    while let Some(samples) = audio_rx.blocking_recv() {
        match chunker.push(&samples) {
            Ok(chunks) => {
                for chunk in chunks {
                    if chunk_tx.send(Ok(chunk)).is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = chunk_tx.send(Err(format!("speech_chunker_failed: {error}")));
                return;
            }
        }
    }

    match chunker.finish() {
        Ok(chunks) => chunks.into_iter().for_each(|chunk| {
            let _ = chunk_tx.send(Ok(chunk));
        }),
        Err(error) => {
            let _ = chunk_tx.send(Err(format!("speech_chunker_failed: {error}")));
        }
    }
}

async fn upload_chunks<A: BatchSttAdapter>(
    batch: BatchClient<A>,
    allowed_scripts: Vec<Script>,
    channel: usize,
    channels: usize,
    chunk_rx: mpsc::UnboundedReceiver<Result<AudioChunk, String>>,
    response_tx: mpsc::Sender<Result<StreamResponse, crate::Error>>,
) {
    let provider = A::default().provider_name();
    let mut results = UnboundedReceiverStream::new(chunk_rx)
        .map(|chunk| {
            let batch = batch.clone();
            let allowed_scripts = allowed_scripts.clone();
            async move { transcribe_chunk(&batch, &allowed_scripts, chunk?).await }
        })
        .buffered(PARALLEL_UPLOADS);

    let mut end = 0.0;
    while let Some(result) = results.next().await {
        let response = match result {
            Ok(segment) if segment.text.is_empty() => {
                end = segment.end;
                continue;
            }
            Ok(segment) => {
                end = segment.end;
                let mut response =
                    segment::response(&segment.text, segment.start, segment.end, true, None);
                if !segment.words.is_empty()
                    && let StreamResponse::TranscriptResponse { channel, .. } = &mut response
                {
                    channel.alternatives[0].words = segment.words;
                }
                response
            }
            Err(message) => StreamResponse::ErrorResponse {
                error_code: None,
                error_message: message,
                provider: provider.into(),
            },
        };
        if send(&response_tx, response, channel, channels)
            .await
            .is_err()
        {
            return;
        }
    }

    let _ = send(&response_tx, segment::finished(end), channel, channels).await;
}

async fn send(
    response_tx: &mpsc::Sender<Result<StreamResponse, crate::Error>>,
    mut response: StreamResponse,
    channel: usize,
    channels: usize,
) -> Result<(), ()> {
    response.set_channel_index(channel as i32, channels as i32);
    response_tx.send(Ok(response)).await.map_err(|_| ())
}

struct ChunkText {
    text: String,
    start: f64,
    end: f64,
    /// Word timings on the session clock, when the model returns them. Empty
    /// means the chunk stays one timed segment.
    words: Vec<owhisper_interface::stream::Word>,
}

/// The words of a chunk response, moved onto the session clock. Provider
/// speaker labels are dropped: they restart in every chunk.
fn session_words(
    response: &owhisper_interface::batch::Response,
    offset: f64,
) -> Vec<owhisper_interface::stream::Word> {
    response
        .results
        .channels
        .iter()
        .filter_map(|channel| channel.alternatives.first())
        .flat_map(|alternative| &alternative.words)
        .map(|word| {
            WordBuilder::new(word.punctuated_word.as_deref().unwrap_or(&word.word))
                .start(offset + word.start)
                .end(offset + word.end)
                .confidence(word.confidence)
                .build()
        })
        .collect()
}

async fn transcribe_chunk<A: BatchSttAdapter>(
    batch: &BatchClient<A>,
    allowed_scripts: &[Script],
    chunk: AudioChunk,
) -> Result<ChunkText, String> {
    let start = chunk.sample_start as f64 / SAMPLE_RATE as f64;
    let end = chunk.sample_end as f64 / SAMPLE_RATE as f64;
    let wav = write_wav(&chunk.samples).map_err(|error| format!("chunk_wav_failed: {error}"))?;

    let mut last_error = String::new();
    let mut script_retried = false;
    for attempt in 0..UPLOAD_ATTEMPTS {
        match batch.transcribe_file(wav.path()).await {
            Ok(response) => {
                let text = response
                    .results
                    .channels
                    .iter()
                    .filter_map(|channel| channel.alternatives.first())
                    .map(|alternative| alternative.transcript.trim())
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                // Short chunks sometimes come back in a language nobody spoke.
                // Try once more, then drop the chunk.
                if has_unexpected_script(&text, allowed_scripts) {
                    if !script_retried {
                        script_retried = true;
                        tracing::warn!(chunk.start = start, "chunk_unexpected_script_retry");
                        continue;
                    }
                    tracing::warn!(chunk.start = start, "chunk_unexpected_script_dropped");
                    return Ok(ChunkText {
                        text: String::new(),
                        start,
                        end,
                        words: Vec::new(),
                    });
                }
                let words = session_words(&response, start);
                return Ok(ChunkText {
                    text,
                    start,
                    end,
                    words,
                });
            }
            Err(error) => {
                last_error = error.to_string();
                tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
            }
        }
    }
    Err(format!("chunk_transcription_failed: {last_error}"))
}

/// Writing systems we can detect. Characters of other scripts are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Arabic,
    Cyrillic,
    Greek,
    Hebrew,
    Devanagari,
    Thai,
    Hangul,
    Cjk,
}

fn script_of(c: char) -> Option<Script> {
    match c as u32 {
        0x0041..=0x024F if c.is_alphabetic() => Some(Script::Latin),
        0x0370..=0x03FF => Some(Script::Greek),
        0x0400..=0x052F => Some(Script::Cyrillic),
        0x0590..=0x05FF => Some(Script::Hebrew),
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => {
            Some(Script::Arabic)
        }
        0x0900..=0x097F => Some(Script::Devanagari),
        0x0E00..=0x0E7F => Some(Script::Thai),
        0x1100..=0x11FF | 0xAC00..=0xD7AF => Some(Script::Hangul),
        0x3040..=0x30FF | 0x4E00..=0x9FFF => Some(Script::Cjk),
        _ => None,
    }
}

/// Scripts of the spoken languages, plus Latin (English words are common in
/// any meeting). Empty means no filter.
fn allowed_scripts(languages: &[anlg_language::Language]) -> Vec<Script> {
    if languages.is_empty() {
        return Vec::new();
    }
    let mut scripts = vec![Script::Latin];
    for language in languages {
        let script = match language.iso639().code() {
            "ar" | "fa" | "ur" | "ps" => Script::Arabic,
            "ru" | "uk" | "bg" | "sr" | "be" | "kk" | "mk" => Script::Cyrillic,
            "el" => Script::Greek,
            "he" | "yi" => Script::Hebrew,
            "hi" | "mr" | "ne" => Script::Devanagari,
            "th" => Script::Thai,
            "ko" => Script::Hangul,
            "zh" | "ja" => Script::Cjk,
            _ => Script::Latin,
        };
        if !scripts.contains(&script) {
            scripts.push(script);
        }
    }
    scripts
}

fn has_unexpected_script(text: &str, allowed: &[Script]) -> bool {
    !allowed.is_empty()
        && text
            .chars()
            .filter_map(script_of)
            .any(|script| !allowed.contains(&script))
}

fn write_wav(samples: &[f32]) -> Result<tempfile::NamedTempFile, Box<dyn std::error::Error>> {
    let file = tempfile::Builder::new().suffix(".wav").tempfile()?;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(file.path(), spec)?;
    for sample in samples {
        writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_scripts_of_languages_nobody_spoke() {
        use anlg_language::ISO639;

        let arabic: Vec<anlg_language::Language> = vec![ISO639::Ar.into()];
        let allowed = allowed_scripts(&arabic);
        assert!(has_unexpected_script("Покажи.", &allowed));
        assert!(has_unexpected_script("你好", &allowed));
        assert!(!has_unexpected_script(
            "إيه الكلام. Okay, check it.",
            &allowed
        ));
        assert!(!has_unexpected_script("123 ...", &allowed));

        let russian: Vec<anlg_language::Language> = vec![ISO639::Ru.into()];
        assert!(!has_unexpected_script(
            "Покажи.",
            &allowed_scripts(&russian)
        ));
        assert!(!has_unexpected_script("Покажи.", &allowed_scripts(&[])));
    }

    #[test]
    fn moves_chunk_words_onto_the_session_clock() {
        let response: owhisper_interface::batch::Response = serde_json::from_value(serde_json::json!({
            "metadata": {},
            "results": { "channels": [{ "alternatives": [{
                "transcript": "hello there",
                "confidence": 1.0,
                "words": [
                    { "word": "hello", "start": 0.1, "end": 0.4, "confidence": 0.9, "channel": 0, "speaker": 3, "punctuated_word": null },
                    { "word": "there", "start": 0.5, "end": 0.8, "confidence": 0.8, "channel": 0, "speaker": 4, "punctuated_word": "there." }
                ]
            }]}]}
        }))
        .unwrap();

        let words = session_words(&response, 10.0);
        let texts: Vec<_> = words.iter().map(|word| word.word.as_str()).collect();
        assert_eq!(texts, ["hello", "there."]);
        assert_eq!((words[0].start, words[1].end), (10.1, 10.8));
        assert!(words.iter().all(|word| word.speaker.is_none()));
    }

    #[test]
    fn writes_16khz_mono_wav() {
        let wav = write_wav(&[0.0, 0.5, -0.5, 1.0]).unwrap();
        let reader = hound::WavReader::open(wav.path()).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.len(), 4);
    }
}
