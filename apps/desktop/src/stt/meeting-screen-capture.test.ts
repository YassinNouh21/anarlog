import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import { startMeetingScreenCapture } from "./meeting-screen-capture";

const {
  captureMeetingScreenShareMock,
  captureScreenMock,
  checkPermissionMock,
  describeMeetingScreenMock,
  listScreensMock,
  persistMeetingScreenCaptureMock,
  platformMock,
  setSettingValueMock,
  settingsMock,
  toastInfoMock,
} = vi.hoisted(() => ({
  captureMeetingScreenShareMock: vi.fn(),
  captureScreenMock: vi.fn(),
  checkPermissionMock: vi.fn(),
  describeMeetingScreenMock: vi.fn(),
  listScreensMock: vi.fn(),
  persistMeetingScreenCaptureMock: vi.fn(),
  platformMock: vi.fn(),
  setSettingValueMock: vi.fn(),
  settingsMock: vi.fn(),
  toastInfoMock: vi.fn(),
}));

vi.mock("@anlg/plugin-detect", () => ({
  commands: {
    captureMeetingScreenShare: captureMeetingScreenShareMock,
    captureScreen: captureScreenMock,
    listScreens: listScreensMock,
  },
}));

vi.mock("~/settings/queries", () => ({
  getStoredSettingValues: async () => settingsMock(),
  setSettingValue: setSettingValueMock,
}));

vi.mock("~/shared/config", () => ({
  resolveConfigValue: (key: string, values: Record<string, unknown>) =>
    values[key] ?? false,
}));

vi.mock("~/stt/meeting-screen-notes", () => ({
  describeMeetingScreen: describeMeetingScreenMock,
}));

vi.mock("@anlg/plugin-permissions", () => ({
  commands: { checkPermission: checkPermissionMock },
}));

vi.mock("@tauri-apps/plugin-os", () => ({ platform: platformMock }));

vi.mock("~/stt/meeting-screen-records", () => ({
  persistMeetingScreenCapture: persistMeetingScreenCaptureMock,
}));

vi.mock("@anlg/ui/components/ui/toast", () => ({
  toast: { warning: vi.fn(), info: toastInfoMock, dismiss: vi.fn() },
}));

const frame = {
  sharing: true,
  app: { id: "us.zoom.xos", name: "Zoom" },
  platform: "zoom" as const,
  jpeg: [0xff, 0xd8],
  width: 1920,
  height: 1080,
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

describe("startMeetingScreenCapture", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    platformMock.mockReturnValue("macos");
    checkPermissionMock.mockResolvedValue({ status: "ok", data: "authorized" });
    persistMeetingScreenCaptureMock.mockResolvedValue(undefined);
    listScreensMock.mockResolvedValue({
      status: "ok",
      data: [{ id: 1, name: "Built-in", primary: true }],
    });
    describeMeetingScreenMock.mockResolvedValue(undefined);
    settingsMock.mockReturnValue({});
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  test("persists only frames the sampler kept", async () => {
    captureMeetingScreenShareMock
      .mockResolvedValueOnce({ status: "ok", data: { ...frame, jpeg: null } })
      .mockResolvedValueOnce({ status: "ok", data: frame })
      .mockResolvedValue({ status: "ok", data: { ...frame, jpeg: null } });

    const stop = startMeetingScreenCapture({
      sessionId: "session-1",
      isEnabled: () => true,
    });
    await vi.advanceTimersByTimeAsync(6_000);
    await stop();

    expect(persistMeetingScreenCaptureMock).toHaveBeenCalledTimes(1);
    expect(persistMeetingScreenCaptureMock).toHaveBeenCalledWith({
      sessionId: "session-1",
      capture: frame,
    });
  });

  test("never captures on macOS when Screen Recording is denied", async () => {
    checkPermissionMock.mockResolvedValue({ status: "ok", data: "denied" });

    const stop = startMeetingScreenCapture({
      sessionId: "session-1",
      isEnabled: () => true,
    });
    await vi.advanceTimersByTimeAsync(3_000);
    await stop();

    expect(captureMeetingScreenShareMock).not.toHaveBeenCalled();
  });

  test("captures on Windows, where there is no Screen Recording permission", async () => {
    platformMock.mockReturnValue("windows");
    checkPermissionMock.mockResolvedValue({ status: "ok", data: "denied" });
    captureMeetingScreenShareMock.mockResolvedValue({
      status: "ok",
      data: frame,
    });

    const stop = startMeetingScreenCapture({
      sessionId: "session-1",
      isEnabled: () => true,
    });
    await vi.advanceTimersByTimeAsync(0);
    await stop();

    expect(persistMeetingScreenCaptureMock).toHaveBeenCalledTimes(1);
  });

  test("stops without waiting for a slow accessibility walk and drops its frame", async () => {
    const pending = deferred<unknown>();
    captureMeetingScreenShareMock.mockReturnValue(pending.promise);

    const stop = startMeetingScreenCapture({
      sessionId: "session-1",
      isEnabled: () => true,
    });
    await vi.advanceTimersByTimeAsync(0);
    await stop();

    pending.resolve({ status: "ok", data: frame });
    await vi.advanceTimersByTimeAsync(0);

    expect(persistMeetingScreenCaptureMock).not.toHaveBeenCalled();
  });

  test("waits for an in-flight frame write when capture stops", async () => {
    const write = deferred<undefined>();
    captureMeetingScreenShareMock.mockResolvedValue({
      status: "ok",
      data: frame,
    });
    persistMeetingScreenCaptureMock.mockReturnValue(write.promise);

    const stop = startMeetingScreenCapture({
      sessionId: "session-1",
      isEnabled: () => true,
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(persistMeetingScreenCaptureMock).toHaveBeenCalledTimes(1);
    let stopped = false;
    const stopping = stop().then(() => {
      stopped = true;
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(stopped).toBe(false);

    write.resolve(undefined);
    await stopping;
    expect(stopped).toBe(true);
  });

  test("full-screen mode captures the display and describes kept frames", async () => {
    settingsMock.mockReturnValue({ capture_full_screen: true });
    const record = { id: "session-1:meeting-screen:a1", attachmentId: "a1" };
    persistMeetingScreenCaptureMock.mockResolvedValue(record);
    captureScreenMock
      .mockResolvedValueOnce({ status: "ok", data: { ...frame, jpeg: null } })
      .mockResolvedValue({ status: "ok", data: frame });
    const visionModel = { modelId: "vision" } as never;

    const stop = startMeetingScreenCapture({
      sessionId: "session-1",
      visionModel,
    });
    await vi.advanceTimersByTimeAsync(3_000);
    await stop();

    expect(captureMeetingScreenShareMock).not.toHaveBeenCalled();
    expect(captureScreenMock).toHaveBeenCalledWith(true, null);
    expect(describeMeetingScreenMock).toHaveBeenCalledWith({
      sessionId: "session-1",
      record,
      jpeg: frame.jpeg,
      model: visionModel,
      language: "English",
    });
  });

  test("with two screens, asks which one has the meeting", async () => {
    settingsMock.mockReturnValue({
      capture_full_screen: true,
      capture_screen_id: 0,
    });
    listScreensMock.mockResolvedValue({
      status: "ok",
      data: [
        { id: 1, name: "Built-in", primary: true },
        { id: 2, name: "DELL", primary: false },
      ],
    });
    captureScreenMock.mockResolvedValue({
      status: "ok",
      data: { ...frame, app: null, platform: null, jpeg: null },
    });

    const stop = startMeetingScreenCapture({ sessionId: "session-1" });
    await vi.advanceTimersByTimeAsync(0);
    expect(captureScreenMock).toHaveBeenLastCalledWith(true, null);

    const [, options] = toastInfoMock.mock.calls[0];
    expect(options.action.label).toBe("Built-in");
    expect(options.secondaryAction.label).toBe("DELL");
    options.secondaryAction.onClick();
    expect(setSettingValueMock).toHaveBeenCalledWith("capture_screen_id", 2);

    await vi.advanceTimersByTimeAsync(3_000);
    await stop();
    expect(captureScreenMock).toHaveBeenLastCalledWith(true, 2);
    expect(listScreensMock).toHaveBeenCalledTimes(1);
  });

  test("does not ask about screens while a meeting window is captured", async () => {
    settingsMock.mockReturnValue({ capture_full_screen: true });
    listScreensMock.mockResolvedValue({
      status: "ok",
      data: [
        { id: 1, name: "Built-in", primary: true },
        { id: 2, name: "DELL", primary: false },
      ],
    });
    captureScreenMock.mockResolvedValue({
      status: "ok",
      data: { ...frame, jpeg: null },
    });

    const stop = startMeetingScreenCapture({ sessionId: "session-1" });
    await vi.advanceTimersByTimeAsync(0);
    await stop();

    expect(listScreensMock).not.toHaveBeenCalled();
    expect(toastInfoMock).not.toHaveBeenCalled();
  });

  test("full-screen mode without a vision model only saves the frame", async () => {
    settingsMock.mockReturnValue({ capture_full_screen: true });
    persistMeetingScreenCaptureMock.mockResolvedValue({ id: "r1" });
    captureScreenMock.mockResolvedValue({ status: "ok", data: frame });

    const stop = startMeetingScreenCapture({ sessionId: "session-1" });
    await vi.advanceTimersByTimeAsync(0);
    await stop();

    expect(persistMeetingScreenCaptureMock).toHaveBeenCalledTimes(1);
    expect(describeMeetingScreenMock).not.toHaveBeenCalled();
  });

  test("captures nothing when both screen settings are off", async () => {
    const stop = startMeetingScreenCapture({ sessionId: "session-1" });
    await vi.advanceTimersByTimeAsync(3_000);
    await stop();

    expect(captureScreenMock).not.toHaveBeenCalled();
    expect(captureMeetingScreenShareMock).not.toHaveBeenCalled();
  });

  test("drops a frame when the setting is turned off mid-capture", async () => {
    let enabled = true;
    const pending = deferred<unknown>();
    captureMeetingScreenShareMock.mockReturnValue(pending.promise);

    const stop = startMeetingScreenCapture({
      sessionId: "session-1",
      isEnabled: () => enabled,
    });
    await vi.advanceTimersByTimeAsync(0);
    enabled = false;
    pending.resolve({ status: "ok", data: frame });
    await vi.advanceTimersByTimeAsync(0);
    await stop();

    expect(persistMeetingScreenCaptureMock).not.toHaveBeenCalled();
  });
});
