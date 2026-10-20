use anlg_audio_utils::Source;
use anlg_transcript::{
    StoredSpeakerHint, parse_stored_speaker_hints, parse_stored_transcript_words,
};
use owhisper_interface::batch;
use tauri::Manager;

use crate::voiceprint::{
    known_speakers_for_session, load_transcript_words_and_hints, write_speaker_assignment_hints,
};

const PROVIDER_SPEAKER_HINT: &str = "provider_speaker_index";

/// Labels who spoke each part of a transcript that was made live (for example
/// from OpenRouter chunks), with the local diarizer on the session recording.
/// Writes `provider_speaker_index` hints and returns how many words got a
/// speaker. Fully local; no API key.
#[tauri::command]
#[specta::specta]
pub async fn diarize_session_transcript<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
    transcript_id: String,
    audio_path: String,
    speaker_count: Option<u32>,
) -> Result<u32, String> {
    let pool = app
        .try_state::<tauri_plugin_db::ManagedState>()
        .map(|state| state.pool().clone())
        .ok_or_else(|| "database is not ready yet".to_string())?;
    let Some((words_json, hints_json)) =
        load_transcript_words_and_hints(&pool, &session_id, &transcript_id).await?
    else {
        return Ok(0);
    };

    let words = parse_stored_transcript_words(&words_json);
    let recording_channels = anlg_audio_utils::source_from_path(&audio_path)
        .map(|source| u16::from(source.channels()))
        .map_err(|error| error.to_string())?;
    let recording_channels = recording_channels.min(2);
    let (response, word_ids) = transcript_response(&words, usize::from(recording_channels));
    if word_ids.iter().all(Vec::is_empty) {
        return Ok(0);
    }

    // The diarizer plans each transcript channel against the recorded
    // channels (mic and system audio), so it needs the real count.
    let listen_params = owhisper_interface::ListenParams {
        channels: recording_channels as u8,
        num_speakers: speaker_count,
        ..Default::default()
    };
    let known_speakers = known_speakers_for_session(&app, &session_id).await;
    let labeled = anlg_transcription_core::listener2::diarize_transcript(
        audio_path,
        listen_params,
        known_speakers,
        response,
    )
    .await;

    let mut hints: Vec<StoredSpeakerHint> = parse_stored_speaker_hints(&hints_json)
        .into_iter()
        .filter(|hint| hint.hint_type != PROVIDER_SPEAKER_HINT)
        .collect();
    let mut count = 0;
    for (channel, ids) in labeled.results.channels.iter().zip(&word_ids) {
        let Some(alternative) = channel.alternatives.first() else {
            continue;
        };
        for (word, id) in alternative.words.iter().zip(ids) {
            let Some(speaker) = word.speaker else {
                continue;
            };
            hints.push(StoredSpeakerHint {
                id: format!("{id}:{PROVIDER_SPEAKER_HINT}"),
                word_id: Some(id.clone()),
                hint_type: PROVIDER_SPEAKER_HINT.to_string(),
                value: serde_json::json!({ "channel": word.channel, "speaker_index": speaker }),
            });
            count += 1;
        }
    }
    if count == 0 {
        return Ok(0);
    }

    let next_json = serde_json::to_string(&hints).map_err(|error| error.to_string())?;
    let written = write_speaker_assignment_hints(
        &pool,
        &session_id,
        &transcript_id,
        &words_json,
        &hints_json,
        &next_json,
    )
    .await?;
    // The transcript changed while we were working; keep the newer version.
    Ok(if written { count } else { 0 })
}

/// The stored words as a batch response the diarizer understands: one
/// transcript channel per recorded channel (mic = 0, system audio = 1), plus
/// the word ids in the same order.
fn transcript_response(
    words: &[anlg_transcript::StoredTranscriptWord],
    recording_channels: usize,
) -> (batch::Response, Vec<Vec<String>>) {
    let channel_count = recording_channels.max(1);
    let mut channels: Vec<Vec<batch::Word>> = vec![Vec::new(); channel_count];
    let mut word_ids: Vec<Vec<String>> = vec![Vec::new(); channel_count];
    for word in words {
        let (Some(text), Some(start_ms), Some(end_ms)) =
            (word.text.as_ref(), word.start_ms, word.end_ms)
        else {
            continue;
        };
        let channel = word.channel.unwrap_or(0.0) as usize;
        if channel >= channel_count {
            continue;
        }
        channels[channel].push(batch::Word {
            word: text.clone(),
            start: start_ms / 1000.0,
            end: end_ms / 1000.0,
            confidence: 1.0,
            channel: channel as i32,
            speaker: None,
            punctuated_word: None,
        });
        word_ids[channel].push(word.id.clone());
    }

    let response = batch::Response {
        metadata: serde_json::json!({}),
        results: batch::Results {
            channels: channels
                .into_iter()
                .map(|words| batch::Channel {
                    alternatives: vec![batch::Alternatives {
                        transcript: String::new(),
                        confidence: 1.0,
                        words,
                    }],
                })
                .collect(),
        },
    };
    (response, word_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(id: &str, channel: f64, start_ms: f64) -> anlg_transcript::StoredTranscriptWord {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "text": "hello",
            "start_ms": start_ms,
            "end_ms": start_ms + 500.0,
            "channel": channel,
        }))
        .unwrap()
    }

    #[test]
    fn splits_words_by_recorded_channel() {
        let words = [
            word("a", 0.0, 0.0),
            word("b", 1.0, 1000.0),
            word("c", 1.0, 2000.0),
        ];
        let (response, ids) = transcript_response(&words, 2);
        assert_eq!(response.results.channels.len(), 2);
        assert_eq!(
            ids,
            vec![
                vec!["a".to_string()],
                vec!["b".to_string(), "c".to_string()]
            ]
        );
        let remote = &response.results.channels[1].alternatives[0].words;
        assert_eq!((remote[0].start, remote[0].end), (1.0, 1.5));
    }

    #[test]
    fn mono_recording_keeps_only_channel_zero() {
        let words = [word("a", 0.0, 0.0), word("b", 1.0, 1000.0)];
        let (response, ids) = transcript_response(&words, 1);
        assert_eq!(response.results.channels.len(), 1);
        assert_eq!(ids, vec![vec!["a".to_string()]]);
    }
}
