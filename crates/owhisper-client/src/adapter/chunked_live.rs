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
}

impl<A: BatchSttAdapter> ChunkedLiveClient<A> {
    pub fn new(api_base: String, api_key: String, params: ListenParams) -> Self {
        Self {
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
    channel: usize,
    channels: usize,
    chunk_rx: mpsc::UnboundedReceiver<Result<AudioChunk, String>>,
    response_tx: mpsc::Sender<Result<StreamResponse, crate::Error>>,
) {
    let provider = A::default().provider_name();
    let mut results = UnboundedReceiverStream::new(chunk_rx)
        .map(|chunk| {
            let batch = batch.clone();
            async move { transcribe_chunk(&batch, chunk?).await }
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
                segment::response(&segment.text, segment.start, segment.end, true, None)
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
}

async fn transcribe_chunk<A: BatchSttAdapter>(
    batch: &BatchClient<A>,
    chunk: AudioChunk,
) -> Result<ChunkText, String> {
    let start = chunk.sample_start as f64 / SAMPLE_RATE as f64;
    let end = chunk.sample_end as f64 / SAMPLE_RATE as f64;
    let wav = write_wav(&chunk.samples).map_err(|error| format!("chunk_wav_failed: {error}"))?;

    let mut last_error = String::new();
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
                return Ok(ChunkText { text, start, end });
            }
            Err(error) => {
                last_error = error.to_string();
                tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
            }
        }
    }
    Err(format!("chunk_transcription_failed: {last_error}"))
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
    fn writes_16khz_mono_wav() {
        let wav = write_wav(&[0.0, 0.5, -0.5, 1.0]).unwrap();
        let reader = hound::WavReader::open(wav.path()).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.len(), 4);
    }
}
