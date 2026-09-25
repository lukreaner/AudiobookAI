import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
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

type DragDropHandler = (event: { payload: { type: string; paths?: string[] } }) => void;
const dragDrop = vi.hoisted(() => ({ handler: undefined as DragDropHandler | undefined }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: async (handler: DragDropHandler) => {
      dragDrop.handler = handler;
      return () => { dragDrop.handler = undefined; };
    },
  }),
}));

function ImportProbe() {
  const location = useLocation();
  return <p>Import opened {(location.state as { sourcePath?: string } | null)?.sourcePath ?? "without file"}</p>;
}

function renderShell() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/library"]}>
        <Routes>
          <Route element={<AppShell />}>
            <Route path="/library" element={<p>Library content</p>} />
            <Route path="/import" element={<ImportProbe />} />
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

describe("shell conveniences", () => {
  it("remembers the collapsed navigation", async () => {
    const user = userEvent.setup();
    const first = renderShell();
    await user.click((await screen.findAllByRole("button", { name: "Collapse navigation" }))[0]);
    first.unmount();

    renderShell();
    expect((await screen.findAllByRole("button", { name: "Expand navigation" }))[0]).toHaveAttribute("aria-expanded", "false");
  });

  it("opens the import page with Ctrl+O", async () => {
    renderShell();
    await screen.findByText("Library content");

    fireEvent.keyDown(window, { key: "o", ctrlKey: true });
    expect(await screen.findByText("Import opened without file")).toBeInTheDocument();
  });

  it("imports an EPUB dropped anywhere on the desktop window", async () => {
    renderShell();
    await waitFor(() => expect(dragDrop.handler).toBeDefined());

    act(() => dragDrop.handler?.({ payload: { type: "enter", paths: ["/books/notes.txt"] } }));
    expect(screen.getByText("Only EPUB files can be imported")).toBeInTheDocument();
    act(() => dragDrop.handler?.({ payload: { type: "leave" } }));
    expect(screen.queryByText("Only EPUB files can be imported")).not.toBeInTheDocument();

    act(() => dragDrop.handler?.({ payload: { type: "enter", paths: ["/books/Story.EPUB"] } }));
    expect(screen.getByText("Drop to import this EPUB")).toBeInTheDocument();
    act(() => dragDrop.handler?.({ payload: { type: "drop", paths: ["/books/notes.txt", "/books/Story.EPUB"] } }));

    expect(await screen.findByText("Import opened /books/Story.EPUB")).toBeInTheDocument();
    expect(screen.queryByText("Drop to import this EPUB")).not.toBeInTheDocument();
  });
});
