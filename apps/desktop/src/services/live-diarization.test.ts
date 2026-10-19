import { beforeEach, describe, expect, test, vi } from "vitest";

import { maybeDiarizeLiveTranscript } from "./live-diarization";

const { diarizeSessionTranscriptMock } = vi.hoisted(() => ({
  diarizeSessionTranscriptMock: vi.fn(),
}));

vi.mock("@anlg/plugin-transcription", () => ({
  commands: { diarizeSessionTranscript: diarizeSessionTranscriptMock },
}));

const input = {
  sessionId: "session-1",
  transcriptId: "transcript-1",
  audioPath: "/tmp/audio.mp3",
};

describe("maybeDiarizeLiveTranscript", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    diarizeSessionTranscriptMock.mockResolvedValue({ status: "ok", data: 3 });
  });

  test("diarizes OpenRouter live transcripts", async () => {
    await maybeDiarizeLiveTranscript({ ...input, provider: "openrouter" });

    expect(diarizeSessionTranscriptMock).toHaveBeenCalledWith(
      "session-1",
      "transcript-1",
      "/tmp/audio.mp3",
      null,
    );
  });

  test("skips providers that label speakers themselves, and missing audio", async () => {
    await maybeDiarizeLiveTranscript({ ...input, provider: "deepgram" });
    await maybeDiarizeLiveTranscript({
      ...input,
      provider: "openrouter",
      audioPath: null,
    });

    expect(diarizeSessionTranscriptMock).not.toHaveBeenCalled();
  });

  test("never throws when diarization fails", async () => {
    diarizeSessionTranscriptMock.mockRejectedValue(new Error("boom"));

    await expect(
      maybeDiarizeLiveTranscript({ ...input, provider: "openrouter" }),
    ).resolves.toBeUndefined();
  });
});
