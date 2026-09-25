import type { DetectionReasoning, DetectionTemperature, ProviderProfile } from "../api/types";

/**
 * Temperature and reasoning options the selected detection model accepts, as determined by the
 * service from the provider for exactly that model. "Provider default" is always allowed.
 */
export interface DetectionControls {
  temperatureModes: DetectionTemperature["mode"][];
  maxTemperature: number;
  reasoningModes: DetectionReasoning["mode"][];
  efforts: string[];
  minBudget: number;
  maxBudget?: number;
}

const REASONING_ORDER: DetectionReasoning["mode"][] = ["disabled", "effort", "adaptive", "token_budget"];

export function detectionControls(provider?: Pick<ProviderProfile, "model" | "capabilities">): DetectionControls {
  // Options count only when they were determined for exactly the configured model; otherwise
  // the service would reject them, so only provider defaults are offered.
  const capabilities = provider?.capabilities && provider.model
    && provider.capabilities.generationControlsModel === provider.model
    ? provider.capabilities
    : undefined;
  const temperatureModes: DetectionTemperature["mode"][] = ["default"];
  if (capabilities?.temperature === "nullable") temperatureModes.push("null", "value");
  else if (capabilities?.temperature === "number") temperatureModes.push("value");
  const reasoning = capabilities?.reasoning ?? [];
  const efforts = capabilities?.reasoningEfforts ?? [];
  return {
    temperatureModes,
    maxTemperature: capabilities?.maxTemperature ?? 2,
    reasoningModes: [
      "inherit",
      ...REASONING_ORDER.filter((mode) => reasoning.includes(mode) && (mode !== "effort" || efforts.length > 0)),
    ],
    efforts,
    minBudget: capabilities?.minReasoningBudget ?? 1024,
    maxBudget: capabilities?.maxReasoningBudget ?? undefined,
  };
}

/** Keeps a selection only while the current model still allows it. */
export function allowedOr<T>(value: T, allowed: readonly T[], fallback: T): T {
  return allowed.includes(value) ? value : fallback;
}

/** A balanced default level: "medium" when offered, otherwise the middle of the list. */
export function defaultEffort(efforts: readonly string[]): string {
  if (efforts.includes("medium")) return "medium";
  return efforts[Math.floor((efforts.length - 1) / 2)] ?? "medium";
}

/** Detection settings remembered per project so they survive navigating away and back. */
export interface StoredDetectionSettings {
  provider: string;
  temperatureMode: DetectionTemperature["mode"];
  temperatureValue: number;
  reasoningMode: DetectionReasoning["mode"];
  reasoningEffort: string;
  reasoningTokens: number;
}

const TEMPERATURE_MODES: DetectionTemperature["mode"][] = ["default", "null", "value"];
const REASONING_MODES: DetectionReasoning["mode"][] = ["inherit", ...REASONING_ORDER];

function storageKey(projectId: string): string {
  return `audiobookai.detectionSettings.${projectId}`;
}

export function readDetectionSettings(projectId: string): Partial<StoredDetectionSettings> {
  try {
    const raw = localStorage.getItem(storageKey(projectId));
    const value: unknown = raw ? JSON.parse(raw) : undefined;
    if (!value || typeof value !== "object") return {};
    const stored = value as Record<string, unknown>;
    const settings: Partial<StoredDetectionSettings> = {};
    if (typeof stored.provider === "string") settings.provider = stored.provider;
    if (TEMPERATURE_MODES.includes(stored.temperatureMode as DetectionTemperature["mode"])) {
      settings.temperatureMode = stored.temperatureMode as DetectionTemperature["mode"];
    }
    if (typeof stored.temperatureValue === "number" && Number.isFinite(stored.temperatureValue)) {
      settings.temperatureValue = stored.temperatureValue;
    }
    if (REASONING_MODES.includes(stored.reasoningMode as DetectionReasoning["mode"])) {
      settings.reasoningMode = stored.reasoningMode as DetectionReasoning["mode"];
    }
    if (typeof stored.reasoningEffort === "string") settings.reasoningEffort = stored.reasoningEffort;
    if (typeof stored.reasoningTokens === "number" && Number.isFinite(stored.reasoningTokens)) {
      settings.reasoningTokens = stored.reasoningTokens;
    }
    return settings;
  } catch {
    return {};
  }
}

export function writeDetectionSettings(projectId: string, settings: StoredDetectionSettings): void {
  try {
    localStorage.setItem(storageKey(projectId), JSON.stringify(settings));
  } catch {
    // Remembering the selection is a convenience; detection works without it.
  }
}
