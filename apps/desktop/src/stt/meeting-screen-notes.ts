import { generateText, type LanguageModel } from "ai";

import { executeTransaction } from "~/db";
import { enqueueDatabaseWrite } from "~/db/write-queue";
import type { MeetingScreenRecord } from "~/stt/meeting-screen-records";

const screenNotePrompt = (language: string) =>
  `This is one screenshot of the user's screen during a meeting.
Write short notes about it for the meeting summary, in ${language}:
- Which app, page, or screen it is.
- The important text on screen (titles, labels, buttons, numbers). Quote it in its original language.
- UX / UI details worth noting: layout, flows, problems, or anything the team seems to discuss.
Use 2-5 plain bullet points in ${language}. No intro, no guesses.`;

/**
 * Describes one captured screen with a vision model and stores the result as
 * `description` on the screen record, so it can feed the meeting summary.
 */
export async function describeMeetingScreen({
  sessionId,
  record,
  jpeg,
  model,
  language,
}: {
  sessionId: string;
  record: MeetingScreenRecord;
  jpeg: number[];
  model: LanguageModel;
  /** Language name for the notes, e.g. "English" (the AI language setting). */
  language: string;
}): Promise<void> {
  const { text } = await generateText({
    model,
    maxOutputTokens: 400,
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: screenNotePrompt(language) },
          {
            type: "image",
            image: new Uint8Array(jpeg),
            mediaType: "image/jpeg",
          },
        ],
      },
    ],
  });

  const description = text.trim();
  if (!description) {
    return;
  }

  await enqueueDatabaseWrite(`session:${sessionId}`, () =>
    executeTransaction([
      {
        sql: `
          UPDATE session_documents
          SET body = json_set(body, '$.description', ?), updated_at = ?
          WHERE id = ? AND deleted_at IS NULL
        `,
        params: [description, new Date().toISOString(), record.id],
      },
    ]),
  );
}
