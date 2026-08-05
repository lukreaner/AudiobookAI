import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import i18n from "../i18n";
import { AppShell } from "./AppShell";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));

function renderShell() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/library"]}>
        <Routes>
          <Route element={<AppShell />}>
            <Route path="/library" element={<p>Library content</p>} />
          </Route>
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(async () => {
  vi.restoreAllMocks();
  await i18n.changeLanguage("en");
  vi.mocked(isTauri).mockReturnValue(true);
  vi.mocked(invoke).mockResolvedValue(undefined);
  vi.spyOn(api, "health").mockResolvedValue({ status: "ready", version: "0.1.0", database: "ready" });
});

describe("desktop quit control", () => {
  it("confirms and invokes the native owned-process shutdown path", async () => {
    const user = userEvent.setup();
    renderShell();

    await user.click(await screen.findByRole("button", { name: "Quit" }));
    const dialog = screen.getByRole("dialog", { name: "Quit AudiobookAI?" });
    expect(within(dialog).getByText(/Processes started outside AudiobookAI are never terminated/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Quit AudiobookAI" }));

    await waitFor(() => expect(invoke).toHaveBeenCalledOnce());
    expect(invoke).toHaveBeenCalledWith("quit_application");
  });

  it("does not expose host shutdown to browser or LAN sessions", async () => {
    vi.mocked(isTauri).mockReturnValue(false);
    renderShell();

    expect(await screen.findByText("Library content")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Quit" })).not.toBeInTheDocument();
  });

  it("reports a rejected native shutdown request", async () => {
    vi.mocked(invoke).mockRejectedValue(new Error("synthetic rejection"));
    const user = userEvent.setup();
    renderShell();

    await user.click(await screen.findByRole("button", { name: "Quit" }));
    const dialog = screen.getByRole("dialog", { name: "Quit AudiobookAI?" });
    await user.click(within(dialog).getByRole("button", { name: "Quit AudiobookAI" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent("could not begin a clean shutdown");
    expect(within(dialog).getByRole("button", { name: "Quit AudiobookAI" })).toBeEnabled();
  });
});
