import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { ProviderProfile, RateCard } from "../api/types";
import i18n from "../i18n";
import { UsagePage } from "./UsagePage";

const provider: ProviderProfile = {
  id: "00000000-0000-4000-8000-000000000001",
  name: "Local speech",
  kind: "localai",
  mode: "external_endpoint",
  status: "online",
  arguments: [],
  credentialConfigured: false,
  capabilities: {
    tts: true,
    characterDetection: false,
    streaming: false,
    voiceCloning: false,
    pronunciation: false,
    processControl: false,
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

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><UsagePage /></QueryClientProvider>);
}

beforeEach(async () => {
  vi.restoreAllMocks();
  await i18n.changeLanguage("en");
  vi.spyOn(api, "usage").mockResolvedValue({
    periodStart: "2026-08-01T00:00:00Z",
    periodEnd: "2026-09-01T00:00:00Z",
    unknownCostRequests: 0,
    rows: [],
  });
  vi.spyOn(api, "budgets").mockResolvedValue({ items: [] });
  vi.spyOn(api, "rateCards").mockResolvedValue({ items: [] });
  vi.spyOn(api, "providers").mockResolvedValue({ items: [provider] });
  vi.spyOn(api, "createRateCard").mockImplementation(async (input) => ({
    id: "00000000-0000-4000-8000-000000000002",
    effectiveAt: "2026-08-04T00:00:00Z",
    userOverridden: true,
    ...input,
  } as RateCard));
});

describe("usage pricing controls", () => {
  it("accepts an explicit zero-cost local TTS rate without confusing currency units and micros", async () => {
    const user = userEvent.setup();
    renderPage();

    await user.click(await screen.findByRole("button", { name: "Add rate card" }));
    await user.selectOptions(screen.getByLabelText("Provider scope"), provider.id);
    await user.type(screen.getByLabelText(/^Per 1,000 characters/), "0");
    await user.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() => expect(api.createRateCard).toHaveBeenCalled());
    expect(vi.mocked(api.createRateCard).mock.calls[0]?.[0]).toMatchObject({
      providerProfileId: provider.id,
      workload: "tts",
      currency: "EUR",
      pricing: { per_1000_characters_micros: 0 },
    });
  });
});
