import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { AppSettings } from "../api/types";
import i18n from "../i18n";
import { SettingsPage } from "./SettingsPage";

const settings: AppSettings = {
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
    enabled: true,
    tls: true,
    insecureHttpConfirmed: false,
    bindAddress: "0.0.0.0",
    port: 8787,
    certificateChainPath: "/certs/fullchain.pem",
    privateKeyPath: "/certs/private-key.pem",
    advertisedHosts: ["reader.home.arpa"],
    passwordConfigured: true,
    apiTokenCount: 0,
    activeSessions: 0,
    restartRequired: true,
  },
  secretStore: "keychain",
  firstRunComplete: true,
};

function renderSettings() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return {
    client,
    ...render(
    <QueryClientProvider client={client}>
      <SettingsPage />
    </QueryClientProvider>,
    ),
  };
}

beforeEach(async () => {
  vi.restoreAllMocks();
  await i18n.changeLanguage("en");
  vi.spyOn(api, "settings").mockResolvedValue(structuredClone(settings));
  vi.spyOn(api, "updateSettings").mockImplementation(async (value) => ({ ...structuredClone(settings), ...value } as AppSettings));
  vi.spyOn(api, "lanTokens").mockResolvedValue([]);
  vi.spyOn(api, "setLanPassword").mockResolvedValue(undefined);
  vi.spyOn(api, "createLanToken").mockResolvedValue({
    id: "token-1",
    label: "Living-room player",
    token: "one-time-token-value",
    createdAt: "2026-08-04T10:00:00Z",
  });
  vi.spyOn(api, "revokeLanToken").mockResolvedValue(undefined);
  vi.spyOn(api, "revokeLanSessions").mockResolvedValue(undefined);
});

describe("authenticated LAN settings", () => {
  it("exposes persisted TLS identity and advertised-host settings", async () => {
    const user = userEvent.setup();
    renderSettings();
    await user.click(await screen.findByRole("tab", { name: "Security & LAN" }));

    expect(screen.getByRole("switch", { name: "Authenticated LAN access" })).toBeChecked();
    expect(screen.getByRole("switch", { name: "Require TLS" })).toBeChecked();
    expect(await screen.findByLabelText(/^Certificate chain PEM/)).toHaveValue("/certs/fullchain.pem");
    expect(screen.getByLabelText(/^Private key PEM/)).toHaveValue("/certs/private-key.pem");
    expect(screen.getByLabelText(/^Advertised hosts/)).toHaveValue("reader.home.arpa");
    expect(screen.getByText("Restart required")).toBeInTheDocument();

    await user.clear(screen.getByLabelText(/^Advertised hosts/));
    await user.type(screen.getByLabelText(/^Advertised hosts/), "books.home.arpa");
    await user.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() => expect(api.updateSettings).toHaveBeenCalled());
    expect(vi.mocked(api.updateSettings).mock.calls.at(-1)?.[0].lan?.advertisedHosts).toEqual(["books.home.arpa"]);
  });

  it("sends a matching password only to the dedicated verifier endpoint", async () => {
    const user = userEvent.setup();
    renderSettings();
    await user.click(await screen.findByRole("tab", { name: "Security & LAN" }));
    await user.type(screen.getByLabelText("New LAN password"), "a-long-test-password");
    await user.type(screen.getByLabelText("Confirm password"), "a-long-test-password");
    await user.click(screen.getByRole("button", { name: "Replace LAN password" }));

    await waitFor(() => expect(api.setLanPassword).toHaveBeenCalledWith("a-long-test-password"));
    expect(screen.getByLabelText("New LAN password")).toHaveValue("");
    expect(api.updateSettings).not.toHaveBeenCalled();
  });

  it("shows a newly issued API token exactly in the one-time response", async () => {
    const user = userEvent.setup();
    const { client } = renderSettings();
    await user.click(await screen.findByRole("tab", { name: "Security & LAN" }));
    await user.type(screen.getByLabelText("Token label"), "Living-room player");
    await user.click(screen.getByRole("button", { name: "Create token" }));

    expect(await screen.findByText("one-time-token-value")).toBeInTheDocument();
    expect(screen.getByText("Copy this token now. It cannot be shown again.")).toBeInTheDocument();
    expect(api.createLanToken).toHaveBeenCalledWith("Living-room player");

    await user.click(screen.getByRole("button", { name: "Close" }));
    expect(screen.queryByText("one-time-token-value")).not.toBeInTheDocument();
    await waitFor(() => {
      const cached = JSON.stringify(client.getMutationCache().getAll().map((entry) => entry.state.data));
      expect(cached).not.toContain("one-time-token-value");
    });
  });

  it("unlocks the passphrase fallback without persisting the entered passphrase in settings", async () => {
    const user = userEvent.setup();
    vi.mocked(api.settings).mockResolvedValue({ ...structuredClone(settings), secretStore: "locked" });
    vi.spyOn(api, "unlockSecretStore").mockResolvedValue({ unlocked: true, backend: "passphrase" });
    renderSettings();
    await user.click(await screen.findByRole("tab", { name: "Security & LAN" }));

    await user.type(screen.getByLabelText("AudiobookAI passphrase"), "a-long-vault-passphrase");
    await user.type(screen.getByLabelText("Confirm AudiobookAI passphrase"), "a-long-vault-passphrase");
    await user.click(screen.getByRole("button", { name: "Unlock secret store" }));

    await waitFor(() => expect(api.unlockSecretStore).toHaveBeenCalledWith("a-long-vault-passphrase"));
    expect(await screen.findByText("Protected by your AudiobookAI passphrase")).toBeInTheDocument();
    expect(api.updateSettings).not.toHaveBeenCalled();
  });
});

describe("durable application defaults", () => {
  it("saves audio and job defaults while omitting managed paths", async () => {
    const user = userEvent.setup();
    renderSettings();
    await user.click(await screen.findByRole("tab", { name: "Audio defaults" }));

    fireEvent.change(screen.getByLabelText(/^Target loudness/), { target: { value: "-18.5" } });
    fireEvent.change(screen.getByLabelText(/^True peak ceiling/), { target: { value: "-2.5" } });
    await user.clear(screen.getByLabelText(/^Default parallel chapters/));
    await user.type(screen.getByLabelText(/^Default parallel chapters/), "7");
    await user.clear(screen.getByLabelText(/^Default transient retries/));
    await user.type(screen.getByLabelText(/^Default transient retries/), "0");
    await user.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(api.updateSettings).toHaveBeenCalled());
    const patch = vi.mocked(api.updateSettings).mock.calls.at(-1)?.[0];
    expect(patch).toMatchObject({
      defaultLufs: -18.5,
      defaultTruePeakDb: -2.5,
      defaultConcurrency: 7,
      defaultRetryCount: 0,
    });
    expect(patch).not.toHaveProperty("libraryPath");
    expect(patch).not.toHaveProperty("cachePath");
  });

  it("shows managed storage paths as read-only and persists the cache limit", async () => {
    const user = userEvent.setup();
    renderSettings();
    await user.click(await screen.findByRole("tab", { name: "Storage & cache" }));

    expect(screen.getByLabelText(/^Managed library/)).toHaveAttribute("readonly");
    expect(screen.getByLabelText(/^Audio cache/)).toHaveAttribute("readonly");
    await user.clear(screen.getByLabelText(/^Cache limit/));
    await user.type(screen.getByLabelText(/^Cache limit/), "12");
    await user.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(api.updateSettings).toHaveBeenCalled());
    expect(vi.mocked(api.updateSettings).mock.calls.at(-1)?.[0]).toMatchObject({
      cacheLimitBytes: 12_000_000_000,
    });
  });
});
