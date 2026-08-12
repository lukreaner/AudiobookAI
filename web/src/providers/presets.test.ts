import { describe, expect, it } from "vitest";
import type { ProviderKind } from "../api/types";
import { providerPreset, providerPresets, providerPresetsFor } from "./presets";

const supportedProviderKinds: ProviderKind[] = [
  "elevenlabs",
  "mlx_audio",
  "localai",
  "alltalk_v2",
  "native_os",
  "openai_tts",
  "openai",
  "openai_compatible",
  "anthropic",
  "gemini",
  "qwen",
  "kimi",
  "moonshot",
  "lm_studio",
  "ollama",
];

describe("provider presets", () => {
  it("covers every supported provider exactly once", () => {
    expect(providerPresets.map((preset) => preset.kind).sort()).toEqual(
      [...supportedProviderKinds].sort(),
    );
    expect(new Set(providerPresets.map((preset) => preset.kind))).toHaveLength(
      providerPresets.length,
    );
  });

  it("keeps TTS and LLM presets separate and supplies required endpoints", () => {
    expect(providerPresetsFor("tts")).toHaveLength(6);
    expect(providerPresetsFor("llm")).toHaveLength(9);
    for (const preset of providerPresets) {
      expect(providerPreset(preset.kind)).toBe(preset);
      if (preset.defaultMode !== "native" && preset.kind !== "openai_compatible") {
        expect(preset.defaultEndpoint).toMatch(/^https?:\/\//);
      }
    }
  });

  it("keeps OpenAI speech and language-model settings independent", () => {
    expect(providerPreset("openai_tts")).toMatchObject({
      name: "OpenAI Speech",
      role: "tts",
      defaultModel: "gpt-4o-mini-tts",
      defaultEndpoint: "https://api.openai.com/",
    });
    expect(providerPreset("openai")).toMatchObject({
      name: "OpenAI",
      role: "llm",
      defaultModel: "",
      defaultEndpoint: "https://api.openai.com/",
    });
  });
});
