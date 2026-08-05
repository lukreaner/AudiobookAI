import type {
  AppSettings,
  IssuedLanApiToken,
  BookSummary,
  Budget,
  Character,
  CharacterPage,
  CharacterMutationResult,
  CharacterDetectionStatus,
  CharacterDetectionInput,
  CharacterIdentityInput,
  DiagnosticPage,
  DiagnosticQuery,
  DryRunResult,
  DistributionMetadata,
  DistributionMetadataView,
  DistributionPackageView,
  DistributionPolicyView,
  DistributionQualityRun,
  DistributionTarget,
  Estimate,
  ExportArtifact,
  HealthStatus,
  ImportDraft,
  Job,
  LanApiToken,
  PageResponse,
  PreviewResult,
  ProofingSegmentPage,
  ProofingSegmentQuery,
  ProofingSegmentView,
  ProofingSummary,
  QualityReport,
  RegenerationEstimate,
  SegmentReviewState,
  SegmentSelectionInput,
  SegmentTake,
  SegmentUpdateInput,
  ProjectDetail,
  PronunciationPreviewInput,
  PronunciationPreviewResult,
  PronunciationRule,
  ProviderLogsResponse,
  ProviderModelLibrary,
  ProviderModelOperation,
  ProviderProfile,
  ProviderProfileInput,
  MlxManagement,
  MlxOperation,
  RateCard,
  RateCardInput,
  SecretStatus,
  UsageSummary,
  Voice,
  VoiceAuditionInput,
  VoiceAuditionResponse,
  VoiceAssignment,
  VoiceCloneInput,
  SpeakerOverrideInput,
  StartJobRequest,
  ProblemDetails,
} from "./types";

declare global {
  interface Window {
    __AUDIOBOOKAI_API__?: string;
    __AUDIOBOOKAI_BOOTSTRAP__?: string;
    __AUDIOBOOKAI_OPEN_EPUB__?: string;
  }
}

const baseUrl = (
  window.__AUDIOBOOKAI_API__ ?? (import.meta.env.VITE_API_BASE_URL as string | undefined) ?? ""
).replace(/\/$/, "");

let bootstrapPromise: Promise<void> | undefined;

function ensureDesktopSession(): Promise<void> {
  if (bootstrapPromise) return bootstrapPromise;
  const nonce = window.__AUDIOBOOKAI_BOOTSTRAP__;
  delete window.__AUDIOBOOKAI_BOOTSTRAP__;
  if (!nonce) return Promise.resolve();
  bootstrapPromise = fetch(`${baseUrl}/api/v1/auth/bootstrap`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json", "Accept": "application/json" },
    body: JSON.stringify({ nonce }),
  }).then(async (response) => {
    if (response.ok) return;
    let problem: ProblemDetails;
    try {
      problem = (await response.json()) as ProblemDetails;
    } catch {
      problem = { type: "about:blank", title: response.statusText || "Authentication failed", status: response.status };
    }
    throw new ApiError(problem);
  });
  return bootstrapPromise;
}

export class ApiError extends Error {
  readonly problem: ProblemDetails;

  constructor(problem: ProblemDetails) {
    super(problem.detail || problem.title);
    this.name = "ApiError";
    this.problem = problem;
  }
}

function csrfToken(): string | undefined {
  return document.cookie
    .split(";")
    .map((part) => part.trim())
    .find((part) => part.startsWith("audiobookai_csrf="))
    ?.split("=")[1];
}

type RequestOptions = RequestInit & { idempotencyKey?: string };

async function request<T>(path: string, init: RequestOptions = {}): Promise<T> {
  if (!["/api/v1/health", "/api/v1/auth/bootstrap", "/api/v1/auth/login"].includes(path)) await ensureDesktopSession();
  const method = init.method?.toUpperCase() ?? "GET";
  const headers = new Headers(init.headers);
  headers.set("Accept", "application/json");
  if (init.body && !(init.body instanceof FormData)) headers.set("Content-Type", "application/json");
  if (!["GET", "HEAD"].includes(method)) {
    headers.set("Idempotency-Key", init.idempotencyKey ?? crypto.randomUUID());
    const csrf = csrfToken();
    if (csrf) headers.set("X-CSRF-Token", decodeURIComponent(csrf));
  }

  let response: Response;
  try {
    const { idempotencyKey: _idempotencyKey, ...fetchInit } = init;
    response = await fetch(`${baseUrl}${path}`, { ...fetchInit, headers, credentials: "include" });
  } catch (cause) {
    throw new ApiError({
      type: "urn:audiobookai:problem:offline",
      title: "Service unavailable",
      status: 0,
      detail: cause instanceof Error ? cause.message : "Could not connect to the AudiobookAI service.",
      code: "service_offline",
    });
  }

  if (!response.ok) {
    let problem: ProblemDetails;
    try {
      problem = (await response.json()) as ProblemDetails;
    } catch {
      problem = {
        type: "about:blank",
        title: response.statusText || "Request failed",
        status: response.status,
      };
    }
    throw new ApiError(problem);
  }
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}

const json = (value: unknown): string => JSON.stringify(value);

function diagnosticQueryString(query: DiagnosticQuery = {}): string {
  const parameters = new URLSearchParams();
  if (query.level) parameters.set("level", query.level);
  if (query.target?.trim()) parameters.set("target", query.target.trim());
  if (query.search?.trim()) parameters.set("search", query.search.trim());
  if (query.after != null) parameters.set("after", String(query.after));
  if (query.limit != null) parameters.set("limit", String(query.limit));
  const value = parameters.toString();
  return value ? `?${value}` : "";
}

function proofingQueryString(query: ProofingSegmentQuery = {}): string {
  const parameters = new URLSearchParams();
  if (query.chapterId) parameters.set("chapterId", query.chapterId);
  if (query.state) parameters.set("state", query.state);
  if (query.issuesOnly) parameters.set("issuesOnly", "true");
  if (query.staleOnly) parameters.set("staleOnly", "true");
  if (query.search?.trim()) parameters.set("search", query.search.trim());
  if (query.cursor) parameters.set("cursor", query.cursor);
  if (query.limit != null) parameters.set("limit", String(query.limit));
  const value = parameters.toString();
  return value ? `?${value}` : "";
}

async function downloadDiagnostics(query: DiagnosticQuery = {}): Promise<void> {
  await ensureDesktopSession();
  const response = await fetch(`${baseUrl}/api/v1/diagnostics/export${diagnosticQueryString(query)}`, {
    credentials: "include",
    headers: { Accept: "application/x-ndjson" },
  });
  if (!response.ok) {
    let problem: ProblemDetails;
    try {
      problem = (await response.json()) as ProblemDetails;
    } catch {
      problem = { type: "about:blank", title: response.statusText || "Export failed", status: response.status };
    }
    throw new ApiError(problem);
  }
  const blobUrl = URL.createObjectURL(await response.blob());
  const anchor = document.createElement("a");
  anchor.href = blobUrl;
  anchor.download = `audiobookai-diagnostics-${new Date().toISOString().replaceAll(":", "-")}.jsonl`;
  anchor.click();
  URL.revokeObjectURL(blobUrl);
}

export const api = {
  health: () => request<HealthStatus>("/api/v1/health"),
  loginLan: (password: string) =>
    request<{ authenticated: boolean; expiresInSeconds: number }>("/api/v1/auth/login", { method: "POST", body: json({ password }) }),
  books: () => request<PageResponse<BookSummary>>("/api/v1/projects"),
  project: (id: string) => request<ProjectDetail>(`/api/v1/projects/${id}`),
  deleteProject: (id: string) => request<void>(`/api/v1/projects/${id}`, { method: "DELETE" }),
  updateProject: (id: string, patch: Partial<ProjectDetail>) =>
    request<ProjectDetail>(`/api/v1/projects/${id}`, { method: "PATCH", body: json(patch) }),
  createImportDraft: (file: File) => {
    const data = new FormData();
    data.append("epub", file);
    return request<ImportDraft>("/api/v1/imports", { method: "POST", body: data });
  },
  createImportDraftFromPath: (sourcePath: string) =>
    request<ImportDraft>("/api/v1/imports/from-path", { method: "POST", body: json({ sourcePath }) }),
  commitImport: (draftId: string, chapterIds: string[]) =>
    request<ProjectDetail>(`/api/v1/imports/${draftId}/commit`, {
      method: "POST",
      body: json({ chapterIds }),
    }),
  characters: (projectId: string) =>
    request<CharacterPage>(`/api/v1/projects/${projectId}/characters`),
  characterDetectionStatus: (projectId: string) =>
    request<CharacterDetectionStatus>(`/api/v1/projects/${projectId}/character-detection`),
  detectCharacters: (projectId: string, input: CharacterDetectionInput, idempotencyKey?: string) =>
    request<Job>(`/api/v1/projects/${projectId}/character-detection`, {
      method: "POST",
      body: json(input),
      idempotencyKey,
    }),
  approveCharacters: (projectId: string, expectedCharacterRevision: number) =>
    request<{ reviewStatus: string; characterRevision: number }>(`/api/v1/projects/${projectId}/character-review`, {
      method: "PUT",
      body: json({ approved: true, expectedCharacterRevision }),
    }),
  assignVoice: (projectId: string, characterId: string, assignment: VoiceAssignment, expectedCharacterRevision: number) =>
    request<CharacterMutationResult>(`/api/v1/projects/${projectId}/characters/${characterId}/voice`, {
      method: "PUT",
      body: json({ ...assignment, expectedCharacterRevision }),
    }),
  updateCharacter: (projectId: string, characterId: string, identity: CharacterIdentityInput) =>
    request<CharacterMutationResult>(`/api/v1/projects/${projectId}/characters/${characterId}`, {
      method: "PATCH",
      body: json(identity),
    }),
  createCharacter: (projectId: string, identity: CharacterIdentityInput, idempotencyKey?: string) =>
    request<CharacterMutationResult>(`/api/v1/projects/${projectId}/characters`, {
      method: "POST",
      body: json(identity),
      idempotencyKey,
    }),
  mergeCharacter: (projectId: string, sourceId: string, targetCharacterId: string, expectedCharacterRevision: number, idempotencyKey?: string) =>
    request<CharacterMutationResult>(`/api/v1/projects/${projectId}/characters/${sourceId}/actions/merge`, {
      method: "POST",
      body: json({ targetCharacterId, expectedCharacterRevision }),
      idempotencyKey,
    }),
  deleteCharacter: (projectId: string, characterId: string, expectedCharacterRevision: number, idempotencyKey?: string) =>
    request<CharacterMutationResult>(`/api/v1/projects/${projectId}/characters/${characterId}/actions/delete`, {
      method: "POST",
      body: json({ expectedCharacterRevision }),
      idempotencyKey,
    }),
  setSpeakerOverride: (projectId: string, paragraphId: string, input: SpeakerOverrideInput, expectedCharacterRevision: number) =>
    request<{ characterRevision: number }>(`/api/v1/projects/${projectId}/speaker-overrides/${paragraphId}`, {
      method: "PUT",
      body: json({ ...input, expectedCharacterRevision }),
    }),
  deleteSpeakerOverride: (projectId: string, paragraphId: string, startOffset: number, endOffset: number, expectedCharacterRevision: number) =>
    request<{ characterRevision: number }>(`/api/v1/projects/${projectId}/speaker-overrides/${paragraphId}`, {
      method: "DELETE",
      body: json({ startOffset, endOffset, expectedCharacterRevision }),
    }),
  voices: (providerProfileId?: string) =>
    request<PageResponse<Voice>>(
      `/api/v1/voices${providerProfileId ? `?providerProfileId=${encodeURIComponent(providerProfileId)}` : ""}`,
    ),
  createVoiceClone: (providerProfileId: string, input: VoiceCloneInput) => {
    const data = new FormData();
    data.append("name", input.name);
    if (input.description) data.append("description", input.description);
    data.append("projectId", input.projectId);
    input.referenceAudio.forEach((sample) => data.append("referenceAudio", sample));
    return request<Voice>(`/api/v1/providers/${providerProfileId}/voice-clones`, {
      method: "POST",
      body: data,
    });
  },
  updateVoiceClone: (id: string, name: string) =>
    request<Voice>(`/api/v1/voices/${id}`, { method: "PATCH", body: json({ name }) }),
  deleteVoiceClone: (id: string, confirmed: true) =>
    request<void>(`/api/v1/voices/${id}?confirmed=${String(confirmed)}`, { method: "DELETE" }),
  pronunciationRules: (projectId?: string) =>
    request<PageResponse<PronunciationRule>>(
      `/api/v1/pronunciation-rules${projectId ? `?projectId=${encodeURIComponent(projectId)}` : ""}`,
    ),
  createPronunciationRule: (rule: Omit<PronunciationRule, "id">) =>
    request<PronunciationRule>("/api/v1/pronunciation-rules", { method: "POST", body: json(rule) }),
  deletePronunciationRule: (id: string) =>
    request<void>(`/api/v1/pronunciation-rules/${id}`, { method: "DELETE" }),
  previewPronunciationRules: (input: PronunciationPreviewInput) =>
    request<PronunciationPreviewResult>("/api/v1/pronunciation-rules/preview", {
      method: "POST",
      body: json(input),
    }),
  providers: () => request<PageResponse<ProviderProfile>>("/api/v1/providers"),
  providerAction: (id: string, action: "start" | "stop" | "restart" | "refresh") =>
    request<ProviderProfile>(`/api/v1/providers/${id}/actions/${action}`, { method: "POST" }),
  providerLogs: (id: string) =>
    request<ProviderLogsResponse>(`/api/v1/providers/${id}/actions/logs`, {
      method: "POST",
      body: json({}),
    }),
  providerModelAction: (id: string, action: "load-model" | "unload-model" | "switch-model", model: string) =>
    request<ProviderProfile>(`/api/v1/providers/${id}/actions/${action}`, {
      method: "POST",
      body: json({ model }),
    }),
  providerModels: (id: string) =>
    request<ProviderModelLibrary>(`/api/v1/providers/${id}/models`),
  downloadProviderModel: (id: string, model: string, quantization?: string) =>
    request<ProviderModelOperation>(`/api/v1/providers/${id}/models`, {
      method: "POST",
      body: json({ model, quantization: quantization || undefined }),
    }),
  cancelProviderModelDownload: (id: string, operationId: string) =>
    request<ProviderModelOperation>(`/api/v1/providers/${id}/model-downloads/${operationId}/cancel`, {
      method: "POST",
    }),
  deleteProviderModel: (id: string, model: string, confirmed: true) =>
    request<void>(`/api/v1/providers/${id}/models`, {
      method: "DELETE",
      body: json({ model, confirmed }),
    }),
  updateProvider: (id: string, patch: ProviderProfileInput) =>
    request<ProviderProfile>(`/api/v1/providers/${id}`, { method: "PATCH", body: json(patch) }),
  createProvider: (profile: ProviderProfileInput) =>
    request<ProviderProfile>("/api/v1/providers", { method: "POST", body: json(profile) }),
  deleteProvider: (id: string) =>
    request<void>(`/api/v1/providers/${id}`, { method: "DELETE" }),
  mlxManagement: () => request<MlxManagement>("/api/v1/providers/mlx-audio/management"),
  installMlx: () => request<MlxOperation>("/api/v1/providers/mlx-audio/install", { method: "POST" }),
  uninstallMlx: (confirmed: true) => request<MlxOperation>("/api/v1/providers/mlx-audio/uninstall", {
    method: "POST",
    body: json({ confirmed }),
  }),
  cancelMlxOperation: (id: string) =>
    request<MlxOperation>(`/api/v1/providers/mlx-audio/operations/${id}/cancel`, { method: "POST" }),
  downloadMlxModel: (repository: string, revision: string) =>
    request<MlxOperation>("/api/v1/providers/mlx-audio/models", {
      method: "POST",
      body: json({ repository, revision }),
    }),
  removeMlxModel: (id: string, confirmed: true) =>
    request<void>(`/api/v1/providers/mlx-audio/models/${id}`, {
      method: "DELETE",
      body: json({ confirmed }),
    }),
  estimate: (projectId: string) => request<Estimate>(`/api/v1/projects/${projectId}/preflight/estimate`, { method: "POST" }),
  dryRun: (projectId: string, exportSettings: StartJobRequest["export"]) =>
    request<DryRunResult>(`/api/v1/projects/${projectId}/preflight/dry-run`, {
      method: "POST",
      body: json({ export: exportSettings }),
    }),
  preview: (projectId: string, text?: string) =>
    request<PreviewResult>(`/api/v1/projects/${projectId}/preflight/preview`, {
      method: "POST",
      body: json({ text }),
    }),
  voiceAuditions: (projectId: string, input: VoiceAuditionInput, idempotencyKey?: string) =>
    request<VoiceAuditionResponse>(`/api/v1/projects/${projectId}/voice-auditions`, {
      method: "POST",
      body: json(input),
      idempotencyKey,
    }),
  proofingSummary: (projectId: string) =>
    request<ProofingSummary>(`/api/v1/projects/${projectId}/proofing`),
  proofingSegments: (projectId: string, query: ProofingSegmentQuery = {}) =>
    request<ProofingSegmentPage>(`/api/v1/projects/${projectId}/proofing/segments${proofingQueryString(query)}`),
  updateProofingSegment: (projectId: string, segmentId: string, input: SegmentUpdateInput) =>
    request<ProofingSegmentView>(`/api/v1/projects/${projectId}/proofing/segments/${segmentId}`, {
      method: "PATCH",
      body: json(input),
    }),
  updateProofingReview: (projectId: string, segmentId: string, state: SegmentReviewState, expectedRevision: number) =>
    request<ProofingSegmentView>(`/api/v1/projects/${projectId}/proofing/segments/${segmentId}/review`, {
      method: "PUT",
      body: json({ state, expectedRevision }),
    }),
  proofingTakes: (projectId: string, segmentId: string) =>
    request<SegmentTake[]>(`/api/v1/projects/${projectId}/proofing/segments/${segmentId}/takes`),
  selectProofingTake: (projectId: string, segmentId: string, input: SegmentSelectionInput) =>
    request<ProofingSegmentView>(`/api/v1/projects/${projectId}/proofing/segments/${segmentId}/selection`, {
      method: "PUT",
      body: json(input),
    }),
  proofingRegenerationEstimate: (projectId: string, segmentId: string, expectedSegmentRevision: number) =>
    request<RegenerationEstimate>(`/api/v1/projects/${projectId}/proofing/segments/${segmentId}/regeneration-estimate`, {
      method: "POST",
      body: json({ expectedSegmentRevision }),
    }),
  startProofingRegeneration: (projectId: string, segmentId: string, expectedSegmentRevision: number, estimateToken: string, allowBudgetOverride = false) =>
    request<Job>(`/api/v1/projects/${projectId}/proofing/segments/${segmentId}/regenerations`, {
      method: "POST",
      body: json({ expectedSegmentRevision, estimateToken, allowBudgetOverride }),
    }),
  startProofingExport: (projectId: string, strictRetailer: boolean, exportSettings: StartJobRequest["export"]) =>
    request<Job>(`/api/v1/projects/${projectId}/proofing/exports`, {
      method: "POST",
      body: json({ strictRetailer, export: exportSettings }),
    }),
  distributionPolicies: () =>
    request<{ items: DistributionPolicyView[] }>("/api/v1/distribution/policies"),
  distributionMetadata: (projectId: string) =>
    request<DistributionMetadataView>(`/api/v1/projects/${projectId}/distribution/metadata`),
  updateDistributionMetadata: (projectId: string, expectedRevision: number, metadata: DistributionMetadata) =>
    request<DistributionMetadataView>(`/api/v1/projects/${projectId}/distribution/metadata`, {
      method: "PUT",
      body: json({ expectedRevision, metadata }),
    }),
  distributionPackages: (projectId: string) =>
    request<{ items: DistributionPackageView[] }>(`/api/v1/projects/${projectId}/distribution/packages`),
  createDistributionPackage: (projectId: string, target: DistributionTarget, uploadArtifactIds: string[], reviewArtifactIds: string[] = []) =>
    request<DistributionPackageView>(`/api/v1/projects/${projectId}/distribution/packages`, {
      method: "POST",
      body: json({ target, uploadArtifactIds, reviewArtifactIds }),
    }),
  distributionReports: (packageId: string) =>
    request<{ items: QualityReport[] }>(`/api/v1/distribution/packages/${packageId}/reports`),
  runDistributionQualityControl: (packageId: string) =>
    request<DistributionQualityRun>(`/api/v1/distribution/packages/${packageId}/reports`, {
      method: "POST",
      body: json({}),
    }),
  startJob: (input: StartJobRequest) =>
    request<Job>("/api/v1/jobs", { method: "POST", body: json(input) }),
  jobs: () => request<PageResponse<Job>>("/api/v1/jobs"),
  job: (id: string) => request<Job>(`/api/v1/jobs/${id}`),
  jobAction: (id: string, action: "pause" | "resume" | "cancel" | "retry") =>
    request<Job>(`/api/v1/jobs/${id}/actions/${action}`, { method: "POST" }),
  exports: () => request<PageResponse<ExportArtifact>>("/api/v1/exports"),
  usage: () => request<UsageSummary>("/api/v1/usage/summary"),
  budgets: () => request<PageResponse<Budget>>("/api/v1/budgets"),
  createBudget: (budget: Omit<Budget, "id" | "used" | "reserved">) =>
    request<Budget>("/api/v1/budgets", { method: "POST", body: json(budget) }),
  deleteBudget: (id: string) => request<void>(`/api/v1/budgets/${id}`, { method: "DELETE" }),
  rateCards: () => request<PageResponse<RateCard>>("/api/v1/rate-cards"),
  createRateCard: (input: RateCardInput) =>
    request<RateCard>("/api/v1/rate-cards", { method: "POST", body: json(input) }),
  deleteRateCard: (id: string) => request<void>(`/api/v1/rate-cards/${id}`, { method: "DELETE" }),
  diagnostics: (query: DiagnosticQuery = {}) =>
    request<DiagnosticPage>(`/api/v1/diagnostics${diagnosticQueryString(query)}`),
  downloadDiagnostics,
  settings: () => request<AppSettings>("/api/v1/settings"),
  updateSettings: (patch: Partial<AppSettings>) =>
    request<AppSettings>("/api/v1/settings", { method: "PATCH", body: json(patch) }),
  completeFirstRun: () => request<AppSettings>("/api/v1/settings/first-run", { method: "POST" }),
  secretStatus: () => request<SecretStatus>("/api/v1/secrets/status"),
  unlockSecretStore: (passphrase: string) =>
    request<SecretStatus>("/api/v1/secrets/unlock", { method: "POST", body: json({ passphrase }) }),
  lockSecretStore: () => request<void>("/api/v1/secrets/lock", { method: "POST" }),
  revokeLanSessions: () => request<void>("/api/v1/settings/lan/sessions", { method: "DELETE" }),
  setLanPassword: (password: string) =>
    request<void>("/api/v1/settings/lan/password", { method: "PUT", body: json({ password }) }),
  lanTokens: () => request<LanApiToken[]>("/api/v1/settings/lan/tokens"),
  createLanToken: (name: string) =>
    request<IssuedLanApiToken>("/api/v1/settings/lan/tokens", { method: "POST", body: json({ name }) }),
  revokeLanToken: (id: string) =>
    request<void>(`/api/v1/settings/lan/tokens/${id}`, { method: "DELETE" }),
};

export function jobEventsUrl(jobId: string): string {
  return `${baseUrl}/api/v1/jobs/${jobId}/events`;
}

export function playbackSocketUrl(jobId: string): string {
  const origin = baseUrl || window.location.origin;
  const url = new URL(`/api/v1/jobs/${jobId}/playback`, origin);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.toString();
}

export function artifactUrl(artifactId: string): string {
  return `${baseUrl}/api/v1/artifacts/${artifactId}`;
}

export function distributionReportHtmlUrl(reportId: string): string {
  return `${baseUrl}/api/v1/distribution/reports/${reportId}/html`;
}
