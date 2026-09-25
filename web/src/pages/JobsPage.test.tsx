import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { Job } from "../api/types";
import i18n from "../i18n";
import { JobsPage, ProgressivePlayer } from "./JobsPage";

class FakeWebSocket {
  static instances: FakeWebSocket[] = [];

  binaryType = "blob";
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((event: { data: ArrayBuffer | string }) => void) | null = null;
  onopen: (() => void) | null = null;

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }

  close() {
    this.onclose?.();
  }
}

const postMessage = vi.fn();
const disconnect = vi.fn();
const closeAudioContext = vi.fn(async () => undefined);

class FakeAudioWorkletNode {
  port = { postMessage };
  connect = vi.fn();
  disconnect = disconnect;
}

class FakeAudioContext {
  audioWorklet = { addModule: vi.fn(async () => undefined) };
  destination = {};
  close = closeAudioContext;
  resume = vi.fn(async () => undefined);
}

describe("ProgressivePlayer", () => {
  beforeEach(async () => {
    vi.useFakeTimers();
    FakeWebSocket.instances = [];
    postMessage.mockClear();
    disconnect.mockClear();
    closeAudioContext.mockClear();
    Object.defineProperty(globalThis, "WebSocket", { configurable: true, value: FakeWebSocket });
    Object.defineProperty(globalThis, "AudioContext", { configurable: true, value: FakeAudioContext });
    Object.defineProperty(globalThis, "AudioWorkletNode", { configurable: true, value: FakeAudioWorkletNode });
    await i18n.changeLanguage("en");
  });

  afterEach(() => vi.useRealTimers());

  it("reconnects with backoff and keeps the audio worklet alive", async () => {
    render(<ProgressivePlayer jobId="job-42" />);

    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Progressive playback" })));
    expect(FakeWebSocket.instances).toHaveLength(1);
    expect(FakeWebSocket.instances[0].url).toContain("/api/v1/jobs/job-42/playback");

    act(() => FakeWebSocket.instances[0].onopen?.());
    act(() => FakeWebSocket.instances[0].onclose?.());
    expect(screen.getByText("Reconnecting to live audio…")).toBeInTheDocument();

    await act(async () => vi.advanceTimersByTime(499));
    expect(FakeWebSocket.instances).toHaveLength(1);
    await act(async () => vi.advanceTimersByTime(1));
    expect(FakeWebSocket.instances).toHaveLength(2);

    act(() => FakeWebSocket.instances[1].onopen?.());
    act(() => FakeWebSocket.instances[1].onmessage?.({ data: JSON.stringify({ type: "reset" }) }));
    expect(postMessage).toHaveBeenCalledWith({ type: "clear" });
  });

  it("cancels a pending reconnect when playback is stopped", async () => {
    render(<ProgressivePlayer jobId="job-42" />);

    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Progressive playback" })));
    act(() => FakeWebSocket.instances[0].onopen?.());
    act(() => FakeWebSocket.instances[0].onclose?.());
    fireEvent.click(screen.getByRole("button", { name: "Close" }));

    await act(async () => vi.advanceTimersByTime(30_000));
    expect(FakeWebSocket.instances).toHaveLength(1);
    expect(disconnect).toHaveBeenCalledOnce();
    expect(closeAudioContext).toHaveBeenCalledOnce();
  });

  it("stops retrying and releases audio resources after the bounded retry window", async () => {
    render(<ProgressivePlayer jobId="job-42" />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Progressive playback" })));

    const delays = [500, 1_000, 2_000, 4_000, 8_000, 8_000];
    for (const [index, delay] of delays.entries()) {
      act(() => FakeWebSocket.instances[index].onopen?.());
      act(() => FakeWebSocket.instances[index].onclose?.());
      await act(async () => vi.advanceTimersByTime(delay));
    }
    expect(FakeWebSocket.instances).toHaveLength(7);
    act(() => FakeWebSocket.instances[6].onopen?.());
    act(() => FakeWebSocket.instances[6].onclose?.());

    await act(async () => vi.advanceTimersByTime(60_000));
    expect(FakeWebSocket.instances).toHaveLength(7);
    expect(disconnect).toHaveBeenCalledOnce();
    expect(closeAudioContext).toHaveBeenCalledOnce();
    expect(screen.getByRole("button", { name: "Progressive playback" })).toBeInTheDocument();
  });
});

describe("job detail", () => {
  const runningJob: Job = {
    id: "job-7",
    projectId: "project-1",
    projectTitle: "The Example Book",
    kind: "conversion",
    status: "running",
    progress: 40,
    updatedAt: "2026-08-04T10:05:00Z",
    units: [],
    uncertainCharge: false,
  };

  class FakeEventSource {
    addEventListener = vi.fn();
    close = vi.fn();
  }

  function renderJob() {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
    return render(
      <QueryClientProvider client={client}>
        <MemoryRouter initialEntries={["/jobs/job-7"]}>
          <Routes>
            <Route path="/jobs/:id" element={<JobsPage />} />
            <Route path="/exports" element={<div>Exports opened</div>} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );
  }

  beforeEach(async () => {
    vi.restoreAllMocks();
    Object.defineProperty(globalThis, "EventSource", { configurable: true, value: FakeEventSource });
    await i18n.changeLanguage("en");
  });

  it("asks for confirmation before cancelling a job", async () => {
    vi.spyOn(api, "job").mockResolvedValue(runningJob);
    const jobAction = vi.spyOn(api, "jobAction").mockResolvedValue({ ...runningJob, status: "cancelling" });
    const user = userEvent.setup();
    renderJob();

    await user.click(await screen.findByRole("button", { name: "Cancel job" }));
    const dialog = screen.getByRole("dialog", { name: "Cancel this job?" });
    expect(jobAction).not.toHaveBeenCalled();
    await user.click(within(dialog).getByRole("button", { name: "Keep running" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(jobAction).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Cancel job" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Cancel job" }));
    await waitFor(() => expect(jobAction).toHaveBeenCalledWith("job-7", "cancel"));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("leads from a finished conversion to its exports", async () => {
    vi.spyOn(api, "job").mockResolvedValue({ ...runningJob, status: "complete", progress: 100 });
    const user = userEvent.setup();
    renderJob();

    expect(await screen.findByText("Job finished")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Cancel job" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("link", { name: "Open exports" }));
    expect(await screen.findByText("Exports opened")).toBeInTheDocument();
  });
});
