import { describe, expect, it } from "vitest";
import type { ProviderCapabilities } from "../api/types";
import { allowedOr, defaultEffort, detectionControls } from "./detectionControls";

function capabilities(overrides: Partial<ProviderCapabilities>): { model: string; capabilities: ProviderCapabilities } {
  return { model: "model-x", capabilities: {
    tts: false, characterDetection: true, streaming: false, voiceCloning: false, pronunciation: false,
    processControl: false, modelControl: false, modelList: false, modelDownload: false, modelDelete: false,
    modelLoad: false, modelUnload: false, modelSwitch: false, temperature: "unsupported", reasoning: [],
    modelPerformance: [], generationControlsModel: "model-x", ...overrides,
  } };
}

describe("detection controls", () => {
  it("offers exactly the levels determined for GPT-6 Luna and no temperature", () => {
    const controls = detectionControls(capabilities({
      reasoning: ["disabled", "effort"],
      reasoningEfforts: ["low", "medium", "high", "xhigh", "max"],
    }));
    expect(controls.temperatureModes).toEqual(["default"]);
    expect(controls.reasoningModes).toEqual(["inherit", "disabled", "effort"]);
    expect(controls.efforts).not.toContain("minimal");
    expect(allowedOr("minimal", controls.efforts, defaultEffort(controls.efforts))).toBe("medium");
  });

  it("never offers an effort mode without determined levels", () => {
    const controls = detectionControls(capabilities({ reasoning: ["effort"], reasoningEfforts: [] }));
    expect(controls.reasoningModes).toEqual(["inherit"]);
  });

  it("uses the model's temperature ceiling and budget minimum", () => {
    const controls = detectionControls(capabilities({
      temperature: "number", maxTemperature: 1, reasoning: ["token_budget"], minReasoningBudget: 2048,
    }));
    expect(controls.temperatureModes).toEqual(["default", "value"]);
    expect(controls.maxTemperature).toBe(1);
    expect(controls.minBudget).toBe(2048);
  });

  it("ignores options determined for a different model", () => {
    const provider = capabilities({ temperature: "number", reasoning: ["disabled"], generationControlsModel: "old-model" });
    expect(detectionControls(provider).reasoningModes).toEqual(["inherit"]);
    expect(detectionControls(provider).temperatureModes).toEqual(["default"]);
  });

  it("falls back to provider defaults when nothing is known", () => {
    const controls = detectionControls(undefined);
    expect(controls.temperatureModes).toEqual(["default"]);
    expect(controls.reasoningModes).toEqual(["inherit"]);
    expect(defaultEffort(["low", "high", "max"])).toBe("high");
  });
});
