import { describe, expect, test, vi } from "vitest";

import { formatMeetingScreenContext } from "./meeting-screen-records";

vi.mock("~/db", () => ({
  executeTransaction: vi.fn(),
  liveQueryClient: { execute: vi.fn() },
  useLiveQuery: vi.fn(),
}));

vi.mock("~/db/write-queue", () => ({ enqueueDatabaseWrite: vi.fn() }));

vi.mock("~/session/attachments", () => ({
  catalogLocalNoteAttachment: vi.fn(),
  sha256Hex: vi.fn(),
}));

vi.mock("@anlg/plugin-fs-sync", () => ({ commands: {} }));

vi.mock("~/stt/meeting-chat-records", () => ({
  formatMeetingPlatform: (platform: string) => platform,
}));

const screen = {
  id: "s1",
  attachmentId: "a1",
  platform: null,
  width: 1920,
  height: 1080,
  capturedAt: "2026-10-19T10:30:00.000Z",
};

describe("formatMeetingScreenContext", () => {
  test("lists only screens the vision model described", () => {
    const context = formatMeetingScreenContext([
      { ...screen, description: "- Figma: New Content Guidance modal" },
      { ...screen, id: "s2", attachmentId: "a2" },
      { ...screen, id: "s3", description: "   " },
    ]);

    expect(context).toMatch(/^## Screens shown in the meeting\n### Screen at /);
    expect(context).toContain("- Figma: New Content Guidance modal");
    expect(context.match(/### Screen at/g)).toHaveLength(1);
  });

  test("is empty when no screen has a description", () => {
    expect(formatMeetingScreenContext([screen])).toBe("");
  });
});
