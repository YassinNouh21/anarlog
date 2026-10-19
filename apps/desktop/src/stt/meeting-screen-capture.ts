import { platform } from "@tauri-apps/plugin-os";
import type { LanguageModel } from "ai";

import { commands as detectCommands } from "@anlg/plugin-detect";
import { commands as permissionsCommands } from "@anlg/plugin-permissions";
import { toast } from "@anlg/ui/components/ui/toast";

import { getStoredSettingValues } from "~/settings/queries";
import { resolveConfigValue } from "~/shared/config";
import { describeMeetingScreen } from "~/stt/meeting-screen-notes";
import { persistMeetingScreenCapture } from "~/stt/meeting-screen-records";

const MEETING_SCREEN_CAPTURE_INTERVAL_MS = 3_000;

type CaptureMode = "full" | "shared" | null;

export function startMeetingScreenCapture({
  sessionId,
  isEnabled,
  visionModel,
  notesLanguage = "English",
}: {
  sessionId: string;
  isEnabled?: () => boolean | Promise<boolean>;
  /** Language the screen notes are written in. */
  notesLanguage?: string;
  /** When set, each new screen is described by this model (image input). */
  visionModel?: LanguageModel | null;
}) {
  let stopped = false;
  let resetSampler = true;
  let inFlight: Promise<void> | null = null;
  let pendingPersistence: Promise<void> | null = null;
  let timeout: ReturnType<typeof setTimeout> | null = null;
  let permissionWarned = false;
  const captureMode = async (): Promise<CaptureMode> => {
    if (isEnabled) {
      return (await isEnabled()) ? "shared" : null;
    }
    const settings = await getStoredSettingValues();
    if (resolveConfigValue("capture_full_screen", settings)) {
      return "full";
    }
    return resolveConfigValue("capture_shared_screens", settings)
      ? "shared"
      : null;
  };

  const captureOnce = async () => {
    try {
      const mode = await captureMode();
      if (!mode) {
        resetSampler = true;
        return;
      }

      const permission =
        platform() === "macos"
          ? await permissionsCommands.checkPermission("screenRecording")
          : null;
      if (permission?.status === "ok" && permission.data === "denied") {
        if (!permissionWarned) {
          permissionWarned = true;
          toast.warning(
            "Screen capture needs Screen Recording permission in Settings",
            { id: "meeting-screen-capture-warning", duration: Infinity },
          );
        }
        return;
      }

      if (stopped) {
        return;
      }
      const result =
        mode === "full"
          ? await detectCommands.captureScreen(resetSampler)
          : await detectCommands.captureMeetingScreenShare(resetSampler);
      resetSampler = false;
      if (stopped) {
        return;
      }
      if ((await captureMode()) !== mode || stopped) {
        resetSampler = true;
        return;
      }
      if (result.status === "error") {
        console.warn(
          "[listener] failed to capture shared screen",
          result.error,
        );
        return;
      }
      const jpeg = result.data.jpeg;
      if (!jpeg) {
        return;
      }

      const persistence = persistMeetingScreenCapture({
        sessionId,
        capture: result.data,
      });
      const settledPersistence = persistence.then(
        () => undefined,
        () => undefined,
      );
      pendingPersistence = settledPersistence;
      let record: Awaited<typeof persistence>;
      try {
        record = await persistence;
      } finally {
        if (pendingPersistence === settledPersistence) {
          pendingPersistence = null;
        }
      }
      if (record && mode === "full" && visionModel) {
        // Runs in the background so a slow model never delays the next capture.
        void describeMeetingScreen({
          sessionId,
          record,
          jpeg,
          model: visionModel,
          language: notesLanguage,
        }).catch((error) =>
          console.warn("[listener] failed to describe screen", error),
        );
      }
    } catch (error) {
      resetSampler = true;
      console.warn("[listener] failed to capture shared screen", error);
    }
  };

  const scheduleCapture = () => {
    if (stopped || timeout) {
      return;
    }
    timeout = setTimeout(() => {
      timeout = null;
      void capture();
    }, MEETING_SCREEN_CAPTURE_INTERVAL_MS);
  };

  const capture = () => {
    if (stopped || inFlight) {
      return;
    }
    const pendingCapture = captureOnce().finally(() => {
      if (inFlight === pendingCapture) {
        inFlight = null;
        scheduleCapture();
      }
    });
    inFlight = pendingCapture;
  };

  void capture();

  return async () => {
    stopped = true;
    if (timeout) {
      clearTimeout(timeout);
      timeout = null;
    }
    await pendingPersistence;
  };
}
