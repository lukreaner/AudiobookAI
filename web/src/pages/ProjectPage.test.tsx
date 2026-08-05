import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { Character, Job, ProjectDetail, PronunciationRule, ProviderProfile, Voice } from "../api/types";
import i18n from "../i18n";
import { AUTO_SPEAKER, NARRATOR_SPEAKER, parseAliases } from "../features/characterReview";
import { ProjectPage } from "./ProjectPage";

const project: ProjectDetail = {
  id: "project-1",
  title: "The Example Book",
  author: "Example Author",
  chapterCount: 1,
  selectedChapterCount: 1,
  progress: 0,
  status: "ready",
  updatedAt: "2026-08-04T10:00:00Z",
  consentCloudText: true,
  consentCloudAudio: false,
  characterReviewStatus: "needs_review",
  characterRevision: 3,
  chapters: [{
    id: "chapter-1",
    index: 0,
    title: "Chapter One",
    selected: true,
    wordCount: 120,
    characterCount: 640,
    status: "pending",
  }],
};

const alice: Character = {
  id: "character-alice",
  role: "character",
  canonicalName: "Alice",
  aliases: ["Ally"],
  confidence: 0.82,
  dialogueCount: 1,
  evidence: [{
    id: "evidence-1",
    paragraphId: "paragraph-42",
    chapterId: "chapter-1",
    chapterTitle: "Chapter One",
    excerpt: "I knew you would come back.",
    confidence: 0.82,
    startOffset: 14,
    endOffset: 42,
  }],
};

const bob: Character = {
  id: "character-bob",
  role: "character",
  canonicalName: "Bob",
  aliases: [],
  confidence: 0.91,
  dialogueCount: 0,
  evidence: [],
};

const queuedJob: Job = {
  id: "job-1",
  projectId: "project-1",
  projectTitle: "The Example Book",
  kind: "character_detection",
  status: "queued",
  progress: 0,
  updatedAt: "2026-08-04T10:05:00Z",
  units: [],
  uncertainCharge: false,
};

const cloneProvider: ProviderProfile = {
  id: "provider-clone",
  name: "Cloud Voice Lab",
  kind: "elevenlabs",
  mode: "cloud_remote",
  arguments: [],
  status: "online",
  credentialConfigured: true,
  capabilities: {
    tts: true,
    characterDetection: false,
    streaming: true,
    voiceCloning: true,
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

const ownedClone: Voice = {
  id: "voice-clone-1",
  providerProfileId: cloneProvider.id,
  name: "Story Voice",
  kind: "remote_clone",
  owned: true,
};

const pronunciationRule: PronunciationRule = {
  id: "rule-1",
  projectId: project.id,
  scope: "project",
  kind: "whole_word",
  source: "Kyiv",
  replacement: "Kee-yiv",
  language: "en",
  caseSensitive: false,
  enabled: true,
  order: 0,
};

function renderProjectTab(tab: "characters" | "preflight" | "pronunciation") {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return { ...render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[`/projects/project-1/${tab}`]}>
        <Routes>
          <Route path={`/projects/:id/${tab}`} element={<ProjectPage tab={tab} />} />
          <Route path="/jobs/:id" element={<div>Job opened</div>} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  ), queryClient: client };
}

function renderCharacterReview() {
  return renderProjectTab("characters");
}

beforeEach(async () => {
  await i18n.changeLanguage("en");
  vi.spyOn(api, "project").mockResolvedValue(project);
  vi.spyOn(api, "characters").mockResolvedValue({ items: [alice, bob], total: 2, characterRevision: 3 });
  vi.spyOn(api, "characterDetectionStatus").mockResolvedValue({});
  vi.spyOn(api, "providers").mockResolvedValue({ items: [], total: 0 });
  vi.spyOn(api, "voices").mockResolvedValue({ items: [], total: 0 });
  vi.spyOn(api, "updateCharacter").mockResolvedValue({ character: alice, characterRevision: 4 });
  vi.spyOn(api, "createCharacter").mockResolvedValue({ character: alice, characterRevision: 4 });
  vi.spyOn(api, "mergeCharacter").mockResolvedValue({ character: bob, removedCharacterId: alice.id, characterRevision: 4 });
  vi.spyOn(api, "deleteCharacter").mockResolvedValue({ removedCharacterId: bob.id, characterRevision: 4 });
  vi.spyOn(api, "setSpeakerOverride").mockResolvedValue({ characterRevision: 4 });
  vi.spyOn(api, "deleteSpeakerOverride").mockResolvedValue({ characterRevision: 4 });
  vi.spyOn(api, "approveCharacters").mockResolvedValue({ reviewStatus: "approved", characterRevision: 4 });
  vi.spyOn(api, "detectCharacters").mockResolvedValue(queuedJob);
  vi.spyOn(api, "jobAction").mockResolvedValue({ ...queuedJob, status: "running" });
  vi.spyOn(api, "pronunciationRules").mockResolvedValue({ items: [], total: 0 });
  vi.spyOn(api, "createPronunciationRule").mockImplementation(async (rule) => ({ id: "rule-1", ...rule } as PronunciationRule));
  vi.spyOn(api, "previewPronunciationRules").mockResolvedValue({ originalText: "", transformedText: "", appliedRuleIds: [], conflicts: [] });
  vi.spyOn(api, "deletePronunciationRule").mockResolvedValue(undefined);
  vi.spyOn(api, "createVoiceClone").mockResolvedValue(ownedClone);
  vi.spyOn(api, "updateVoiceClone").mockResolvedValue(ownedClone);
  vi.spyOn(api, "deleteVoiceClone").mockResolvedValue(undefined);
  vi.spyOn(api, "updateProject").mockResolvedValue(project);
  vi.spyOn(api, "estimate").mockResolvedValue({
    selectedChapters: 1,
    characters: 640,
    estimatedDurationSeconds: 46,
    estimatedDiskBytes: 4_416_000,
    monetaryCostMicros: 640,
    currency: "EUR",
    priceSource: "Configured snapshot",
    priceEffectiveAt: "2026-08-01T00:00:00Z",
    providerEstimates: [{
      providerProfileId: "provider-local",
      providerName: "Local fixture",
      model: "fixture-model",
      characters: 640,
      estimatedDurationSeconds: 46,
      monetaryCostMicros: 640,
      currency: "EUR",
      priceSource: "Configured snapshot",
      priceEffectiveAt: "2026-08-01T00:00:00Z",
    }],
    unknownFields: ["provider throughput"],
  });
  vi.spyOn(api, "dryRun").mockResolvedValue({ ready: true, checkedAt: "2026-08-04T10:04:00Z", checks: [] });
  vi.spyOn(api, "startJob").mockResolvedValue(queuedJob);
});

describe("pronunciation rules", () => {
  it("sends the project identifier required by project-scoped rules", async () => {
    const user = userEvent.setup();
    renderProjectTab("pronunciation");

    await user.click((await screen.findAllByRole("button", { name: "Add pronunciation" }))[0]);
    await user.type(screen.getByRole("textbox", { name: "When text is" }), "Caoimhe");
    await user.type(screen.getByRole("textbox", { name: "Pronounce as" }), "Kee-va");
    await user.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() => expect(api.createPronunciationRule).toHaveBeenCalledWith({
      source: "Caoimhe",
      replacement: "Kee-va",
      kind: "whole_word",
      scope: "project",
      language: undefined,
      caseSensitive: false,
      projectId: "project-1",
      characterId: undefined,
      enabled: true,
      order: 0,
    }));
  });

  it("previews deterministic transformations with language and character scope", async () => {
    const user = userEvent.setup();
    vi.mocked(api.previewPronunciationRules).mockResolvedValue({
      originalText: "Kyiv greeted Alice.",
      transformedText: "Kee-yiv greeted Alice.",
      appliedRuleIds: ["rule-1"],
      conflicts: [],
    });
    renderProjectTab("pronunciation");

    await user.type(await screen.findByRole("textbox", { name: "Text to transform" }), "Kyiv greeted Alice.");
    await user.type(screen.getByRole("textbox", { name: "Language" }), "en");
    await user.selectOptions(screen.getByRole("combobox", { name: "Character" }), "character-alice");
    await user.click(screen.getByRole("button", { name: "Apply rules" }));

    await waitFor(() => expect(api.previewPronunciationRules).toHaveBeenCalledWith({
      text: "Kyiv greeted Alice.",
      projectId: "project-1",
      language: "en",
      characterId: "character-alice",
    }));
    expect(await screen.findByText("Kee-yiv greeted Alice.")).toBeInTheDocument();
    expect(screen.getByText("1 rule applied")).toBeInTheDocument();
  });

  it("creates a character-scoped rule and confirms before deleting it", async () => {
    const user = userEvent.setup();
    vi.mocked(api.pronunciationRules).mockResolvedValue({ items: [pronunciationRule], total: 1 });
    renderProjectTab("pronunciation");

    await user.click((await screen.findAllByRole("button", { name: "Add pronunciation" }))[0]);
    await user.type(screen.getByRole("textbox", { name: "When text is" }), "Dr.");
    await user.type(screen.getByRole("textbox", { name: "Pronounce as" }), "Doctor");
    await user.selectOptions(screen.getByRole("combobox", { name: "Rule type" }), "literal");
    const dialogCharacter = screen.getAllByRole("combobox", { name: "Character" }).at(-1);
    expect(dialogCharacter).toBeDefined();
    await user.selectOptions(dialogCharacter!, "character-bob");
    await user.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() => expect(api.createPronunciationRule).toHaveBeenCalledWith({
      source: "Dr.",
      replacement: "Doctor",
      kind: "literal",
      scope: "project",
      language: undefined,
      characterId: "character-bob",
      caseSensitive: false,
      projectId: "project-1",
      enabled: true,
      order: 1,
    }));

    await user.click(screen.getByRole("button", { name: "Delete rule for Kyiv" }));
    expect(screen.getByRole("heading", { name: "Delete pronunciation rule?" })).toBeInTheDocument();
    expect(api.deletePronunciationRule).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(api.deletePronunciationRule).toHaveBeenCalledWith("rule-1"));
  });
});

describe("character review", () => {
  it("restores an active detection job and locks conflicting review edits", async () => {
    vi.mocked(api.characterDetectionStatus).mockResolvedValue({ activeJob: queuedJob, latestJob: queuedJob });
    renderCharacterReview();

    expect(await screen.findByText("Character detection is queued")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add character" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Edit Alice" })).toBeDisabled();
    expect(screen.getByRole("link", { name: "Open job" })).toHaveAttribute("href", "/jobs/job-1");
  });

  it("offers a durable retry when the latest detection job failed", async () => {
    const user = userEvent.setup();
    const failedJob: Job = { ...queuedJob, status: "failed", currentStage: "Provider timed out" };
    vi.mocked(api.characterDetectionStatus).mockResolvedValue({ latestJob: failedJob });
    renderCharacterReview();

    expect(await screen.findByText("The last character detection failed")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Retry failed job" }));
    await waitFor(() => expect(api.jobAction).toHaveBeenCalledWith(failedJob.id, "retry"));
  });

  it("creates, merges, and deletes identities with the current character revision", async () => {
    const user = userEvent.setup();
    renderCharacterReview();

    await user.click(await screen.findByRole("button", { name: "Add character" }));
    await user.type(screen.getByRole("textbox", { name: "Canonical name" }), "Charlie");
    await user.type(screen.getByRole("textbox", { name: "New character aliases" }), "Charles, Chaz");
    await user.click(screen.getAllByRole("button", { name: "Add character" }).at(-1)!);
    await waitFor(() => expect(api.createCharacter).toHaveBeenCalledWith("project-1", {
      canonicalName: "Charlie",
      aliases: ["Charles", "Chaz"],
      expectedCharacterRevision: 3,
    }, expect.any(String)));

    await user.click(screen.getByRole("button", { name: "Merge Alice" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "Merge target" }), bob.id);
    await user.click(screen.getByRole("switch", { name: "I understand this merge cannot be undone" }));
    await user.click(screen.getByRole("button", { name: "Merge character" }));
    await waitFor(() => expect(api.mergeCharacter).toHaveBeenCalledWith(
      "project-1", alice.id, bob.id, 3, expect.any(String),
    ));

    await user.click(screen.getByRole("button", { name: "Delete Bob" }));
    await user.click(screen.getByRole("switch", { name: "I understand this character will be permanently deleted" }));
    await user.click(screen.getByRole("button", { name: "Delete character" }));
    await waitFor(() => expect(api.deleteCharacter).toHaveBeenCalledWith(
      "project-1", bob.id, 3, expect.any(String),
    ));
  });

  it("sends only capability-gated temperature and reasoning controls", async () => {
    const user = userEvent.setup();
    const provider: ProviderProfile = {
      ...cloneProvider,
      id: "provider-ai",
      name: "Reasoning AI",
      kind: "openai",
      capabilities: {
        ...cloneProvider.capabilities!,
        tts: false,
        characterDetection: true,
        voiceCloning: false,
        temperature: "nullable",
        reasoning: ["disabled", "effort"],
      },
    };
    vi.mocked(api.providers).mockResolvedValue({ items: [provider], total: 1 });
    renderCharacterReview();

    await user.selectOptions(await screen.findByRole("combobox", { name: "Detection provider" }), provider.id);
    await user.selectOptions(screen.getByRole("combobox", { name: /^Temperature mode/ }), "null");
    await user.selectOptions(screen.getByRole("combobox", { name: /^Reasoning mode/ }), "effort");
    await user.selectOptions(screen.getByRole("combobox", { name: "Effort level" }), "high");
    await user.click(screen.getByRole("button", { name: "Run detection again" }));

    await waitFor(() => expect(api.detectCharacters).toHaveBeenCalledWith("project-1", {
      providerProfileId: provider.id,
      temperature: { mode: "null" },
      reasoning: { mode: "effort", effort: "high" },
      expectedCharacterRevision: 3,
    }, expect.any(String)));
  });

  it("normalizes aliases without duplicates or the canonical name", () => {
    expect(parseAliases("Ally, ally\nA. Example\nAlice", "Alice")).toEqual(["Ally", "A. Example"]);
  });

  it("edits the canonical name and aliases through accessible controls", async () => {
    const user = userEvent.setup();
    renderCharacterReview();

    await user.click(await screen.findByRole("button", { name: "Edit Alice" }));
    const name = screen.getByRole("textbox", { name: "Canonical name" });
    const aliases = screen.getByRole("textbox", { name: "Aliases" });
    await user.clear(name);
    await user.type(name, "Alicia");
    await user.clear(aliases);
    await user.type(aliases, "Ally, ally{enter}A. Example{enter}Alicia");
    await user.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(api.updateCharacter).toHaveBeenCalledWith("project-1", "character-alice", {
      canonicalName: "Alicia",
      aliases: ["Ally", "A. Example"],
      expectedCharacterRevision: 3,
    }));
  });

  it("reflects approval invalidation immediately after changing a voice", async () => {
    const user = userEvent.setup();
    const approvedProject = { ...project, characterReviewStatus: "approved" as const };
    const needsReviewProject = { ...project, characterReviewStatus: "needs_review" as const, characterRevision: 4 };
    const assignedAlice: Character = {
      ...alice,
      voiceAssignment: {
        providerProfileId: cloneProvider.id,
        providerName: cloneProvider.name,
        voiceId: ownedClone.id,
        voiceName: ownedClone.name,
        performance: {},
        timing: {},
      },
    };
    vi.mocked(api.project).mockResolvedValueOnce(approvedProject).mockResolvedValue(needsReviewProject);
    vi.mocked(api.characters)
      .mockResolvedValueOnce({ items: [alice, bob], total: 2, characterRevision: 3 })
      .mockResolvedValue({ items: [assignedAlice, bob], total: 2, characterRevision: 4 });
    vi.mocked(api.providers).mockResolvedValue({ items: [cloneProvider], total: 1 });
    vi.mocked(api.voices).mockResolvedValue({ items: [ownedClone], total: 1 });
    vi.spyOn(api, "assignVoice").mockResolvedValue({ character: assignedAlice, characterRevision: 4 });
    renderCharacterReview();

    expect((await screen.findAllByText("Cast approved")).length).toBeGreaterThan(0);
    await user.click(screen.getAllByRole("button", { name: /No voice/ })[0]!);
    await user.selectOptions(screen.getByRole("combobox", { name: "Provider connections" }), cloneProvider.id);
    await user.selectOptions(screen.getByRole("combobox", { name: "Voice" }), ownedClone.id);
    await user.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(api.assignVoice).toHaveBeenCalled());
    expect(await screen.findByRole("button", { name: "Approve cast for synthesis" })).toBeInTheDocument();
    expect(screen.getByText("Review required")).toBeInTheDocument();
    expect(api.project).toHaveBeenCalledTimes(2);
  });

  it("sets character and narrator overrides and can return to the detected speaker", async () => {
    const user = userEvent.setup();
    renderCharacterReview();

    await user.click(await screen.findByText("Dialogue evidence"));
    const speaker = screen.getByRole("combobox", { name: "Speaker for dialogue in Chapter One" });
    await user.selectOptions(speaker, "character-bob");

    await waitFor(() => expect(api.setSpeakerOverride).toHaveBeenCalledWith("project-1", "paragraph-42", {
      characterId: "character-bob",
      startOffset: 14,
      endOffset: 42,
    }, 3));
    await waitFor(() => expect(speaker).toHaveValue("character-bob"));

    await user.selectOptions(speaker, NARRATOR_SPEAKER);

    await waitFor(() => expect(api.setSpeakerOverride).toHaveBeenCalledWith("project-1", "paragraph-42", {
      characterId: null,
      startOffset: 14,
      endOffset: 42,
    }, 3));
    await waitFor(() => expect(speaker).toHaveValue(NARRATOR_SPEAKER));

    await user.selectOptions(speaker, AUTO_SPEAKER);
    await waitFor(() => expect(api.deleteSpeakerOverride).toHaveBeenCalledWith("project-1", "paragraph-42", 14, 42, 3));
  });

  it("stops showing an optimistic speaker when a newer durable revision arrives", async () => {
    const user = userEvent.setup();
    const evidence = alice.evidence[0]!;
    const withSpeaker = (characterId: string, revision: number) => ({
      items: [{
        ...alice,
        evidence: [{
          ...evidence,
          speakerOverride: { characterId, startOffset: evidence.startOffset, endOffset: evidence.endOffset },
          speakerOverrideActive: true,
        }],
      }, bob],
      total: 2,
      characterRevision: revision,
    });
    vi.mocked(api.characters)
      .mockResolvedValueOnce({ items: [alice, bob], total: 2, characterRevision: 3 })
      .mockResolvedValueOnce(withSpeaker(bob.id, 4))
      .mockResolvedValue(withSpeaker(alice.id, 5));
    vi.mocked(api.setSpeakerOverride).mockResolvedValue({ characterRevision: 4 });
    const { queryClient } = renderCharacterReview();

    await user.click(await screen.findByText("Dialogue evidence"));
    const speaker = screen.getByRole("combobox", { name: "Speaker for dialogue in Chapter One" });
    await user.selectOptions(speaker, bob.id);
    await waitFor(() => expect(speaker).toHaveValue(bob.id));

    await queryClient.invalidateQueries({ queryKey: ["characters", "project-1"] });
    await waitFor(() => expect(speaker).toHaveValue(alice.id));
    await user.click(screen.getByRole("button", { name: "Approve cast for synthesis" }));
    await waitFor(() => expect(api.approveCharacters).toHaveBeenCalledWith("project-1", 5));
  });

  it("creates, renames, and explicitly confirms deletion of an app-owned remote clone", async () => {
    const user = userEvent.setup();
    vi.mocked(api.providers).mockResolvedValue({ items: [cloneProvider], total: 1 });
    vi.mocked(api.voices).mockResolvedValue({ items: [ownedClone], total: 1 });
    vi.mocked(api.updateProject).mockResolvedValue({ ...project, consentCloudAudio: true });
    vi.mocked(api.updateVoiceClone).mockResolvedValue({ ...ownedClone, name: "Updated Voice" });
    renderCharacterReview();

    await user.click(await screen.findByRole("button", { name: "Manage voice clones" }));
    expect(screen.getByRole("heading", { name: "Voice clone library" })).toBeInTheDocument();

    await user.click(screen.getByRole("switch", { name: "Allow this project's reference audio to be sent to cloud providers" }));
    await waitFor(() => expect(api.updateProject).toHaveBeenCalledWith("project-1", { consentCloudAudio: true }));

    const sample = new File([new Uint8Array([1, 2, 3])], "sample.wav", { type: "audio/wav" });
    await user.type(screen.getByRole("textbox", { name: "Clone name" }), "New Story Voice");
    await user.type(screen.getByRole("textbox", { name: "Description" }), "A clear reading voice");
    await user.upload(screen.getByLabelText("Reference audio"), sample);
    const createButton = screen.getByRole("button", { name: "Create voice clone" });
    await waitFor(() => expect(createButton).toBeEnabled());
    expect(api.createVoiceClone).not.toHaveBeenCalled();
    await user.click(createButton);
    await waitFor(() => expect(api.createVoiceClone).toHaveBeenCalledWith("provider-clone", {
      name: "New Story Voice",
      description: "A clear reading voice",
      projectId: "project-1",
      referenceAudio: [sample],
    }));

    await user.click(screen.getByRole("button", { name: "Rename Story Voice" }));
    const cloneName = screen.getByRole("textbox", { name: "Rename Story Voice" });
    await user.clear(cloneName);
    await user.type(cloneName, "Updated Voice");
    await user.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() => expect(api.updateVoiceClone).toHaveBeenCalledWith("voice-clone-1", "Updated Voice"));

    await user.click(screen.getByRole("button", { name: "Delete Story Voice" }));
    const deleteButton = screen.getByRole("button", { name: "Delete remote clone" });
    expect(deleteButton).toBeDisabled();
    expect(api.deleteVoiceClone).not.toHaveBeenCalled();
    await user.click(screen.getByRole("switch", { name: "I understand that the remote clone will be permanently deleted" }));
    await user.click(deleteButton);
    await waitFor(() => expect(api.deleteVoiceClone).toHaveBeenCalledWith("voice-clone-1", true));
  });
});

describe("preflight export settings", () => {
  it("shows the provider, model, and local rate-card provenance", async () => {
    const user = userEvent.setup();
    renderProjectTab("preflight");

    await user.click(await screen.findByRole("button", { name: "Calculate estimate" }));

    expect(await screen.findByText("Provider and model breakdown")).toBeInTheDocument();
    expect(screen.getByText("Local fixture")).toBeInTheDocument();
    expect(screen.getByText("fixture-model")).toBeInTheDocument();
    expect(screen.getAllByText(/Configured snapshot/).length).toBeGreaterThan(0);
  });

  it("starts with audiobook-safe defaults and omits blank optional paths", async () => {
    const user = userEvent.setup();
    renderProjectTab("preflight");

    expect(await screen.findByRole("heading", { name: "Export settings" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Audio format" })).toHaveValue("m4b");
    expect(screen.getByRole("combobox", { name: "Encoded bitrate" })).toHaveValue("128");
    expect(screen.getByRole("switch", { name: "One file per chapter" })).not.toBeChecked();
    expect(screen.getByRole("spinbutton", { name: "Music gain (dB)" })).toHaveValue(-24);
    expect(screen.getByRole("switch", { name: "Speech-driven ducking" })).toBeChecked();
    expect(screen.getByText("Leave blank to use AudiobookAI's managed export directory.")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Run checks" }));
    const startButton = screen.getByRole("button", { name: "Start full conversion" });
    await waitFor(() => expect(startButton).toBeEnabled());
    await user.click(startButton);

    await waitFor(() => expect(api.startJob).toHaveBeenCalledWith({
      projectId: "project-1",
      allowBudgetOverride: false,
      export: {
        format: "m4b",
        splitPerChapter: false,
        bitrateKbps: 128,
        confirmBackgroundMusicOwned: false,
        musicGainDb: -24,
        ducking: true,
      },
    }));
  });

  it("requires music ownership and sends the selected export profile", async () => {
    const user = userEvent.setup();
    renderProjectTab("preflight");

    await screen.findByRole("heading", { name: "Export settings" });
    await user.click(screen.getByRole("button", { name: "Run checks" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Start full conversion" })).toBeEnabled());

    await user.selectOptions(screen.getByRole("combobox", { name: "Audio format" }), "mp3");
    await user.selectOptions(screen.getByRole("combobox", { name: "Encoded bitrate" }), "192");
    await user.type(screen.getByRole("textbox", { name: "Output directory" }), " /tmp/audiobooks ");
    await user.type(screen.getByRole("textbox", { name: "File name" }), " Example Book ");
    await user.click(screen.getByRole("switch", { name: "One file per chapter" }));
    await user.type(screen.getByRole("textbox", { name: "Background audio path" }), " /music/owned.wav ");

    const startButton = screen.getByRole("button", { name: "Start full conversion" });
    expect(screen.getByRole("alert")).toHaveTextContent("Confirm that you own or are licensed to use the selected background audio.");
    expect(startButton).toBeDisabled();

    fireEvent.change(screen.getByRole("spinbutton", { name: "Music gain (dB)" }), { target: { value: "-18" } });
    await user.click(screen.getByRole("switch", { name: "Speech-driven ducking" }));
    await user.click(screen.getByRole("switch", { name: "I own this audio or have permission to use it" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(startButton).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Run checks" }));
    await waitFor(() => expect(startButton).toBeEnabled());
    await user.click(startButton);

    await waitFor(() => expect(api.startJob).toHaveBeenCalledWith({
      projectId: "project-1",
      allowBudgetOverride: false,
      export: {
        format: "mp3",
        splitPerChapter: true,
        outputDirectory: "/tmp/audiobooks",
        fileName: "Example Book",
        bitrateKbps: 192,
        backgroundMusicPath: "/music/owned.wav",
        confirmBackgroundMusicOwned: true,
        musicGainDb: -18,
        ducking: false,
      },
    }));
  });
});
