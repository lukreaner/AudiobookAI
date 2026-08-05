import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import i18n from "../i18n";
import { DiagnosticsPage } from "./DiagnosticsPage";

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <DiagnosticsPage />
    </QueryClientProvider>,
  );
}

beforeEach(async () => {
  vi.restoreAllMocks();
  await i18n.changeLanguage("en");
  vi.spyOn(api, "diagnostics").mockResolvedValue({
    items: [{
      sequence: 42,
      timestamp: "2026-08-04T12:34:56.789Z",
      level: "warn",
      target: "audiobookai_service::http",
      message: "HTTP request completed",
      fields: { method: "POST", route: "/api/v1/jobs", status: 409, redactedFieldCount: 1 },
    }],
    total: 1,
    latestSequence: 42,
  });
  vi.spyOn(api, "downloadDiagnostics").mockResolvedValue(undefined);
});

describe("DiagnosticsPage", () => {
  it("shows detailed sanitized fields and the privacy boundary", async () => {
    renderPage();

    expect(await screen.findByText("HTTP request completed")).toBeInTheDocument();
    expect(screen.getByText("/api/v1/jobs")).toBeInTheDocument();
    expect(screen.getByText("No secrets, API tokens, book text, reference audio, headers, request bodies, or provider responses are collected.")).toBeInTheDocument();
    expect(screen.getByText("redactedFieldCount")).toBeInTheDocument();
  });

  it("applies filters and exports only the active sanitized view", async () => {
    const user = userEvent.setup();
    renderPage();
    await screen.findByText("HTTP request completed");

    await user.selectOptions(screen.getByRole("combobox", { name: "Minimum level" }), "error");
    await user.type(screen.getByLabelText(/^Component/), "conversion");
    await user.type(screen.getByLabelText(/^Search/), "failed");
    await user.click(screen.getByRole("button", { name: "Apply search" }));
    await waitFor(() => expect(api.diagnostics).toHaveBeenLastCalledWith({
      level: "error",
      target: "conversion",
      search: "failed",
      limit: 500,
    }));

    await user.click(screen.getByRole("button", { name: "Export sanitized logs" }));
    await waitFor(() => expect(api.downloadDiagnostics).toHaveBeenCalledWith({
      level: "error",
      target: "conversion",
      search: "failed",
      limit: 500,
    }));
  });
});
