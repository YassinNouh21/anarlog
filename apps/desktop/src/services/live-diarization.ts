import { commands as transcriptionCommands } from "@anlg/plugin-transcription";

// Providers whose live transcript has no speaker labels of its own.
const LIVE_PROVIDERS_WITHOUT_SPEAKERS = new Set(["openrouter"]);

/**
 * Labels speakers on a live transcript after the recording stops, with the
 * local diarizer. Runs before the summary, so the summary sees the speakers.
 */
export async function maybeDiarizeLiveTranscript(input: {
  provider: string | null | undefined;
  sessionId: string;
  transcriptId: string;
  audioPath: string | null | undefined;
}): Promise<void> {
  if (
    !input.audioPath ||
    !LIVE_PROVIDERS_WITHOUT_SPEAKERS.has(input.provider ?? "")
  ) {
    return;
  }

  try {
    const result = await transcriptionCommands.diarizeSessionTranscript(
      input.sessionId,
      input.transcriptId,
      input.audioPath,
      null,
    );
    if (result.status === "error") {
      console.warn(
        "[diarization] live transcript diarization failed",
        result.error,
      );
    }
  } catch (error) {
    console.warn("[diarization] live transcript diarization failed", error);
  }
}
