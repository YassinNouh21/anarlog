import { platform } from "@tauri-apps/plugin-os";
import type { LanguageModel } from "ai";

import { commands as detectCommands } from "@anlg/plugin-detect";
import { commands as permissionsCommands } from "@anlg/plugin-permissions";
import { toast } from "@anlg/ui/components/ui/toast";

import type { ScreenInfo } from "@anlg/plugin-detect";

import { getStoredSettingValues, setSettingValue } from "~/settings/queries";
import { resolveConfigValue } from "~/shared/config";
import { describeMeetingScreen } from "~/stt/meeting-screen-notes";
import { persistMeetingScreenCapture } from "~/stt/meeting-screen-records";

const MEETING_SCREEN_CAPTURE_INTERVAL_MS = 3_000;
const PICK_SCREEN_TOAST_ID = "meeting-screen-pick";

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
  let screenId: number | null = null;
  let screenAsked = false;
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

  // With more than one display, capture the one picked last time and ask
  // which display has this meeting.
  const askWhichScreen = async () => {
    screenAsked = true;
    const result = await detectCommands.listScreens();
    if (result.status === "error" || result.data.length < 2) {
      return;
    }
    const screens = result.data;
    const savedId = resolveConfigValue(
      "capture_screen_id",
      await getStoredSettingValues(),
    );
    const current =
      screens.find((screen) => screen.id === savedId) ??
      screens.find((screen) => screen.primary) ??
      screens[0];
    const other = screens.find((screen) => screen.id !== current.id)!;
    screenId = current.id;

    const label = (screen: ScreenInfo) =>
      screen.name || `Screen ${screens.indexOf(screen) + 1}`;
    const pick = (screen: ScreenInfo) => () => {
      if (screen.id !== screenId) {
        screenId = screen.id;
        resetSampler = true;
      }
      void setSettingValue("capture_screen_id", screen.id);
    };
    toast.info("Which screen has the meeting?", {
      id: PICK_SCREEN_TOAST_ID,
      description: `Capturing ${label(current)} until you pick.`,
      duration: Infinity,
      action: { label: label(current), onClick: pick(current) },
      secondaryAction: { label: label(other), onClick: pick(other) },
    });
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
      if (mode === "full" && !screenAsked) {
        await askWhichScreen();
      }
      const result =
        mode === "full"
          ? await detectCommands.captureScreen(resetSampler, screenId)
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
    toast.dismiss(PICK_SCREEN_TOAST_ID);
    if (timeout) {
      clearTimeout(timeout);
      timeout = null;
    }
    await pendingPersistence;
  };
}
