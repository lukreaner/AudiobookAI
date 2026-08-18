import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { MlxManagement, PiperManagement, ProviderProfile } from "../api/types";
import i18n from "../i18n";
import { ProvidersPage } from "./ProvidersPage";

const managedProvider: ProviderProfile = {
  id: "provider-localai",
  name: "LocalAI",
  kind: "localai",
  role: "tts",
  mode: "managed_child",
  endpoint: "http://127.0.0.1:8080",
  executablePath: "/opt/localai/local-ai",
  workingDirectory: "/opt/localai",
  arguments: ["--address", "127.0.0.1:8080"],
  status: "offline",
  model: "voice-model",
  credentialConfigured: false,
  capabilities: {
    tts: true,
    characterDetection: false,
    streaming: true,
    voiceCloning: false,
    pronunciation: false,
    processControl: true,
    modelControl: false,
    modelList: false,
    modelDownload: false,
    modelDelete: false,
    modelLoad: false,
    modelUnload: false,
    modelSwitch: false,
    temperature: "unsupported",
    reasoning: [],
    modelPerformance: [],
  },
};

const mlxManagement: MlxManagement = {
  supported: true,
  supportDetail: "MLX-audio app management is available on Apple Silicon.",
  installerStatus: "ready",
  uvAvailable: true,
  requiredUvVersion: "0.12.1",
  installerPayloadAvailable: true,
  installed: false,
  models: [],
  profileActionRequired: false,
};

const piperManagement: PiperManagement = {
  supported: false,
  supportDetail: "Piper app management is available on Linux.",
  installerStatus: "unsupported_platform",
  installed: false,
  catalog: [],
  installedVoices: [],
  voiceIssues: [],
  profileActionRequired: false,
};

function renderProviders() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <ProvidersPage />
    </QueryClientProvider>,
  );
}

beforeEach(async () => {
  vi.restoreAllMocks();
  await i18n.changeLanguage("en");
  vi.spyOn(api, "providers").mockResolvedValue({ items: [structuredClone(managedProvider)], total: 1 });
  vi.spyOn(api, "nativeProviderAvailability").mockResolvedValue({
    platform: "linux",
    providerName: "eSpeak NG",
    available: true,
    detail: null,
  });
  vi.spyOn(api, "piperManagement").mockResolvedValue(structuredClone(piperManagement));
  vi.spyOn(api, "installPiper").mockResolvedValue({ id: "piper-install", kind: "install", state: "queued", progressPercent: 0, phase: "queued", message: "Queued", startedAt: new Date().toISOString() });
  vi.spyOn(api, "uninstallPiper").mockResolvedValue({ id: "piper-uninstall", kind: "uninstall", state: "queued", progressPercent: 0, phase: "queued", message: "Queued", startedAt: new Date().toISOString() });
  vi.spyOn(api, "cancelPiperOperation").mockResolvedValue({ id: "piper-operation", kind: "download_voice", state: "cancelling", progressPercent: 50, phase: "cancelling", message: "Cancelling", voiceId: "de_DE-thorsten-medium", startedAt: new Date().toISOString() });
  vi.spyOn(api, "downloadPiperVoice").mockResolvedValue({ id: "piper-download", kind: "download_voice", state: "queued", progressPercent: 0, phase: "queued", message: "Queued", voiceId: "de_DE-thorsten-medium", startedAt: new Date().toISOString() });
  vi.spyOn(api, "removePiperVoice").mockResolvedValue(undefined);
  vi.spyOn(api, "mlxManagement").mockResolvedValue(structuredClone(mlxManagement));
  vi.spyOn(api, "installMlx").mockResolvedValue({ id: "operation-install", kind: "install", state: "queued", progressPercent: 0, phase: "queued", message: "Queued", startedAt: new Date().toISOString() });
  vi.spyOn(api, "uninstallMlx").mockResolvedValue({ id: "operation-uninstall", kind: "uninstall", state: "queued", progressPercent: 0, phase: "queued", message: "Queued", startedAt: new Date().toISOString() });
  vi.spyOn(api, "removeMlxModel").mockResolvedValue(undefined);
  vi.spyOn(api, "providerModels").mockResolvedValue({ models: [], operations: [] });
  vi.spyOn(api, "discoverProviderModels").mockResolvedValue({ items: [], strict: false });
  vi.spyOn(api, "createProvider").mockImplementation(async (input) => ({
    ...structuredClone(managedProvider),
    ...input,
    id: "provider-created",
    endpoint: input.endpoint ?? undefined,
    executablePath: input.executablePath ?? undefined,
    workingDirectory: input.workingDirectory ?? undefined,
    model: input.model ?? undefined,
  } as ProviderProfile));
  vi.spyOn(api, "downloadProviderModel").mockResolvedValue({
    id: "provider-model-operation",
    providerProfileId: managedProvider.id,
    model: "localai@voice-model",
    state: "running",
    startedAt: new Date().toISOString(),
  });
  vi.spyOn(api, "cancelProviderModelDownload").mockResolvedValue({
    id: "provider-model-operation",
    providerProfileId: managedProvider.id,
    model: "localai@voice-model",
    state: "cancelling",
    startedAt: new Date().toISOString(),
  });
  vi.spyOn(api, "deleteProviderModel").mockResolvedValue(undefined);
  vi.spyOn(api, "updateProvider").mockImplementation(async (_id, input) => ({
    ...structuredClone(managedProvider),
    ...input,
    endpoint: input.endpoint ?? undefined,
    executablePath: input.executablePath ?? undefined,
    workingDirectory: input.workingDirectory ?? undefined,
    model: input.model ?? undefined,
  } as ProviderProfile));
  vi.spyOn(api, "deleteProvider").mockResolvedValue(undefined);
});

describe("managed provider configuration", () => {
  it("fetches available models automatically and renders them as choices", async () => {
    vi.mocked(api.discoverProviderModels).mockResolvedValue({
      items: [
        { id: "qwen3:8b", name: "Qwen 3 8B" },
        { id: "gemma3:4b", name: "Gemma 3 4B" },
      ],
      strict: false,
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText(/^Provider use/), "llm");
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "ollama");

    await waitFor(() => expect(api.discoverProviderModels).toHaveBeenCalledWith(expect.objectContaining({
      kind: "ollama",
      role: "llm",
      mode: "external_endpoint",
      endpoint: "http://127.0.0.1:11434/",
    })));
    await waitFor(() => expect(screen.getByLabelText(/^LLM model/).tagName).toBe("SELECT"));
    expect(screen.getByText("Available models detected automatically: 2")).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "Qwen 3 8B (qwen3:8b)" })).toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText(/^LLM model/), "qwen3:8b");
    expect(screen.getByLabelText(/^LLM model/)).toHaveValue("qwen3:8b");
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));
    await waitFor(() => expect(api.createProvider).toHaveBeenCalledWith(expect.objectContaining({
      kind: "ollama",
      role: "llm",
      model: "qwen3:8b",
    })));
  });

  it("offers OpenAI in TTS mode with strict speech-only model choices", async () => {
    vi.mocked(api.discoverProviderModels).mockResolvedValue({
      items: [
        { id: "gpt-4o-mini-tts", name: "gpt-4o-mini-tts" },
        { id: "tts-1-hd", name: "tts-1-hd" },
      ],
      strict: true,
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "openai");

    expect(screen.getByText("Text to speech (TTS)")).toBeInTheDocument();
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("https://api.openai.com/");
    expect(screen.getByLabelText(/^TTS model/)).toBeDisabled();
    expect(screen.queryByRole("option", { name: "Enter a model manually" })).not.toBeInTheDocument();
    await user.type(screen.getByLabelText(/^API key/), "test-openai-key");

    await waitFor(() => expect(api.discoverProviderModels).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: "openai",
        role: "tts",
        mode: "cloud_remote",
        endpoint: "https://api.openai.com/",
      }),
    ));
    await waitFor(() => expect(screen.getByLabelText(/^TTS model/).tagName).toBe("SELECT"));
    expect(screen.getByLabelText(/^TTS model/)).toHaveValue("gpt-4o-mini-tts");
    expect(screen.getByRole("option", { name: "tts-1-hd" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "Enter a model manually" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Save and check connection" }));
    await waitFor(() => expect(api.createProvider).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: "openai",
        role: "tts",
        model: "gpt-4o-mini-tts",
      }),
    ));
  });

  it("keeps separate OpenAI TTS and LLM connections with the same endpoint", async () => {
    vi.mocked(api.discoverProviderModels).mockImplementation(async (input) => input.role === "tts"
      ? { items: [{ id: "gpt-4o-mini-tts", name: "gpt-4o-mini-tts" }], strict: true }
      : { items: [{ id: "gpt-5-mini", name: "gpt-5-mini" }], strict: true });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "openai");
    await user.type(screen.getByLabelText(/^API key/), "test-openai-key");
    await waitFor(() => expect(screen.getByRole("option", { name: "gpt-4o-mini-tts" })).toBeInTheDocument());
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));
    await waitFor(() => expect(api.createProvider).toHaveBeenCalledTimes(1));

    await user.click(screen.getByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText(/^Provider use/), "llm");
    expect(screen.getByLabelText("Choose a provider type")).toHaveValue("openai");
    await user.type(screen.getByLabelText(/^API key/), "test-openai-key");
    await waitFor(() => expect(screen.getByRole("option", { name: "gpt-5-mini" })).toBeInTheDocument());
    await user.selectOptions(screen.getByLabelText(/^LLM model/), "gpt-5-mini");
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));

    await waitFor(() => expect(api.createProvider).toHaveBeenCalledTimes(2));
    expect(api.createProvider).toHaveBeenNthCalledWith(1, expect.objectContaining({
      kind: "openai", role: "tts", endpoint: "https://api.openai.com/", model: "gpt-4o-mini-tts",
    }));
    expect(api.createProvider).toHaveBeenNthCalledWith(2, expect.objectContaining({
      kind: "openai", role: "llm", endpoint: "https://api.openai.com/", model: "gpt-5-mini",
    }));
  });

  it("clears the previous role's discovered models as soon as the role changes", async () => {
    vi.mocked(api.discoverProviderModels).mockImplementation(async (input) => input.role === "tts"
      ? { items: [{ id: "gpt-4o-mini-tts", name: "gpt-4o-mini-tts" }], strict: true }
      : { items: [{ id: "gpt-5-mini", name: "gpt-5-mini" }], strict: true });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "openai");
    await user.type(screen.getByLabelText(/^API key/), "test-openai-key");
    await waitFor(() => expect(screen.getByRole("option", { name: "gpt-4o-mini-tts" })).toBeInTheDocument());

    await user.selectOptions(screen.getByLabelText(/^Provider use/), "llm");
    expect(screen.queryByRole("option", { name: "gpt-4o-mini-tts" })).not.toBeInTheDocument();
    expect(screen.getByLabelText(/^LLM model/)).toBeDisabled();
    await waitFor(() => expect(screen.getByRole("option", { name: "gpt-5-mini" })).toBeInTheDocument());
  });

  it("edits the persisted role and revalidates the model for a dual-role provider", async () => {
    const openaiProvider: ProviderProfile = {
      ...structuredClone(managedProvider),
      id: "provider-openai",
      name: "OpenAI shared account",
      kind: "openai",
      role: "tts",
      mode: "cloud_remote",
      endpoint: "https://api.openai.com/",
      executablePath: undefined,
      workingDirectory: undefined,
      arguments: [],
      model: "gpt-4o-mini-tts",
      credentialConfigured: true,
      capabilities: { ...structuredClone(managedProvider.capabilities!), processControl: false },
    };
    vi.mocked(api.providers).mockResolvedValue({ items: [openaiProvider], total: 1 });
    vi.mocked(api.discoverProviderModels).mockImplementation(async (input) => input.role === "tts"
      ? { items: [{ id: "gpt-4o-mini-tts", name: "gpt-4o-mini-tts" }], strict: true }
      : { items: [{ id: "gpt-5-mini", name: "gpt-5-mini" }], strict: true });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Settings" }));
    expect(screen.getByLabelText(/^Provider use/)).toHaveValue("tts");
    await user.selectOptions(screen.getByLabelText(/^Provider use/), "llm");
    await waitFor(() => expect(screen.getByRole("option", { name: "gpt-5-mini" })).toBeInTheDocument());
    await user.selectOptions(screen.getByLabelText(/^LLM model/), "gpt-5-mini");
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));

    await waitFor(() => expect(api.updateProvider).toHaveBeenCalledWith("provider-openai", expect.objectContaining({
      role: "llm",
      model: "gpt-5-mini",
      endpoint: "https://api.openai.com/",
    })));
  });

  it("normalizes a hidden legacy OpenAI Speech connection for an LLM role switch", async () => {
    const legacyProvider: ProviderProfile = {
      ...structuredClone(managedProvider),
      id: "provider-openai-speech-legacy",
      name: "OpenAI Speech",
      kind: "openai_tts",
      role: "tts",
      mode: "cloud_remote",
      endpoint: "https://api.openai.com/",
      executablePath: undefined,
      workingDirectory: undefined,
      arguments: [],
      model: "gpt-4o-mini-tts",
      credentialConfigured: true,
      capabilities: { ...structuredClone(managedProvider.capabilities!), processControl: false },
    };
    vi.mocked(api.providers).mockResolvedValue({ items: [legacyProvider], total: 1 });
    vi.mocked(api.discoverProviderModels).mockImplementation(async (input) => input.role === "tts"
      ? { items: [{ id: "gpt-4o-mini-tts", name: "gpt-4o-mini-tts" }], strict: true }
      : { items: [{ id: "gpt-5-mini", name: "gpt-5-mini" }], strict: true });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Settings" }));
    expect(screen.getByLabelText("Choose a provider type")).toHaveValue("openai");
    await user.selectOptions(screen.getByLabelText(/^Provider use/), "llm");
    await waitFor(() => expect(screen.getByRole("option", { name: "gpt-5-mini" })).toBeInTheDocument());
    await user.selectOptions(screen.getByLabelText(/^LLM model/), "gpt-5-mini");
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));

    await waitFor(() => expect(api.updateProvider).toHaveBeenCalledWith(
      legacyProvider.id,
      expect.objectContaining({ role: "llm", model: "gpt-5-mini" }),
    ));
    expect(vi.mocked(api.updateProvider).mock.calls.at(-1)?.[1]).not.toHaveProperty("kind");
  });

  it("separates configured TTS providers from LLM providers", async () => {
    vi.mocked(api.providers).mockResolvedValue({
      items: [{ ...structuredClone(managedProvider), id: "provider-openai-tts", name: "OpenAI narration", kind: "openai", role: "tts" }],
      total: 1,
    });
    renderProviders();

    const ttsSection = await screen.findByRole("region", { name: "Text-to-speech providers" });
    const llmSection = screen.getByRole("region", { name: "LLM providers" });
    expect(screen.getByText("Automatic model detection")).toBeInTheDocument();
    expect(within(ttsSection).getByText("OpenAI narration")).toBeInTheDocument();
    expect(within(llmSection).getByText("No LLM provider is configured yet.")).toBeInTheDocument();
    expect(within(llmSection).getByRole("button", { name: "Add LLM provider" })).toBeEnabled();
  });

  it("opens a model-capable LLM preset directly from the empty role section", async () => {
    const user = userEvent.setup();
    renderProviders();

    const llmSection = await screen.findByRole("region", { name: "LLM providers" });
    await user.click(within(llmSection).getByRole("button", { name: "Add LLM provider" }));

    expect(screen.getByLabelText("Choose a provider type")).toHaveValue("openai");
    expect(screen.getByText("Language model (LLM)")).toBeInTheDocument();
    expect(screen.getByLabelText(/^LLM model/)).toBeInTheDocument();
    expect(screen.getAllByText("Models load automatically after the endpoint and required credential are configured.")).not.toHaveLength(0);
  });

  it("explains that native system voices do not expose a model catalog", async () => {
    const nativeProvider: ProviderProfile = {
      ...structuredClone(managedProvider),
      id: "provider-native",
      name: "eSpeak NG",
      kind: "native_os",
      mode: "native",
      endpoint: undefined,
      executablePath: undefined,
      workingDirectory: undefined,
      arguments: [],
      model: undefined,
      status: "online",
    };
    vi.mocked(api.providers).mockResolvedValue({ items: [nativeProvider], total: 1 });
    const user = userEvent.setup();
    renderProviders();

    const ttsSection = await screen.findByRole("region", { name: "Text-to-speech providers" });
    expect(within(ttsSection).getByText("Runs locally on this computer")).toBeInTheDocument();
    expect(within(ttsSection).getByText("eSpeak NG · no model catalog")).toBeInTheDocument();
    await user.click(within(ttsSection).getByRole("button", { name: "Settings" }));
    expect(screen.getByText("This native provider uses system voices and does not expose a model catalog.")).toBeInTheDocument();
    expect(screen.queryByLabelText(/^TTS model/)).not.toBeInTheDocument();
    expect(api.discoverProviderModels).not.toHaveBeenCalled();
  });

  it("disables unavailable Linux native TTS and replaces its raw profile error with setup guidance", async () => {
    const nativeProvider: ProviderProfile = {
      ...structuredClone(managedProvider),
      id: "provider-native-missing",
      name: "Native system voices",
      kind: "native_os",
      mode: "native",
      endpoint: undefined,
      executablePath: undefined,
      workingDirectory: undefined,
      arguments: [],
      model: undefined,
      status: "unconfigured",
      lastError: "provider configuration is invalid: native TTS executable must be an existing absolute file",
    };
    vi.mocked(api.providers).mockResolvedValue({ items: [nativeProvider], total: 1 });
    vi.mocked(api.nativeProviderAvailability).mockResolvedValue({
      platform: "linux",
      providerName: "eSpeak NG",
      available: false,
      detail: "No eSpeak NG executable was found.",
    });
    const user = userEvent.setup();
    renderProviders();

    const ttsSection = await screen.findByRole("region", { name: "Text-to-speech providers" });
    expect(within(ttsSection).getByText("eSpeak NG needs setup")).toBeInTheDocument();
    expect(within(ttsSection).getByText(/Linux does not include a speech engine by default/)).toBeInTheDocument();
    expect(within(ttsSection).getByText(/managed Piper section above/)).toBeInTheDocument();
    expect(within(ttsSection).queryByText("No eSpeak NG executable was found.")).not.toBeInTheDocument();
    expect(screen.queryByText(/native TTS executable must be an existing absolute file/)).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Add provider" }));
    const dialog = screen.getByRole("dialog", { name: "Add provider" });
    const nativeOption = within(dialog).getByRole("option", { name: "eSpeak NG — setup needed" });
    expect(nativeOption).toBeDisabled();
    const nativeSetupSummary = dialog.querySelector("summary");
    expect(nativeSetupSummary).not.toBeNull();
    await user.click(nativeSetupSummary!);
    expect(within(dialog).getByText(/Install eSpeak NG through your system package manager/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Save and check connection" })).toBeEnabled();
  });

  it("does not mark a working profile-specific native executable as broken", async () => {
    const nativeProvider: ProviderProfile = {
      ...structuredClone(managedProvider),
      id: "provider-native-override",
      name: "Studio eSpeak",
      kind: "native_os",
      mode: "native",
      endpoint: undefined,
      executablePath: "/opt/studio/bin/espeak-ng",
      workingDirectory: undefined,
      arguments: [],
      model: undefined,
      status: "online",
    };
    vi.mocked(api.providers).mockResolvedValue({ items: [nativeProvider], total: 1 });
    vi.mocked(api.nativeProviderAvailability).mockResolvedValue({
      platform: "linux",
      providerName: "eSpeak NG",
      available: false,
      detail: "No global eSpeak NG executable was found.",
    });
    const user = userEvent.setup();
    renderProviders();

    const ttsSection = await screen.findByRole("region", { name: "Text-to-speech providers" });
    expect(within(ttsSection).getByText("Online")).toBeInTheDocument();
    expect(within(ttsSection).queryByText("eSpeak NG needs setup")).not.toBeInTheDocument();
    await user.click(within(ttsSection).getByRole("button", { name: "Settings" }));
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));

    await waitFor(() => expect(api.updateProvider).toHaveBeenCalledWith(nativeProvider.id, expect.any(Object)));
    const nativePatch = vi.mocked(api.updateProvider).mock.calls.at(-1)?.[1];
    expect(nativePatch).not.toHaveProperty("executablePath");
  });

  it("rechecks native availability when Add opens and offers eSpeak NG after it is installed", async () => {
    vi.mocked(api.nativeProviderAvailability)
      .mockResolvedValueOnce({ platform: "linux", providerName: "eSpeak NG", available: false, detail: "Not found." })
      .mockResolvedValue({ platform: "linux", providerName: "eSpeak NG", available: true, detail: null });
    const user = userEvent.setup();
    renderProviders();

    expect(await screen.findByText("LocalAI")).toBeInTheDocument();
    expect(api.nativeProviderAvailability).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole("button", { name: "Add provider" }));

    await waitFor(() => expect(screen.getByRole("option", { name: "eSpeak NG" })).toBeEnabled());
    expect(api.nativeProviderAvailability).toHaveBeenCalledTimes(2);
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "native_os");
    expect(screen.getByLabelText("Title")).toHaveValue("eSpeak NG");
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));
    await waitFor(() => expect(api.createProvider).toHaveBeenCalledWith(expect.objectContaining({
      kind: "native_os",
      role: "tts",
      mode: "native",
      name: "eSpeak NG",
    })));
  });

  it("removes an unavailable native system-voice connection without implying software uninstall", async () => {
    const nativeProvider: ProviderProfile = {
      ...structuredClone(managedProvider),
      id: "provider-native-unavailable",
      name: "eSpeak NG",
      kind: "native_os",
      mode: "native",
      endpoint: undefined,
      executablePath: undefined,
      workingDirectory: undefined,
      arguments: [],
      model: undefined,
      status: "error",
      lastError: "native TTS executable is unavailable",
    };
    vi.mocked(api.providers)
      .mockResolvedValueOnce({ items: [nativeProvider], total: 1 })
      .mockResolvedValue({ items: [], total: 0 });
    const user = userEvent.setup();
    renderProviders();

    const ttsSection = await screen.findByRole("region", { name: "Text-to-speech providers" });
    await user.click(within(ttsSection).getByRole("button", { name: "Settings" }));
    await user.click(screen.getByRole("button", { name: "Delete provider" }));

    const dialog = screen.getByRole("dialog", { name: "Delete eSpeak NG?" });
    expect(within(dialog).getByText(/does not uninstall or change eSpeak NG/)).toBeInTheDocument();
    expect(within(dialog).getByText(/add the connection again later/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Delete provider" }));

    await waitFor(() => expect(api.deleteProvider).toHaveBeenCalledWith(nativeProvider.id));
    await waitFor(() => expect(screen.queryByText("eSpeak NG")).not.toBeInTheDocument());
  });

  it("discovers only installed voices for a native Piper connection", async () => {
    vi.mocked(api.discoverProviderModels).mockResolvedValue({
      items: [{ id: "de_DE-thorsten-medium", name: "Thorsten (German, medium)" }],
      strict: true,
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "piper");

    await waitFor(() => expect(api.discoverProviderModels).toHaveBeenCalledWith(expect.objectContaining({
      kind: "piper",
      role: "tts",
      mode: "native",
      endpoint: null,
    })));
    expect(screen.queryByLabelText(/^API key/)).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("option", { name: "Thorsten (German, medium) (de_DE-thorsten-medium)" })).toBeInTheDocument());
    await user.selectOptions(screen.getByLabelText(/^TTS model/), "de_DE-thorsten-medium");
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));
    await waitFor(() => expect(api.createProvider).toHaveBeenCalledWith(expect.objectContaining({
      kind: "piper",
      role: "tts",
      mode: "native",
      model: "de_DE-thorsten-medium",
    })));
  });

  it("applies working presets and discards transient credentials when provider type changes", async () => {
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("https://api.elevenlabs.io/");
    await user.clear(screen.getByLabelText(/^Endpoint URL/));
    await user.type(screen.getByLabelText(/^Endpoint URL/), "https://api.example.test");
    await user.type(screen.getByLabelText(/^API key/), "temporary-provider-credential");
    await user.selectOptions(screen.getByLabelText(/^Provider use/), "llm");
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "ollama");

    expect(screen.getByText("Language model (LLM)")).toBeInTheDocument();
    expect(screen.getByLabelText("Deployment")).toHaveValue("external_endpoint");
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("http://127.0.0.1:11434/");
    expect(screen.getByLabelText(/^API key/)).toHaveValue("");
  });

  it("sends literal argv values and explicit clears", async () => {
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Settings" }));
    expect(screen.getByLabelText(/^Working directory/)).toHaveValue("/opt/localai");
    expect(screen.getByLabelText(/^Launch arguments/)).toHaveValue("--address\n127.0.0.1:8080");

    await user.clear(screen.getByLabelText(/^Endpoint URL/));
    await user.clear(screen.getByLabelText(/^TTS model/));
    await user.clear(screen.getByLabelText(/^Launch arguments/));
    await user.type(screen.getByLabelText(/^Launch arguments/), "--address\n127.0.0.1:9090");
    await user.click(screen.getByRole("button", { name: "Save and check connection" }));

    await waitFor(() => expect(api.updateProvider).toHaveBeenCalledWith("provider-localai", expect.objectContaining({
      role: "tts",
      endpoint: null,
      model: null,
      executablePath: "/opt/localai/local-ai",
      workingDirectory: "/opt/localai",
      arguments: ["--address", "127.0.0.1:9090"],
    })));
  });

  it("requires confirmation and never offers to delete provider-owned resources", async () => {
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Settings" }));
    await user.click(screen.getByRole("button", { name: "Delete provider" }));

    expect(screen.getByText(/Deleting a profile does not remove its external resources/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Delete provider" }));
    await waitFor(() => expect(api.deleteProvider).toHaveBeenCalledWith("provider-localai"));
  });

  it("starts the pinned app-managed MLX-audio installation from the desktop UI", async () => {
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Install MLX-audio" }));
    await waitFor(() => expect(api.installMlx).toHaveBeenCalledOnce());
    expect(screen.getByText(/bundled uv 0.12.1/)).toBeInTheDocument();
  });

  it("blocks an incomplete offline payload and shows only safe installer diagnostics", async () => {
    vi.mocked(api.mlxManagement).mockResolvedValue({
      ...structuredClone(mlxManagement),
      supportDetail: "Managed installation is disabled. The complete bundled offline installer payload is unavailable.",
      installerStatus: "not_bundled",
      installerPayloadAvailable: false,
      lastOperation: {
        id: "operation-failed",
        kind: "install",
        state: "failed",
        progressPercent: 20,
        phase: "failed",
        message: "Only allowlisted, redacted diagnostics were retained.",
        exitCode: 23,
        diagnostics: ["The bundled artifact failed hash verification."],
        startedAt: new Date().toISOString(),
        finishedAt: new Date().toISOString(),
      },
    });
    renderProviders();

    expect(await screen.findByRole("button", { name: "Install MLX-audio" })).toBeDisabled();
    expect(screen.getByText(/does not include the verified offline MLX-audio installer/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Install MLX-audio" })).toHaveAttribute("aria-describedby", "mlx-installer-status");
    const configure = screen.getByRole("button", { name: "Configure isolated MLX-audio" });
    expect(configure).toBeEnabled();
    await userEvent.setup().click(screen.getByText("Safe installer diagnostics"));
    expect(screen.getByText("Installer exit code: 23")).toBeInTheDocument();
    expect(screen.getByText("The bundled artifact failed hash verification.")).toBeInTheDocument();
    await userEvent.setup().click(configure);
    expect(screen.getByLabelText("Deployment")).toHaveValue("managed_child");
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("http://127.0.0.1:8000/");
    expect(screen.getByLabelText(/^Launch arguments/)).toHaveValue("--host\n127.0.0.1\n--port\n8000");
  });

  it("requires confirmation before removing an app-owned MLX model", async () => {
    vi.mocked(api.mlxManagement).mockResolvedValue({
      ...structuredClone(mlxManagement),
      installed: true,
      installedVersion: "0.4.6",
      models: [{
        id: "model-owned",
        repository: "owner/public-model",
        revision: "main",
        localPath: "/app-owned/models/model-owned",
        state: "ready",
        bytes: 1024,
        createdAt: new Date().toISOString(),
      }],
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Remove owner/public-model" }));
    expect(screen.getByText(/Only the matching app-owned model directory/)).toBeInTheDocument();
    expect(api.removeMlxModel).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Remove model" }));
    await waitFor(() => expect(api.removeMlxModel).toHaveBeenCalledWith("model-owned", true));
  });

  it("requires confirmation before uninstalling the app-owned MLX runtime", async () => {
    vi.mocked(api.mlxManagement).mockResolvedValue({
      ...structuredClone(mlxManagement),
      installed: true,
      installedVersion: "0.4.6",
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Uninstall runtime" }));
    expect(api.uninstallMlx).not.toHaveBeenCalled();
    const dialog = screen.getByRole("dialog", { name: "Uninstall the app-managed MLX-audio runtime?" });
    expect(within(dialog).getByText(/Downloaded models are retained/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Uninstall runtime" }));
    await waitFor(() => expect(api.uninstallMlx).toHaveBeenCalledWith(true));
  });

  it("installs the app-managed Piper engine on a supported Linux system", async () => {
    vi.mocked(api.piperManagement).mockResolvedValue({
      ...structuredClone(piperManagement),
      supported: true,
      installerStatus: "ready",
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Install Piper" }));
    await waitFor(() => expect(api.installPiper).toHaveBeenCalledOnce());
  });

  it("opens an add-Piper-connection form from the installed engine card", async () => {
    vi.mocked(api.piperManagement).mockResolvedValue({
      ...structuredClone(piperManagement),
      supported: true,
      installerStatus: "ready",
      installed: true,
      installedVersion: "1.2.0",
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add Piper connection" }));
    expect(screen.getByLabelText("Choose a provider type")).toHaveValue("piper");
    expect(screen.getByLabelText("Deployment")).toHaveValue("native");
    expect(screen.queryByLabelText(/^API key/)).not.toBeInTheDocument();
  });

  it("requires confirmation before uninstalling Piper and can cancel an active operation", async () => {
    vi.mocked(api.piperManagement).mockResolvedValue({
      ...structuredClone(piperManagement),
      supported: true,
      installerStatus: "ready",
      installed: true,
      installedVersion: "1.2.0",
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Uninstall Piper" }));
    const dialog = screen.getByRole("dialog", { name: "Uninstall Piper?" });
    expect(api.uninstallPiper).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("button", { name: "Uninstall Piper" }));
    await waitFor(() => expect(api.uninstallPiper).toHaveBeenCalledWith(true));

    vi.mocked(api.piperManagement).mockResolvedValue({
      ...structuredClone(piperManagement),
      supported: true,
      installerStatus: "ready",
      activeOperation: { id: "piper-active", kind: "download_voice", state: "running", progressPercent: 50, phase: "download", message: "Downloading voice", voiceId: "de_DE-thorsten-medium", bytesDownloaded: 512, bytesTotal: 1024, startedAt: new Date().toISOString() },
    });
    const piperCard = screen.getByText("Piper local voices").closest("section")!;
    await user.click(within(piperCard).getByRole("button", { name: "Refresh" }));
    await user.click(await screen.findByRole("button", { name: "Cancel operation" }));
    await waitFor(() => expect(vi.mocked(api.cancelPiperOperation).mock.calls[0]?.[0]).toBe("piper-active"));
  });

  it("requires explicit license acceptance before downloading a curated Piper voice", async () => {
    vi.mocked(api.piperManagement).mockResolvedValue({
      ...structuredClone(piperManagement),
      supported: true,
      installerStatus: "ready",
      installed: true,
      installedVersion: "1.2.0",
      catalog: [{
        id: "de_DE-thorsten-medium",
        name: "Thorsten",
        language: "German",
        quality: "Medium",
        speakers: 1,
        sampleRate: 22_050,
        sizeBytes: 64 * 1024 * 1024,
        license: "Source dataset: CC0-1.0",
        licenseUrl: "https://example.test/license",
        licenseSummary: "The pinned model card declares the source dataset as CC0.",
        modelCardUrl: "https://example.test/model-card",
        sourceUrl: "https://example.test/source",
      }],
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Download" }));
    const dialog = screen.getByRole("dialog", { name: "Download Thorsten?" });
    expect(within(dialog).getByRole("button", { name: "Accept and download" })).toBeDisabled();
    expect(api.downloadPiperVoice).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("checkbox", { name: /reviewed the model card/ }));
    await user.click(within(dialog).getByRole("button", { name: "Accept and download" }));
    await waitFor(() => expect(api.downloadPiperVoice).toHaveBeenCalledWith("de_DE-thorsten-medium", true));
  });

  it("shows Piper voice issues and only offers recovery for app-owned files", async () => {
    vi.mocked(api.piperManagement).mockResolvedValue({
      ...structuredClone(piperManagement),
      supported: true,
      installerStatus: "ready",
      installed: true,
      installedVersion: "1.2.0",
      catalog: [
        { id: "de_DE-thorsten-medium", name: "Thorsten", language: "German", quality: "Medium", speakers: 1, sampleRate: 22_050, sizeBytes: 1024, license: "Source dataset: CC0-1.0", licenseUrl: "https://example.test/license", licenseSummary: "CC0 source dataset", modelCardUrl: "https://example.test/model-card", sourceUrl: "https://example.test/source" },
        { id: "en_GB-alba-medium", name: "Alba", language: "English", quality: "Medium", speakers: 1, sampleRate: 22_050, sizeBytes: 1024, license: "Source dataset: CC0-1.0", licenseUrl: "https://example.test/license", licenseSummary: "CC0 source dataset", modelCardUrl: "https://example.test/model-card", sourceUrl: "https://example.test/source" },
      ],
      voiceIssues: [
        { id: "de_DE-thorsten-medium", status: "incomplete", removable: true, detail: "The app-owned voice is incomplete and can be removed before downloading it again." },
        { id: "en_GB-alba-medium", status: "unsafe_filesystem", removable: false, detail: "The voice path is not owned by AudiobookAI and must be resolved manually." },
      ],
    });
    const user = userEvent.setup();
    renderProviders();

    expect(await screen.findByRole("heading", { name: "Voices needing attention" })).toBeInTheDocument();
    expect(screen.getByText("The app-owned voice is incomplete and can be removed before downloading it again.")).toBeInTheDocument();
    expect(screen.getByText("The voice path is not owned by AudiobookAI and must be resolved manually.")).toBeInTheDocument();
    expect(screen.getByText("Manual resolution required")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Download" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Remove incomplete files" }));
    const dialog = screen.getByRole("dialog", { name: "Remove Thorsten?" });
    expect(api.removePiperVoice).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("button", { name: "Remove voice" }));
    await waitFor(() => expect(api.removePiperVoice).toHaveBeenCalledWith("de_DE-thorsten-medium", true));
  });

  it("selects an installed Piper voice per connection and guards removal while in use", async () => {
    const piperOne: ProviderProfile = {
      ...structuredClone(managedProvider),
      id: "piper-one",
      name: "German narration",
      kind: "piper",
      mode: "native",
      model: "de_DE-thorsten-medium",
      executablePath: undefined,
      workingDirectory: undefined,
      arguments: [],
      status: "online",
    };
    const piperTwo = { ...structuredClone(piperOne), id: "piper-two", name: "English narration", model: "en_US-lessac-medium" };
    vi.mocked(api.providers).mockResolvedValue({ items: [piperOne, piperTwo], total: 2 });
    vi.mocked(api.piperManagement).mockResolvedValue({
      ...structuredClone(piperManagement),
      supported: true,
      installerStatus: "ready",
      installed: true,
      installedVersion: "1.2.0",
      installedVoices: [
        { id: "de_DE-thorsten-medium", name: "Thorsten", language: "German", quality: "Medium", modelPath: "/voices/thorsten.onnx", configPath: "/voices/thorsten.onnx.json", sizeBytes: 1024, license: "Source dataset: CC0-1.0", installedAt: "2026-08-18T00:00:00Z" },
        { id: "en_GB-alba-medium", name: "Alba", language: "English", quality: "Medium", modelPath: "/voices/alba.onnx", configPath: "/voices/alba.onnx.json", sizeBytes: 1024, license: "Source dataset: CC0-1.0", installedAt: "2026-08-18T00:00:00Z" },
      ],
    });
    const user = userEvent.setup();
    renderProviders();

    expect(await screen.findByRole("button", { name: "Uninstall Piper" })).toBeDisabled();
    expect(screen.getByText(/Delete every Piper connection before uninstalling/)).toBeInTheDocument();
    await user.click(screen.getAllByRole("button", { name: "Settings" })[0]);
    expect(screen.getByRole("button", { name: "Delete provider" })).toBeEnabled();
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(await screen.findByRole("button", { name: "Remove Thorsten" })).toBeDisabled();
    const albaRow = screen.getByText("Alba").closest("div")!.parentElement!;
    await user.click(within(albaRow).getByRole("button", { name: "Use voice" }));
    const useDialog = screen.getByRole("dialog", { name: "Use Alba" });
    await user.selectOptions(within(useDialog).getByLabelText("Piper connection"), "piper-two");
    await user.click(within(useDialog).getByRole("button", { name: "Use voice" }));
    await waitFor(() => expect(api.updateProvider).toHaveBeenCalledWith("piper-two", { model: "en_GB-alba-medium" }));

    await user.click(screen.getByRole("button", { name: "Remove Alba" }));
    const removeDialog = screen.getByRole("dialog", { name: "Remove Alba?" });
    expect(api.removePiperVoice).not.toHaveBeenCalled();
    await user.click(within(removeDialog).getByRole("button", { name: "Remove voice" }));
    await waitFor(() => expect(api.removePiperVoice).toHaveBeenCalledWith("en_GB-alba-medium", true));
  });

  it("blocks deletion while an app-owned child is online", async () => {
    vi.mocked(api.providers).mockResolvedValue({ items: [{ ...structuredClone(managedProvider), status: "online" }], total: 1 });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Settings" }));
    expect(screen.getByText("Stop this app-owned process before changing its launch configuration.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save and check connection" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Delete provider" }));

    expect(screen.getByText("Stop this app-owned provider process before deleting its profile.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete provider" })).toBeDisabled();
  });

  it("offers only LocalAI's documented model-library controls and confirms deletion", async () => {
    vi.mocked(api.providers).mockResolvedValue({
      items: [{
        ...structuredClone(managedProvider),
        model: "different-model",
        capabilities: {
          ...structuredClone(managedProvider.capabilities!),
          modelControl: true,
          modelList: true,
          modelDownload: true,
          modelDelete: true,
        },
      }],
      total: 1,
    });
    vi.mocked(api.providerModels).mockResolvedValue({
      models: [{ id: "voice-model", name: "voice-model", loadedInstances: [] }],
      operations: [],
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Control" }));
    expect(await screen.findByText(/never deletes LocalAI files directly/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Load model" })).not.toBeInTheDocument();
    await user.type(screen.getByLabelText(/^Model to download/), "localai@voice-model");
    await user.click(screen.getByRole("button", { name: "Download model" }));
    await waitFor(() => expect(api.downloadProviderModel).toHaveBeenCalledWith(
      managedProvider.id,
      "localai@voice-model",
      undefined,
    ));

    await user.click(await screen.findByRole("button", { name: "Delete voice-model" }));
    expect(api.deleteProviderModel).not.toHaveBeenCalled();
    const dialog = screen.getByRole("dialog", { name: "Delete voice-model from the provider?" });
    expect(within(dialog).getByText(/character voice assignment/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Delete model" }));
    await waitFor(() => expect(api.deleteProviderModel).toHaveBeenCalledWith(
      managedProvider.id,
      "voice-model",
      true,
    ));
  });
});
