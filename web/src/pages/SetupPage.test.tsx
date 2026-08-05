import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { AppSettings } from "../api/types";
import i18n from "../i18n";
import { SetupPage } from "./SetupPage";

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
  await i18n.changeLanguage("en");
  vi.spyOn(api, "settings").mockResolvedValue(structuredClone(lockedSettings));
  vi.spyOn(api, "unlockSecretStore").mockResolvedValue({ unlocked: true, backend: "passphrase" });
});

describe("first-run secret-store fallback", () => {
  it("clears an entered endpoint and credential when the provider choice changes", async () => {
    const user = userEvent.setup();
    renderSetup();

    await user.click(await screen.findByRole("button", { name: "Set up AudiobookAI" }));
    await user.click(screen.getByRole("button", { name: /Use local and cloud providers/ }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.selectOptions(screen.getByLabelText("Provider"), "elevenlabs");
    await user.type(screen.getByLabelText("Endpoint URL"), "https://api.example.test");
    await user.click(screen.getByRole("switch", { name: "I’ll add credentials later" }));
    await user.type(screen.getByLabelText(/^API key/), "temporary-provider-credential");

    await user.selectOptions(screen.getByLabelText("Provider"), "localai");

    expect(screen.getByLabelText("Endpoint URL")).toHaveValue("");
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

  it("shows managed paths without submitting relocation edits", async () => {
    const user = userEvent.setup();
    const readySettings = { ...structuredClone(lockedSettings), secretStore: "keychain" as const };
    vi.mocked(api.settings).mockResolvedValue(readySettings);
    vi.spyOn(api, "updateSettings").mockResolvedValue(readySettings);
    vi.spyOn(api, "completeFirstRun").mockResolvedValue({ ...readySettings, firstRunComplete: true });
    renderSetup();

    await user.click(await screen.findByRole("button", { name: "Set up AudiobookAI" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));

    expect(screen.getByLabelText(/^Managed library/)).toHaveValue("/data/library");
    expect(screen.getByLabelText(/^Managed library/)).toHaveAttribute("readonly");
    expect(screen.getByLabelText(/^Audio cache/)).toHaveValue("/data/cache");
    expect(screen.getByLabelText(/^Audio cache/)).toHaveAttribute("readonly");

    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.click(screen.getByRole("button", { name: "Finish setup" }));

    await waitFor(() => expect(api.updateSettings).toHaveBeenCalledWith({ language: "en" }));
    expect(api.completeFirstRun).toHaveBeenCalled();
  });
});
