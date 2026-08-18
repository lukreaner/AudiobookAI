import { describe, expect, it } from "vitest";
import type { ProviderKind } from "../api/types";
import { providerDefaultsForRole, providerPreset, providerPresets, providerPresetsFor, providerRoles } from "./presets";

const supportedProviderKinds: ProviderKind[] = [
  "elevenlabs",
  "mlx_audio",
  "localai",
  "alltalk_v2",
  "piper",
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
    expect(providerPresetsFor("tts")).toHaveLength(7);
    expect(providerPresetsFor("llm")).toHaveLength(9);
    for (const preset of providerPresets) {
      expect(providerPreset(preset.kind)).toBe(preset);
      if (preset.defaultMode !== "native" && preset.kind !== "openai_compatible") {
        expect(preset.defaultEndpoint).toMatch(/^https?:\/\//);
      }
    }
  });

  it("presents OpenAI once while keeping role-specific settings independent", () => {
    expect(providerPreset("openai_tts")).toMatchObject({
      name: "OpenAI Speech (legacy)",
      role: "tts",
      hidden: true,
      defaultModel: "gpt-4o-mini-tts",
      defaultEndpoint: "https://api.openai.com/",
    });
    const openai = providerPreset("openai");
    expect(openai).toMatchObject({
      name: "OpenAI",
      role: "llm",
      defaultEndpoint: "https://api.openai.com/",
    });
    expect(providerRoles(openai)).toEqual(["tts", "llm"]);
    expect(providerDefaultsForRole(openai, "tts")).toEqual({ defaultModel: "gpt-4o-mini-tts", modelSource: "discover" });
    expect(providerDefaultsForRole(openai, "llm")).toEqual({ defaultModel: "", modelSource: "discover" });
    expect(providerPresetsFor("tts").filter((preset) => preset.kind === "openai")).toHaveLength(1);
    expect(providerPresetsFor("llm").filter((preset) => preset.kind === "openai")).toHaveLength(1);
  });

  it("offers Piper only as a native TTS provider with installed-model discovery", () => {
    expect(providerPreset("piper")).toMatchObject({
      name: "Piper",
      role: "tts",
      local: true,
      setupVisible: false,
      defaultMode: "native",
      modes: ["native"],
      modelSource: "discover",
    });
    expect(providerPresetsFor("llm").some((preset) => preset.kind === "piper")).toBe(false);
  });
});
