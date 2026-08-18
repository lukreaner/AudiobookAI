import type { ProviderKind, ProviderProfile, ProviderRole } from "../api/types";

export type { ProviderRole } from "../api/types";
export type ProviderMode = ProviderProfile["mode"];
export type ProviderModelSource = "discover" | "default_only" | "none";

export interface ProviderRoleDefaults {
  defaultModel: string;
  modelSource: ProviderModelSource;
}

export interface ProviderPreset {
  kind: ProviderKind;
  name: string;
  /** Default role plus any additional roles supported by the same visible vendor. */
  role: ProviderRole;
  roles?: ProviderRole[];
  roleDefaults?: Partial<Record<ProviderRole, ProviderRoleDefaults>>;
  hidden?: boolean;
  setupVisible?: boolean;
  local: boolean;
  defaultMode: ProviderMode;
  modes: ProviderMode[];
  defaultEndpoint: string;
  defaultModel: string;
  modelSource: ProviderModelSource;
}

/**
 * Setup-safe defaults shared by first-run and the full provider editor. Versioned
 * OpenAI-compatible bases intentionally omit a trailing slash because the adapters append
 * `v1/...` paths with URL-join semantics.
 */
export const providerPresets: ProviderPreset[] = [
  {
    kind: "elevenlabs",
    name: "ElevenLabs",
    role: "tts",
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://api.elevenlabs.io/",
    defaultModel: "eleven_multilingual_v2",
    modelSource: "discover",
  },
  {
    kind: "mlx_audio",
    name: "MLX-audio",
    role: "tts",
    local: true,
    defaultMode: "external_endpoint",
    modes: ["external_endpoint", "managed_child"],
    defaultEndpoint: "http://127.0.0.1:8000/",
    defaultModel: "kokoro",
    modelSource: "discover",
  },
  {
    kind: "localai",
    name: "LocalAI",
    role: "tts",
    local: true,
    defaultMode: "external_endpoint",
    modes: ["external_endpoint", "managed_child"],
    defaultEndpoint: "http://127.0.0.1:8080/",
    defaultModel: "tts-1",
    modelSource: "discover",
  },
  {
    kind: "alltalk_v2",
    name: "AllTalk V2",
    role: "tts",
    local: true,
    defaultMode: "external_endpoint",
    modes: ["external_endpoint", "managed_child"],
    defaultEndpoint: "http://127.0.0.1:7851/",
    defaultModel: "alltalk-v2",
    modelSource: "default_only",
  },
  {
    kind: "piper",
    name: "Piper",
    role: "tts",
    local: true,
    setupVisible: false,
    defaultMode: "native",
    modes: ["native"],
    defaultEndpoint: "",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "native_os",
    name: "Native system voices",
    role: "tts",
    local: true,
    defaultMode: "native",
    modes: ["native"],
    defaultEndpoint: "",
    defaultModel: "",
    modelSource: "none",
  },
  {
    kind: "openai_tts",
    name: "OpenAI Speech (legacy)",
    role: "tts",
    hidden: true,
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://api.openai.com/",
    defaultModel: "gpt-4o-mini-tts",
    modelSource: "discover",
  },
  {
    kind: "openai",
    name: "OpenAI",
    role: "llm",
    roles: ["tts", "llm"],
    roleDefaults: {
      tts: { defaultModel: "gpt-4o-mini-tts", modelSource: "discover" },
      llm: { defaultModel: "", modelSource: "discover" },
    },
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://api.openai.com/",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "anthropic",
    name: "Anthropic Claude",
    role: "llm",
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://api.anthropic.com/",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "gemini",
    name: "Google Gemini",
    role: "llm",
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://generativelanguage.googleapis.com/",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "qwen",
    name: "Qwen (Model Studio International)",
    role: "llm",
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "kimi",
    name: "Kimi (China)",
    role: "llm",
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://api.moonshot.cn/v1",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "moonshot",
    name: "Kimi / Moonshot (International)",
    role: "llm",
    local: false,
    defaultMode: "cloud_remote",
    modes: ["cloud_remote"],
    defaultEndpoint: "https://api.moonshot.ai/v1",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "openai_compatible",
    name: "OpenAI-compatible",
    role: "llm",
    local: true,
    defaultMode: "external_endpoint",
    modes: ["external_endpoint", "cloud_remote"],
    defaultEndpoint: "",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "lm_studio",
    name: "LM Studio",
    role: "llm",
    local: true,
    defaultMode: "external_endpoint",
    modes: ["external_endpoint", "managed_child"],
    defaultEndpoint: "http://127.0.0.1:1234/",
    defaultModel: "",
    modelSource: "discover",
  },
  {
    kind: "ollama",
    name: "Ollama",
    role: "llm",
    local: true,
    defaultMode: "external_endpoint",
    modes: ["external_endpoint", "managed_child"],
    defaultEndpoint: "http://127.0.0.1:11434/",
    defaultModel: "",
    modelSource: "discover",
  },
];

export function providerPreset(kind: ProviderKind): ProviderPreset {
  const preset = providerPresets.find((candidate) => candidate.kind === kind);
  if (!preset) throw new Error(`Missing provider preset for ${kind}`);
  return preset;
}

export function providerPresetsFor(role: ProviderRole): ProviderPreset[] {
  return providerPresets.filter((preset) => !preset.hidden && providerSupportsRole(preset, role));
}

export function providerRoles(preset: ProviderPreset): ProviderRole[] {
  return preset.roles ?? [preset.role];
}

export function providerSupportsRole(preset: ProviderPreset, role: ProviderRole): boolean {
  return providerRoles(preset).includes(role);
}

export function providerDefaultsForRole(preset: ProviderPreset, role: ProviderRole): ProviderRoleDefaults {
  const defaults = preset.roleDefaults?.[role];
  if (defaults) return defaults;
  if (preset.role === role) return { defaultModel: preset.defaultModel, modelSource: preset.modelSource };
  throw new Error(`${preset.kind} does not support provider role ${role}`);
}
