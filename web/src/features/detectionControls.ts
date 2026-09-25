import type { DetectionReasoning, DetectionTemperature, ProviderCapabilities } from "../api/types";

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

export function detectionControls(capabilities?: ProviderCapabilities): DetectionControls {
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
