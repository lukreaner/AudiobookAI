import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { MlxManagement, ProviderProfile } from "../api/types";
import i18n from "../i18n";
import { ProvidersPage } from "./ProvidersPage";

const managedProvider: ProviderProfile = {
  id: "provider-localai",
  name: "LocalAI",
  kind: "localai",
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
  vi.spyOn(api, "mlxManagement").mockResolvedValue(structuredClone(mlxManagement));
  vi.spyOn(api, "installMlx").mockResolvedValue({ id: "operation-install", kind: "install", state: "queued", progressPercent: 0, phase: "queued", message: "Queued", startedAt: new Date().toISOString() });
  vi.spyOn(api, "uninstallMlx").mockResolvedValue({ id: "operation-uninstall", kind: "uninstall", state: "queued", progressPercent: 0, phase: "queued", message: "Queued", startedAt: new Date().toISOString() });
  vi.spyOn(api, "removeMlxModel").mockResolvedValue(undefined);
  vi.spyOn(api, "providerModels").mockResolvedValue({ models: [], operations: [] });
  vi.spyOn(api, "discoverProviderModels").mockResolvedValue({ items: [] });
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
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "ollama");

    await waitFor(() => expect(api.discoverProviderModels).toHaveBeenCalledWith(expect.objectContaining({
      kind: "ollama",
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
      model: "qwen3:8b",
    })));
  });

  it("offers OpenAI Speech as TTS with speech-only model choices", async () => {
    vi.mocked(api.discoverProviderModels).mockResolvedValue({
      items: [
        { id: "gpt-4o-mini-tts", name: "gpt-4o-mini-tts" },
        { id: "tts-1-hd", name: "tts-1-hd" },
      ],
    });
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "openai_tts");

    expect(screen.getByText("Text to speech (TTS)")).toBeInTheDocument();
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("https://api.openai.com/");
    expect(screen.getByLabelText(/^TTS model/)).toHaveValue("gpt-4o-mini-tts");
    await user.type(screen.getByLabelText(/^API key/), "test-openai-key");

    await waitFor(() => expect(api.discoverProviderModels).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: "openai_tts",
        mode: "cloud_remote",
        endpoint: "https://api.openai.com/",
      }),
    ));
    await waitFor(() => expect(screen.getByLabelText(/^TTS model/).tagName).toBe("SELECT"));
    expect(screen.getByRole("option", { name: "tts-1-hd" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Save and check connection" }));
    await waitFor(() => expect(api.createProvider).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: "openai_tts",
        model: "gpt-4o-mini-tts",
      }),
    ));
  });

  it("separates configured TTS providers from LLM providers", async () => {
    renderProviders();

    const ttsSection = await screen.findByRole("region", { name: "Text-to-speech providers" });
    const llmSection = screen.getByRole("region", { name: "LLM providers" });
    expect(screen.getByText("Automatic model detection")).toBeInTheDocument();
    expect(within(ttsSection).getByText("LocalAI")).toBeInTheDocument();
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
    expect(within(ttsSection).getByText("System voices · no model catalog")).toBeInTheDocument();
    await user.click(within(ttsSection).getByRole("button", { name: "Settings" }));
    expect(screen.getByText("This native provider uses system voices and does not expose a model catalog.")).toBeInTheDocument();
    expect(screen.queryByLabelText(/^TTS model/)).not.toBeInTheDocument();
    expect(api.discoverProviderModels).not.toHaveBeenCalled();
  });

  it("applies working presets and discards transient credentials when provider type changes", async () => {
    const user = userEvent.setup();
    renderProviders();

    await user.click(await screen.findByRole("button", { name: "Add provider" }));
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("https://api.elevenlabs.io/");
    await user.clear(screen.getByLabelText(/^Endpoint URL/));
    await user.type(screen.getByLabelText(/^Endpoint URL/), "https://api.example.test");
    await user.type(screen.getByLabelText(/^API key/), "temporary-provider-credential");
    await user.selectOptions(screen.getByLabelText("Choose a provider type"), "ollama");

    expect(screen.getByText("Language model (LLM)")).toBeInTheDocument();
    expect(screen.getByLabelText("Connection mode")).toHaveValue("external_endpoint");
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

    expect(screen.getByText("Deleting a profile does not remove its external resources or the app-managed MLX-audio runtime and models. External processes are never stopped.")).toBeInTheDocument();
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
    expect(screen.getByLabelText("Connection mode")).toHaveValue("managed_child");
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
