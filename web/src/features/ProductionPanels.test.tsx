import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError, api } from "../api/client";
import type { DistributionMetadata, ProofingSegmentView, ProofingSummary, ProviderProfile } from "../api/types";
import i18n from "../i18n";
import { DistributionPanel } from "./DistributionPanel";
import { ProofingWorkbench } from "./ProofingWorkbench";
import { VoiceAuditionPanel } from "./VoiceAuditionPanel";

function renderPanel(node: React.ReactNode) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return { ...render(<QueryClientProvider client={client}><MemoryRouter>{node}</MemoryRouter></QueryClientProvider>), queryClient: client };
}

const provider = {
  id: "provider-1", name: "Local TTS", kind: "localai", role: "tts", mode: "external_endpoint", arguments: [], status: "online", model: "tts-model", credentialConfigured: false,
  capabilityUpdatedAt: new Date().toISOString(),
  capabilities: { tts: true, characterDetection: false, streaming: false, voiceCloning: false, pronunciation: false, processControl: false, modelControl: false, modelList: false, modelDownload: false, modelDelete: false, modelLoad: false, modelUnload: false, modelSwitch: false, temperature: "unsupported", reasoning: [], modelPerformance: [{ model: "tts-model", performance: { speed: { minimum: 0.5, maximum: 2 }, pitch: null, stability: null, similarity: null, style: null, speaker_boost: false, delivery_cues: [] } }] },
} as ProviderProfile;

const summary: ProofingSummary = {
  available: true,
  requiresNewConversion: false,
  plan: { project_id: "project-1", source_conversion_job_id: "job-source", plan_revision: 2, plan_hash: "hash", status: "ready", dirty_reasons: [], created_at: "2026-08-05T08:00:00Z", updated_at: "2026-08-05T08:00:00Z" },
  counts: { total: 1, unreviewed: 1, flagged: 0, approved: 0, locked: 0, stale: 0, missing: 0 },
  chapters: [{ id: "chapter-1", title: "Chapter one", total: 1, issueCount: 0 }],
  retailerExportReady: false,
  genericExportReady: true,
};

const segmentView = {
  segment: {
    id: "segment-1", project_id: "project-1", chapter_id: "chapter-1", paragraph_id: "paragraph-1", source: "epub_range", stable_key: "chapter-1:0", ordinal: 0, source_content_hash: "source", speaker: { kind: "narrator" }, original_text: "A short line.", effective_text: "A short line.", performance_override: {}, timing_override: {}, expected_input_hash: "semantic", review_state: "unreviewed", active: true, revision: 4, created_at: "2026-08-05T08:00:00Z", updated_at: "2026-08-05T08:00:00Z",
  },
  selection: { segment_id: "segment-1", take_id: "take-1", selected_at: "2026-08-05T08:00:00Z", revision: 3 },
  selectedTake: { id: "take-1", segment_id: "segment-1", artifact_id: "artifact-1", ordinal: 0, source_job_id: "job-source", source_job_unit_id: "unit-1", semantic_input_hash: "semantic", duration_ms: 1400, dictionary_revision_hash: "dict", normalization_version: "v1", synthesis_provenance: {}, findings: [], created_at: "2026-08-05T08:00:00Z" },
  takeCount: 1,
  selectedTakeCurrent: true,
  audioUrl: "/api/v1/artifacts/artifact-1",
} as ProofingSegmentView;

function mockProofingContext() {
  vi.spyOn(api, "providers").mockResolvedValue({ items: [provider], total: 1 });
  vi.spyOn(api, "characters").mockResolvedValue({
    items: [{
      id: "narrator-1", role: "narrator", canonicalName: "Book narrator", aliases: [], confidence: 1, dialogueCount: 0, evidence: [],
      voiceAssignment: { providerProfileId: provider.id, providerName: provider.name, voiceId: "voice-1", voiceName: "Warm", model: "tts-model", performance: {}, timing: {} },
    }],
    total: 1,
    characterRevision: 1,
  });
}

beforeEach(async () => {
  vi.restoreAllMocks();
  await i18n.changeLanguage("en");
});

describe("voice auditions", () => {
  it("requires explicit billing confirmation and sends one validated candidate", async () => {
    const user = userEvent.setup();
    vi.spyOn(api, "providers").mockResolvedValue({ items: [provider], total: 1 });
    vi.spyOn(api, "voices").mockResolvedValue({ items: [{ id: "voice-1", providerProfileId: provider.id, name: "Warm", kind: "catalog", owned: false }], total: 1 });
    vi.spyOn(api, "characters").mockResolvedValue({ items: [], total: 0, characterRevision: 0 });
    vi.spyOn(api, "voiceAuditions").mockResolvedValue({ potentiallyBillable: true, results: [] });
    renderPanel(<VoiceAuditionPanel projectId="project-1" />);

    await user.selectOptions(await screen.findByLabelText("Provider"), provider.id);
    await user.selectOptions(screen.getByLabelText("Voice"), "voice-1");
    const run = screen.getByRole("button", { name: "Run voice comparison" });
    expect(run).toBeDisabled();
    await user.click(screen.getByRole("switch", { name: /I understand every candidate/ }));
    await user.click(run);

    await waitFor(() => expect(api.voiceAuditions).toHaveBeenCalledWith("project-1", expect.objectContaining({
      confirmBillable: true,
      candidates: [expect.objectContaining({ providerProfileId: provider.id, voiceId: "voice-1", performance: {} })],
    }), expect.any(String)));
  });

  it("reuses one idempotency key only for an explicitly reconfirmed failed submission", async () => {
    const user = userEvent.setup();
    vi.spyOn(api, "providers").mockResolvedValue({ items: [provider], total: 1 });
    vi.spyOn(api, "voices").mockResolvedValue({ items: [{ id: "voice-1", providerProfileId: provider.id, name: "Warm", kind: "catalog", owned: false }], total: 1 });
    vi.spyOn(api, "characters").mockResolvedValue({ items: [], total: 0, characterRevision: 0 });
    const submit = vi.spyOn(api, "voiceAuditions")
      .mockRejectedValueOnce(new Error("provider unavailable"))
      .mockResolvedValueOnce({ potentiallyBillable: true, results: [] });
    renderPanel(<VoiceAuditionPanel projectId="project-1" />);

    await user.selectOptions(await screen.findByLabelText("Provider"), provider.id);
    await user.selectOptions(screen.getByLabelText("Voice"), "voice-1");
    await user.click(screen.getByRole("switch", { name: /I understand every candidate/ }));
    await user.click(screen.getByRole("button", { name: "Run voice comparison" }));
    await screen.findByText("provider unavailable");
    expect(screen.queryByRole("button", { name: "Try again" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("switch", { name: /I understand every candidate/ }));
    await user.click(screen.getByRole("button", { name: "Run voice comparison" }));
    await waitFor(() => expect(submit).toHaveBeenCalledTimes(2));
    expect(submit.mock.calls[1]?.[2]).toBe(submit.mock.calls[0]?.[2]);
  });

  it("invalidates confirmation and rotates the idempotency key when request input changes", async () => {
    const user = userEvent.setup();
    vi.spyOn(api, "providers").mockResolvedValue({ items: [provider], total: 1 });
    vi.spyOn(api, "voices").mockResolvedValue({ items: [{ id: "voice-1", providerProfileId: provider.id, name: "Warm", kind: "catalog", owned: false }], total: 1 });
    vi.spyOn(api, "characters").mockResolvedValue({ items: [], total: 0, characterRevision: 0 });
    const submit = vi.spyOn(api, "voiceAuditions")
      .mockRejectedValueOnce(new Error("provider unavailable"))
      .mockResolvedValueOnce({ potentiallyBillable: true, results: [] });
    renderPanel(<VoiceAuditionPanel projectId="project-1" />);

    await user.selectOptions(await screen.findByLabelText("Provider"), provider.id);
    await user.selectOptions(screen.getByLabelText("Voice"), "voice-1");
    const confirmation = screen.getByRole("switch", { name: /I understand every candidate/ });
    await user.click(confirmation);
    await user.click(screen.getByRole("button", { name: "Run voice comparison" }));
    await screen.findByText("provider unavailable");

    await user.type(screen.getByLabelText(/^Audition passage/), "A changed passage");
    expect(confirmation).not.toBeChecked();
    await user.click(confirmation);
    await user.click(screen.getByRole("button", { name: "Run voice comparison" }));
    await waitFor(() => expect(submit).toHaveBeenCalledTimes(2));
    expect(submit.mock.calls[1]?.[2]).not.toBe(submit.mock.calls[0]?.[2]);
  });
});

describe("proofing workbench", () => {
  it("saves typed narration, performance, and local timing with revision guards", async () => {
    const user = userEvent.setup();
    mockProofingContext();
    vi.spyOn(api, "proofingSummary").mockResolvedValue(summary);
    vi.spyOn(api, "proofingSegments").mockResolvedValue({ items: [segmentView], total: 1 });
    vi.spyOn(api, "updateProofingSegment").mockResolvedValue(segmentView);
    vi.spyOn(api, "startProofingExport").mockResolvedValue({} as never);
    renderPanel(<ProofingWorkbench projectId="project-1" />);

    const segmentToggle = await screen.findByRole("button", { expanded: false, name: /A short line\./ });
    await user.click(segmentToggle);
    await screen.findByRole("button", { expanded: true, name: /A short line\./ });
    const narration = await screen.findByPlaceholderText("A short line.");
    await user.type(narration, "A revised line.");
    await user.type(screen.getByLabelText("Speed"), "1.1");
    await user.type(screen.getByLabelText("Pause before"), "250");
    await user.click(screen.getByRole("button", { name: "Save segment overrides" }));

    await waitFor(() => expect(api.updateProofingSegment).toHaveBeenCalledWith("project-1", "segment-1", {
      expectedRevision: 4,
      textOverride: "A revised line.",
      clearTextOverride: false,
      performanceOverride: { speed: 1.1 },
      timingOverride: { pause_before_ms: 250 },
    }));
  });

  it("preserves existing performance overrides when capability lookup has not completed", async () => {
    const user = userEvent.setup();
    vi.spyOn(api, "providers").mockImplementation(() => new Promise(() => undefined));
    vi.spyOn(api, "characters").mockResolvedValue({
      items: [{
        id: "narrator-1", role: "narrator", canonicalName: "Book narrator", aliases: [], confidence: 1, dialogueCount: 0, evidence: [],
        voiceAssignment: { providerProfileId: provider.id, providerName: provider.name, voiceId: "voice-1", voiceName: "Warm", model: "tts-model", performance: {}, timing: {} },
      }],
      total: 1,
      characterRevision: 1,
    });
    const directedView = {
      ...segmentView,
      segment: { ...segmentView.segment, performance_override: { speed: 1.1 } },
    } as ProofingSegmentView;
    vi.spyOn(api, "proofingSummary").mockResolvedValue(summary);
    vi.spyOn(api, "proofingSegments").mockResolvedValue({ items: [directedView], total: 1 });
    vi.spyOn(api, "updateProofingSegment").mockResolvedValue(directedView);
    vi.spyOn(api, "startProofingExport").mockResolvedValue({} as never);
    renderPanel(<ProofingWorkbench projectId="project-1" />);

    await user.click(await screen.findByRole("button", { expanded: false, name: /A short line\./ }));
    expect(screen.getByText("Checking the assigned provider model…")).toBeInTheDocument();
    await user.type(screen.getByPlaceholderText("A short line."), "A revised line.");
    await user.click(screen.getByRole("button", { name: "Save segment overrides" }));

    await waitFor(() => expect(api.updateProofingSegment).toHaveBeenCalledWith("project-1", "segment-1", expect.objectContaining({
      expectedRevision: 4,
      textOverride: "A revised line.",
      performanceOverride: { speed: 1.1 },
    })));
  });

  it("preserves a dirty segment across a newer refetch and ignores an older save response", async () => {
    const user = userEvent.setup();
    mockProofingContext();
    vi.spyOn(api, "proofingSummary").mockResolvedValue(summary);
    const concurrentView = {
      ...segmentView,
      segment: {
        ...segmentView.segment,
        narration_text_override: "Concurrent revision",
        effective_text: "Concurrent revision",
        revision: 6,
      },
    } as ProofingSegmentView;
    const oldSaveResponse = {
      ...segmentView,
      segment: {
        ...segmentView.segment,
        narration_text_override: "Older save response",
        effective_text: "Older save response",
        revision: 5,
      },
    } as ProofingSegmentView;
    vi.spyOn(api, "proofingSegments")
      .mockResolvedValueOnce({ items: [segmentView], total: 1 })
      .mockResolvedValue({ items: [concurrentView], total: 1 });
    let resolveSave!: (view: ProofingSegmentView) => void;
    const saveResponse = new Promise<ProofingSegmentView>((resolve) => { resolveSave = resolve; });
    const update = vi.spyOn(api, "updateProofingSegment").mockReturnValue(saveResponse);
    vi.spyOn(api, "startProofingExport").mockResolvedValue({} as never);
    const { queryClient } = renderPanel(<ProofingWorkbench projectId="project-1" />);

    await user.click(await screen.findByRole("button", { expanded: false, name: /A short line\./ }));
    const narration = screen.getByRole("textbox", { name: "Narration text override" });
    await user.type(narration, "My preserved draft");
    await user.click(screen.getByRole("button", { name: "Save segment overrides" }));
    await waitFor(() => expect(update).toHaveBeenCalledWith("project-1", "segment-1", expect.objectContaining({
      expectedRevision: 4,
      textOverride: "My preserved draft",
    })));

    await queryClient.invalidateQueries({ queryKey: ["proofing", "project-1", "segments"] });
    expect(await screen.findByText("A newer segment revision exists")).toBeInTheDocument();
    expect(narration).toHaveValue("My preserved draft");

    act(() => resolveSave(oldSaveResponse));
    await waitFor(() => expect(screen.getByText("A newer segment revision exists")).toBeInTheDocument());
    expect(narration).toHaveValue("My preserved draft");
    expect(narration).not.toHaveValue("Older save response");

    await user.click(screen.getByRole("button", { name: "Discard draft and reload" }));
    await waitFor(() => expect(narration).toHaveValue("Concurrent revision"));
  });

  it("blocks estimates and exports while a segment has unsaved edits", async () => {
    const user = userEvent.setup();
    mockProofingContext();
    vi.spyOn(api, "proofingSummary").mockResolvedValue(summary);
    vi.spyOn(api, "proofingSegments").mockResolvedValue({ items: [segmentView], total: 1 });
    vi.spyOn(api, "proofingRegenerationEstimate").mockResolvedValue({} as never);
    vi.spyOn(api, "startProofingExport").mockResolvedValue({} as never);
    renderPanel(<ProofingWorkbench projectId="project-1" />);

    await user.click(await screen.findByRole("button", { expanded: false, name: /A short line\./ }));
    expect(screen.getByRole("button", { name: "Get regeneration estimate" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Start selected-take export" })).toBeEnabled();

    await user.type(screen.getByPlaceholderText("A short line."), "Unsaved revision");

    expect(screen.getByRole("button", { name: "Get regeneration estimate" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Start selected-take export" })).toBeDisabled();
    expect(screen.getByText("Save proofing edits before exporting")).toBeInTheDocument();
  });

  it("loads every cursor page instead of stopping at the first segment limit", async () => {
    const user = userEvent.setup();
    mockProofingContext();
    vi.spyOn(api, "proofingSummary").mockResolvedValue({ ...summary, counts: { ...summary.counts, total: 2 } });
    const secondView = {
      ...segmentView,
      segment: { ...segmentView.segment, id: "segment-2", ordinal: 1, original_text: "A second line.", effective_text: "A second line." },
    } as ProofingSegmentView;
    const list = vi.spyOn(api, "proofingSegments").mockImplementation(async (_projectId, query) => {
      const cursor = query == null ? undefined : query.cursor;
      return cursor === "next"
        ? { items: [secondView], total: 2 }
        : { items: [segmentView], total: 2, nextCursor: "next" };
    });
    vi.spyOn(api, "startProofingExport").mockResolvedValue({} as never);
    renderPanel(<ProofingWorkbench projectId="project-1" />);

    expect(await screen.findByRole("button", { name: /A short line\./ })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Load more segments" }));
    expect(await screen.findByRole("button", { name: /A second line\./ })).toBeInTheDocument();
    expect(list).toHaveBeenCalledWith("project-1", expect.objectContaining({ cursor: "next", limit: 250 }));
  });

  it("refreshes the segment and obtains a new estimate after an expired quote rejection", async () => {
    const user = userEvent.setup();
    mockProofingContext();
    vi.spyOn(api, "proofingSummary").mockResolvedValue(summary);
    vi.spyOn(api, "proofingSegments").mockResolvedValue({ items: [segmentView], total: 1 });
    const estimate = vi.spyOn(api, "proofingRegenerationEstimate")
      .mockResolvedValueOnce({ segmentId: "segment-1", segmentRevision: 4, providerProfileId: provider.id, providerName: provider.name, model: "tts-model", characters: 12, monetaryCostMicros: 100, currency: "EUR", unknownPricing: false, estimateToken: "estimate-1", expiresAt: new Date(Date.now() + 60_000).toISOString() })
      .mockResolvedValueOnce({ segmentId: "segment-1", segmentRevision: 4, providerProfileId: provider.id, providerName: provider.name, model: "tts-model", characters: 12, monetaryCostMicros: 100, currency: "EUR", unknownPricing: false, estimateToken: "estimate-2", expiresAt: new Date(Date.now() + 60_000).toISOString() });
    const regenerate = vi.spyOn(api, "startProofingRegeneration").mockRejectedValue(new ApiError({
      type: "urn:audiobookai:problem:conflict",
      title: "Conflict",
      status: 409,
      code: "estimate_expired",
      detail: "the regeneration estimate expired",
    }));
    vi.spyOn(api, "startProofingExport").mockResolvedValue({} as never);
    renderPanel(<ProofingWorkbench projectId="project-1" />);

    await user.click(await screen.findByRole("button", { name: /A short line\./ }));
    await user.click(screen.getByRole("button", { name: "Get regeneration estimate" }));
    await screen.findByText("Estimated cost");
    await user.click(screen.getByRole("switch", { name: /I reviewed this estimate/ }));
    await user.click(screen.getByRole("button", { name: "Start regeneration" }));
    await screen.findByText("the regeneration estimate expired");
    await user.click(screen.getByRole("button", { name: "Reconnect" }));

    await waitFor(() => expect(estimate).toHaveBeenCalledTimes(2));
    expect(regenerate).toHaveBeenCalledTimes(1);
  });
});

describe("distribution workspace", () => {
  it("registers selected completed exports instead of requiring upload UUID entry", async () => {
    const user = userEvent.setup();
    const metadata: DistributionMetadata = { authors: ["A. Author"], narrators: [], opening_credit_segment_ids: [], closing_credit_segment_ids: [], sample_segment_ids: [], attestations: {} };
    vi.spyOn(api, "distributionPolicies").mockResolvedValue({ items: [{ target: "generic_m4b", policyVersion: "generic-1", effectiveDate: "2026-08-05", sourceUrls: [], displayName: "Generic M4B", rules: [] }] });
    vi.spyOn(api, "distributionMetadata").mockResolvedValue({ revision: 0, metadata });
    vi.spyOn(api, "distributionPackages").mockResolvedValue({ items: [] });
    vi.spyOn(api, "exports").mockResolvedValue({ items: [
      { id: "export-2", projectId: "project-1", jobId: "job-1", partIndex: 1, partCount: 2, projectTitle: "Book", format: "mp3", splitMode: "per_chapter", fileName: "chapter-2.mp3", sizeBytes: 9_000, durationSeconds: 55, createdAt: "2026-08-05T08:00:00Z", downloadUrl: "/download/2", manifestUrl: "/manifest", chapterMarkers: false },
      { id: "export-1", projectId: "project-1", jobId: "job-1", partIndex: 0, partCount: 2, projectTitle: "Book", format: "mp3", splitMode: "per_chapter", fileName: "chapter-1.mp3", sizeBytes: 10_000, durationSeconds: 60, createdAt: "2026-08-05T08:00:00Z", downloadUrl: "/download/1", manifestUrl: "/manifest", chapterMarkers: false },
    ], total: 2 });
    vi.spyOn(api, "createDistributionPackage").mockResolvedValue({} as never);
    vi.spyOn(api, "updateDistributionMetadata").mockResolvedValue({ revision: 1, metadata });
    renderPanel(<DistributionPanel projectId="project-1" />);

    await user.click(await screen.findByRole("radio", { name: /chapter-1\.mp3/ }));
    await user.click(screen.getByRole("button", { name: "Create package" }));
    await waitFor(() => expect(api.createDistributionPackage).toHaveBeenCalledWith("project-1", "generic_m4b", ["export-1", "export-2"], []));
  });

  it("clears the ACX authorization reference when authorization is withdrawn", async () => {
    const user = userEvent.setup();
    const metadata: DistributionMetadata = {
      authors: ["A. Author"], narrators: [], opening_credit_segment_ids: [], closing_credit_segment_ids: [], sample_segment_ids: [],
      attestations: { acx_external_authorization: "2026-08-05T08:00:00Z", acx_authorization_reference: "ACX-123" },
    };
    vi.spyOn(api, "distributionPolicies").mockResolvedValue({ items: [{ target: "acx", policyVersion: "acx-1", effectiveDate: "2026-08-05", sourceUrls: [], displayName: "ACX", rules: [] }] });
    vi.spyOn(api, "distributionMetadata").mockResolvedValue({ revision: 3, metadata });
    vi.spyOn(api, "distributionPackages").mockResolvedValue({ items: [] });
    vi.spyOn(api, "exports").mockResolvedValue({ items: [], total: 0 });
    const update = vi.spyOn(api, "updateDistributionMetadata").mockResolvedValue({ revision: 4, metadata: { ...metadata, attestations: {} } });
    renderPanel(<DistributionPanel projectId="project-1" />);

    await user.click(await screen.findByRole("switch", { name: "ACX separately authorized this synthetic narration" }));
    await user.click(screen.getByRole("button", { name: "Save distribution metadata" }));

    await waitFor(() => expect(update).toHaveBeenCalled());
    const submitted = update.mock.calls[0]?.[2];
    expect(submitted?.attestations.acx_external_authorization).toBeUndefined();
    expect(submitted?.attestations.acx_authorization_reference).toBeUndefined();
  });

  it("keeps commas inside names when authors and narrators are entered one per line", async () => {
    const user = userEvent.setup();
    const metadata: DistributionMetadata = { authors: [], narrators: [], opening_credit_segment_ids: [], closing_credit_segment_ids: [], sample_segment_ids: [], attestations: {} };
    vi.spyOn(api, "distributionPolicies").mockResolvedValue({ items: [{ target: "generic_m4b", policyVersion: "generic-1", effectiveDate: "2026-08-05", sourceUrls: [], displayName: "Generic M4B", rules: [] }] });
    vi.spyOn(api, "distributionMetadata").mockResolvedValue({ revision: 0, metadata });
    vi.spyOn(api, "distributionPackages").mockResolvedValue({ items: [] });
    vi.spyOn(api, "exports").mockResolvedValue({ items: [], total: 0 });
    const update = vi.spyOn(api, "updateDistributionMetadata").mockResolvedValue({ revision: 1, metadata });
    renderPanel(<DistributionPanel projectId="project-1" />);

    await user.type(await screen.findByLabelText(/^Authors/), "Doe, Jane\nSmith, Alex");
    await user.type(screen.getByLabelText(/^Narrators/), "Reader, Robin");
    await user.click(screen.getByRole("button", { name: "Save distribution metadata" }));

    await waitFor(() => expect(update).toHaveBeenCalled());
    expect(update.mock.calls[0]?.[2]).toEqual(expect.objectContaining({ authors: ["Doe, Jane", "Smith, Alex"], narrators: ["Reader, Robin"] }));
  });

  it("reloads a newer metadata revision instead of blindly retrying a stale save", async () => {
    const user = userEvent.setup();
    const original: DistributionMetadata = { authors: ["Original"], narrators: [], opening_credit_segment_ids: [], closing_credit_segment_ids: [], sample_segment_ids: [], attestations: {} };
    const fresh: DistributionMetadata = { ...original, authors: ["Concurrent editor"] };
    vi.spyOn(api, "distributionPolicies").mockResolvedValue({ items: [{ target: "generic_m4b", policyVersion: "generic-1", effectiveDate: "2026-08-05", sourceUrls: [], displayName: "Generic M4B", rules: [] }] });
    const metadataRequest = vi.spyOn(api, "distributionMetadata")
      .mockResolvedValueOnce({ revision: 2, metadata: original })
      .mockResolvedValueOnce({ revision: 3, metadata: fresh });
    vi.spyOn(api, "distributionPackages").mockResolvedValue({ items: [] });
    vi.spyOn(api, "exports").mockResolvedValue({ items: [], total: 0 });
    const update = vi.spyOn(api, "updateDistributionMetadata").mockRejectedValue(new ApiError({
      type: "urn:audiobookai:problem:conflict",
      title: "Conflict",
      status: 409,
      code: "stale_distribution_metadata",
      detail: "distribution metadata changed since it was loaded",
    }));
    renderPanel(<DistributionPanel projectId="project-1" />);

    const authors = await screen.findByDisplayValue("Original");
    await user.clear(authors);
    await user.type(authors, "My stale edit");
    await user.click(screen.getByRole("button", { name: "Save distribution metadata" }));
    await waitFor(() => expect(update).toHaveBeenCalledTimes(1));
    await user.click(await screen.findByRole("button", { name: "Reload latest metadata" }));

    await waitFor(() => expect(metadataRequest).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.getByDisplayValue("Concurrent editor")).toBeInTheDocument());
    expect(update).toHaveBeenCalledTimes(1);
  });

  it("preserves a dirty metadata draft across a background revision refresh", async () => {
    const user = userEvent.setup();
    const original: DistributionMetadata = { authors: ["Original"], narrators: [], opening_credit_segment_ids: [], closing_credit_segment_ids: [], sample_segment_ids: [], attestations: {} };
    const concurrent: DistributionMetadata = { ...original, authors: ["Concurrent editor"] };
    vi.spyOn(api, "distributionPolicies").mockResolvedValue({ items: [{ target: "generic_m4b", policyVersion: "generic-1", effectiveDate: "2026-08-05", sourceUrls: [], displayName: "Generic M4B", rules: [] }] });
    vi.spyOn(api, "distributionMetadata").mockResolvedValue({ revision: 2, metadata: original });
    vi.spyOn(api, "distributionPackages").mockResolvedValue({ items: [] });
    vi.spyOn(api, "exports").mockResolvedValue({ items: [], total: 0 });
    const update = vi.spyOn(api, "updateDistributionMetadata").mockRejectedValue(new ApiError({
      type: "urn:audiobookai:problem:conflict",
      title: "Conflict",
      status: 409,
      code: "stale_distribution_metadata",
      detail: "distribution metadata changed since it was loaded",
    }));
    const { queryClient } = renderPanel(<DistributionPanel projectId="project-1" />);

    const authors = await screen.findByDisplayValue("Original");
    await user.clear(authors);
    await user.type(authors, "My preserved draft");
    act(() => queryClient.setQueryData(["distribution", "project-1", "metadata"], { revision: 3, metadata: concurrent }));

    await screen.findByText("A newer metadata revision exists");
    expect(screen.getByDisplayValue("My preserved draft")).toBeInTheDocument();
    expect(screen.queryByDisplayValue("Concurrent editor")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Save distribution metadata" }));
    await waitFor(() => expect(update).toHaveBeenCalledWith("project-1", 2, expect.objectContaining({ authors: ["My preserved draft"] })));
  });

  it("does not roll metadata back when an older save response arrives after a newer refetch", async () => {
    const user = userEvent.setup();
    const original: DistributionMetadata = { authors: ["Original"], narrators: [], opening_credit_segment_ids: [], closing_credit_segment_ids: [], sample_segment_ids: [], attestations: {} };
    const concurrent: DistributionMetadata = { ...original, authors: ["Concurrent revision four"] };
    vi.spyOn(api, "distributionPolicies").mockResolvedValue({ items: [{ target: "generic_m4b", policyVersion: "generic-1", effectiveDate: "2026-08-05", sourceUrls: [], displayName: "Generic M4B", rules: [] }] });
    vi.spyOn(api, "distributionMetadata").mockResolvedValue({ revision: 2, metadata: original });
    vi.spyOn(api, "distributionPackages").mockResolvedValue({ items: [] });
    vi.spyOn(api, "exports").mockResolvedValue({ items: [], total: 0 });
    let resolveSave!: (view: { revision: number; metadata: DistributionMetadata }) => void;
    const saveResponse = new Promise<{ revision: number; metadata: DistributionMetadata }>((resolve) => { resolveSave = resolve; });
    vi.spyOn(api, "updateDistributionMetadata").mockReturnValue(saveResponse);
    const { queryClient } = renderPanel(<DistributionPanel projectId="project-1" />);

    const authors = await screen.findByDisplayValue("Original");
    await user.clear(authors);
    await user.type(authors, "My in-flight draft");
    await user.click(screen.getByRole("button", { name: "Save distribution metadata" }));
    act(() => queryClient.setQueryData(["distribution", "project-1", "metadata"], { revision: 4, metadata: concurrent }));
    expect(await screen.findByText("A newer metadata revision exists")).toBeInTheDocument();

    act(() => resolveSave({ revision: 3, metadata: { ...original, authors: ["Older save response"] } }));
    await waitFor(() => expect(screen.getByDisplayValue("My in-flight draft")).toBeInTheDocument());
    expect(screen.queryByDisplayValue("Older save response")).not.toBeInTheDocument();
    expect(queryClient.getQueryData(["distribution", "project-1", "metadata"])).toEqual({ revision: 4, metadata: concurrent });
  });
});
