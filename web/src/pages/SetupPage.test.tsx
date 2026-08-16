import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { AppSettings } from "../api/types";
import i18n from "../i18n";
import { SetupPage } from "./SetupPage";

const tauri = vi.hoisted(() => ({
  invoke: vi.fn(),
  isTauri: vi.fn(() => true),
}));
vi.mock("@tauri-apps/api/core", () => tauri);

const lockedSettings: AppSettings = {
  language: "en",
  theme: "system",
  libraryPath: "/data/library",
  cachePath: "/data/cache",
  cacheLimitBytes: 20_000_000_000,
  defaultConcurrency: 4,
  defaultRetryCount: 3,
  defaultLufs: -19,
  defaultTruePeakDb: -3,
  closeToTray: true,
  checkForUpdates: true,
  lan: {
    enabled: false,
    tls: false,
    insecureHttpConfirmed: false,
    bindAddress: "127.0.0.1",
    port: 8787,
    certificateChainPath: "",
    privateKeyPath: "",
    advertisedHosts: [],
    passwordConfigured: false,
    apiTokenCount: 0,
    activeSessions: 0,
    restartRequired: false,
  },
  secretStore: "locked",
  firstRunComplete: false,
};

function renderSetup() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={client}>
        <SetupPage />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

beforeEach(async () => {
  vi.restoreAllMocks();
  tauri.invoke.mockReset();
  tauri.invoke.mockResolvedValue(undefined);
  tauri.isTauri.mockReturnValue(true);
  await i18n.changeLanguage("en");
  vi.spyOn(api, "settings").mockResolvedValue(structuredClone(lockedSettings));
  vi.spyOn(api, "unlockSecretStore").mockResolvedValue({ unlocked: true, backend: "passphrase" });
  vi.spyOn(api, "discoverProviderModels").mockResolvedValue({ items: [] });
});

describe("first-run setup", () => {
  it("applies TTS and LLM presets while clearing transient credentials", async () => {
    const user = userEvent.setup();
    renderSetup();

    await user.click(await screen.findByRole("button", { name: "Set up AudiobookAI" }));
    await user.click(screen.getByRole("button", { name: /Use local and cloud providers/ }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.selectOptions(screen.getByLabelText("Provider"), "elevenlabs");
    const endpoint = await screen.findByLabelText(/^Endpoint URL/);
    expect(endpoint).toHaveValue("https://api.elevenlabs.io/");
    await user.clear(endpoint);
    await user.type(endpoint, "https://api.example.test");
    await user.click(screen.getByRole("switch", { name: "I’ll add credentials later" }));
    await user.type(screen.getByLabelText(/^API key/), "temporary-provider-credential");

    await user.selectOptions(screen.getByLabelText("Provider"), "openai_tts");
    expect(screen.getByText("Text to speech (TTS)")).toBeInTheDocument();
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("https://api.openai.com/");
    expect(screen.getByLabelText(/^TTS model/)).toHaveValue("gpt-4o-mini-tts");

    await user.selectOptions(screen.getByLabelText("Provider"), "ollama");

    expect(screen.getByText("Language model (LLM)")).toBeInTheDocument();
    expect(screen.getByLabelText(/^Endpoint URL/)).toHaveValue("http://127.0.0.1:11434/");
    expect(screen.getByRole("switch", { name: "I’ll add credentials later" })).not.toBeChecked();
    expect(screen.queryByLabelText(/^API key/)).not.toBeInTheDocument();
  });

  it("requires and verifies a passphrase in the wizard when the OS keychain is unavailable", async () => {
    const user = userEvent.setup();
    renderSetup();

    await user.click(await screen.findByRole("button", { name: "Set up AudiobookAI" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));

    expect(screen.getByRole("button", { name: "Continue" })).toBeDisabled();
    await user.type(screen.getByLabelText("AudiobookAI passphrase"), "a-long-vault-passphrase");
    await user.type(screen.getByLabelText("Confirm AudiobookAI passphrase"), "a-long-vault-passphrase");
    await user.click(screen.getByRole("button", { name: "Unlock secret store" }));

    await waitFor(() => expect(api.unlockSecretStore).toHaveBeenCalledWith("a-long-vault-passphrase"));
    expect(await screen.findByText("Protected by your AudiobookAI passphrase")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Continue" })).toBeEnabled();
  });

  it("chooses and applies a dedicated media root before setup completes", async () => {
    const user = userEvent.setup();
    const readySettings = { ...structuredClone(lockedSettings), secretStore: "keychain" as const };
    const configuredSettings = { ...readySettings, libraryPath: "/audiobooks/library", cachePath: "/audiobooks/cache" };
    vi.mocked(api.settings).mockResolvedValueOnce(readySettings).mockResolvedValue(configuredSettings);
    vi.spyOn(api, "updateSettings").mockResolvedValue(readySettings);
    vi.spyOn(api, "completeFirstRun").mockResolvedValue({ ...readySettings, firstRunComplete: true });
    tauri.invoke.mockImplementation(async (command: string) => command === "choose_storage_directory" ? "/audiobooks" : undefined);
    renderSetup();

    await user.click(await screen.findByRole("button", { name: "Set up AudiobookAI" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));

    expect(screen.getByLabelText("AudiobookAI media folder")).toHaveValue("/data");
    expect(screen.getByLabelText("AudiobookAI media folder")).not.toHaveAttribute("readonly");
    expect(screen.getByLabelText(/^Managed library/)).toHaveAttribute("readonly");
    expect(screen.getByLabelText(/^Audio cache/)).toHaveAttribute("readonly");

    await user.click(screen.getByRole("button", { name: "Browse…" }));
    expect(screen.getByLabelText("AudiobookAI media folder")).toHaveValue("/audiobooks");
    expect(screen.getByLabelText(/^Managed library/)).toHaveValue("/audiobooks/library");
    expect(screen.getByLabelText(/^Audio cache/)).toHaveValue("/audiobooks/cache");

    await user.click(screen.getByRole("button", { name: "Continue" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("configure_first_run_media_root", { mediaRoot: "/audiobooks" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Finish setup" }));

    await waitFor(() => expect(api.updateSettings).toHaveBeenCalledWith({ language: "en" }));
    expect(api.completeFirstRun).toHaveBeenCalled();
  });

  it("keeps storage read-only outside the desktop host", async () => {
    const user = userEvent.setup();
    tauri.isTauri.mockReturnValue(false);
    renderSetup();

    await user.click(await screen.findByRole("button", { name: "Set up AudiobookAI" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));

    expect(screen.getByLabelText("AudiobookAI media folder")).toHaveAttribute("readonly");
    expect(screen.queryByRole("button", { name: "Browse…" })).not.toBeInTheDocument();
    expect(screen.getByText("Media storage can only be changed from the AudiobookAI desktop app.")).toBeInTheDocument();
  });

  it("shows a storage preflight error returned as a Tauri string", async () => {
    const user = userEvent.setup();
    tauri.invoke.mockRejectedValueOnce("the new media folder must be empty");
    renderSetup();

    await user.click(await screen.findByRole("button", { name: "Set up AudiobookAI" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    const storageRoot = screen.getByLabelText("AudiobookAI media folder");
    await user.clear(storageRoot);
    await user.type(storageRoot, "/occupied");
    await user.click(screen.getByRole("button", { name: "Continue" }));

    expect(await screen.findByText("the new media folder must be empty")).toBeInTheDocument();
    expect(screen.queryByText("Unknown")).not.toBeInTheDocument();
  });
});
