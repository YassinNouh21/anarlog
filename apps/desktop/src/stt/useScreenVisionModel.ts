import type { LanguageModel } from "ai";

import {
  useLanguageModel,
  useLLMConnection,
} from "~/ai/hooks/useLLMConnection";
import { modelSupportsImageInput } from "~/settings/ai/shared/model-capabilities";

/**
 * The selected chat model, when it accepts images. Used to describe captured
 * screens; returns null for text-only models so no image is ever sent to them.
 */
export function useScreenVisionModel(): LanguageModel | null {
  const model = useLanguageModel();
  const { conn } = useLLMConnection();
  if (
    !model ||
    !conn ||
    !modelSupportsImageInput(conn.providerId, conn.modelId)
  ) {
    return null;
  }
  return model;
}
