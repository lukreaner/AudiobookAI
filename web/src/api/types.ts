export type Id = string;

export interface ProblemDetails {
  type: string;
  title: string;
  status: number;
  detail?: string;
  instance?: string;
  code?: string;
  errors?: Record<string, string[]>;
}

export interface HealthStatus {
  status: "ready" | "starting" | "degraded";
  version: string;
  database: "ready" | "migrating" | "unavailable";
}

export interface BookSummary {
  id: Id;
  title: string;
  author?: string;
  coverUrl?: string;
  chapterCount: number;
  selectedChapterCount: number;
  durationSeconds?: number;
  progress: number;
  status: "draft" | "ready" | "processing" | "completed" | "failed";
  updatedAt: string;
  language?: string;
  series?: string;
  seriesPosition?: number;
}

export interface Chapter {
  id: Id;
  index: number;
  title: string;
  selected: boolean;
  wordCount: number;
  characterCount: number;
  estimatedSeconds?: number;
  status: "pending" | "cached" | "processing" | "complete" | "failed";
}

export interface ProjectDetail extends BookSummary {
  narrator?: string;
  publisher?: string;
  description?: string;
  consentCloudText: boolean;
  consentCloudAudio: boolean;
  chapters: Chapter[];
  characterReviewStatus: "not_started" | "needs_review" | "approved";
  outputName?: string;
}

export interface DialogueEvidence {
  id: Id;
  paragraphId: Id;
  chapterId: Id;
  chapterTitle: string;
  excerpt: string;
  confidence: number;
  startOffset: number;
  endOffset: number;
  speakerOverride?: SpeakerOverrideView | Id | null;
  speakerOverrideActive?: boolean;
}

export interface SpeakerOverrideView {
  characterId: Id | null;
  characterName?: string;
  startOffset: number;
  endOffset: number;
}

export interface SpeakerOverrideInput {
  characterId: Id | null;
  startOffset: number;
  endOffset: number;
}

export interface CharacterIdentityInput {
  name: string;
  aliases: string[];
}

export interface Character {
  id: Id;
  canonicalName: string;
  aliases: string[];
  confidence: number;
  dialogueCount: number;
  voiceAssignment?: VoiceAssignment;
  evidence: DialogueEvidence[];
}

export interface VoiceAssignment {
  providerProfileId: Id;
  providerName: string;
  voiceId: Id;
  voiceName: string;
  model?: string;
}

export type DetectionTemperature =
  | { mode: "default" }
  | { mode: "null" }
  | { mode: "value"; value: number };

export type DetectionReasoning =
  | { mode: "inherit" }
  | { mode: "disabled" }
  | { mode: "effort"; effort: "minimal" | "low" | "medium" | "high" }
  | { mode: "adaptive" }
  | { mode: "token_budget"; tokens: number };

export interface CharacterDetectionInput {
  providerProfileId: Id;
  temperature: DetectionTemperature;
  reasoning: DetectionReasoning;
}

export interface Voice {
  id: Id;
  providerProfileId: Id;
  name: string;
  locale?: string;
  gender?: string;
  kind: "catalog" | "local_reference" | "remote_clone" | "native";
  owned: boolean;
  previewUrl?: string;
}

export interface VoiceCloneInput {
  name: string;
  description?: string;
  projectId: Id;
  referenceAudio: File[];
}

export interface PronunciationRule {
  id: Id;
  projectId?: Id;
  scope: "global" | "project";
  kind: "literal" | "whole_word" | "regex" | "alias" | "phoneme";
  source: string;
  replacement: string;
  language?: string;
  characterId?: Id;
  caseSensitive: boolean;
  enabled: boolean;
  order: number;
  conflict?: string;
}

export interface PronunciationPreviewInput {
  text: string;
  projectId?: Id;
  characterId?: Id;
  language?: string;
}

export interface PronunciationPreviewResult {
  originalText: string;
  transformedText: string;
  appliedRuleIds: Id[];
  conflicts: { ruleId: Id; detail: string }[];
}

export type ProviderKind =
  | "elevenlabs"
  | "mlx_audio"
  | "localai"
  | "alltalk_v2"
  | "native_os"
  | "openai"
  | "openai_compatible"
  | "qwen"
  | "kimi"
  | "moonshot"
  | "lm_studio"
  | "anthropic"
  | "gemini"
  | "ollama";

export interface ProviderCapabilities {
  tts: boolean;
  characterDetection: boolean;
  streaming: boolean;
  voiceCloning: boolean;
  pronunciation: boolean;
  processControl: boolean;
  modelControl: boolean;
  modelList: boolean;
  modelDownload: boolean;
  modelDelete: boolean;
  modelLoad: boolean;
  modelUnload: boolean;
  modelSwitch: boolean;
  temperature: "unsupported" | "number" | "nullable";
  reasoning: string[];
  maxConcurrency?: number;
}

export interface ProviderProfile {
  id: Id;
  name: string;
  kind: ProviderKind;
  mode: "cloud_remote" | "external_endpoint" | "managed_child" | "native";
  endpoint?: string;
  executablePath?: string;
  workingDirectory?: string;
  arguments: string[];
  status: "online" | "offline" | "starting" | "stopping" | "error" | "unconfigured";
  model?: string;
  credentialConfigured: boolean;
  capabilities?: ProviderCapabilities;
  capabilitySource?: string;
  capabilityUpdatedAt?: string;
  lastError?: string;
}

export interface ProviderModel {
  id: string;
  name: string;
  sizeBytes?: number;
  format?: string;
  family?: string;
  parameterSize?: string;
  quantization?: string;
  loadedInstances: string[];
}

export type ProviderModelOperationState = "running" | "cancelling" | "succeeded" | "failed" | "cancelled";

export interface ProviderModelOperation {
  id: Id;
  providerProfileId: Id;
  model: string;
  state: ProviderModelOperationState;
  downloadedBytes?: number;
  totalSizeBytes?: number;
  bytesPerSecond?: number;
  progressPercent?: number;
  detailCode?: string;
  startedAt: string;
  finishedAt?: string;
}

export interface ProviderModelLibrary {
  models: ProviderModel[];
  modelsErrorCode?: string;
  operations: ProviderModelOperation[];
}

export interface ProviderProfileInput {
  name?: string;
  kind?: ProviderKind;
  mode?: ProviderProfile["mode"];
  endpoint?: string | null;
  executablePath?: string | null;
  workingDirectory?: string | null;
  arguments?: string[];
  model?: string | null;
  credential?: string;
}

export interface ProviderLogLine {
  timestamp: string;
  stream: "stdout" | "stderr";
  line: string;
}

export interface ProviderLogsResponse {
  providerId: Id;
  logs: ProviderLogLine[];
}

export type MlxOperationState = "queued" | "running" | "cancelling" | "succeeded" | "failed" | "cancelled";

export interface MlxOperation {
  id: Id;
  kind: "install" | "uninstall" | "download_model";
  state: MlxOperationState;
  progressPercent: number;
  phase: string;
  message: string;
  modelId?: Id;
  exitCode?: number | null;
  diagnostics?: string[];
  startedAt: string;
  finishedAt?: string;
}

export interface MlxManagedModel {
  id: Id;
  repository: string;
  revision: string;
  resolvedCommit?: string;
  localPath: string;
  state: "downloading" | "ready" | "failed";
  bytes?: number;
  createdAt: string;
}

export interface MlxManagement {
  supported: boolean;
  supportDetail: string;
  uvAvailable: boolean;
  requiredUvVersion: string;
  installerPayloadAvailable: boolean;
  installed: boolean;
  installedVersion?: string;
  serverExecutable?: string;
  models: MlxManagedModel[];
  activeOperation?: MlxOperation;
  lastOperation?: MlxOperation;
  profileActionRequired: boolean;
}

export interface Estimate {
  selectedChapters: number;
  characters: number;
  estimatedTokens?: number;
  estimatedDurationSeconds: number;
  estimatedDiskBytes: number;
  estimatedCompletionSecondsLow?: number;
  estimatedCompletionSecondsHigh?: number;
  monetaryCostMicros?: number;
  currency?: string;
  credits?: number;
  priceSource?: string;
  priceEffectiveAt?: string;
  providerEstimates: ProviderEstimate[];
  unknownFields: string[];
}

export interface ProviderEstimate {
  providerProfileId: Id;
  providerName: string;
  model?: string;
  characters: number;
  estimatedDurationSeconds: number;
  monetaryCostMicros?: number;
  currency?: string;
  credits?: number;
  rateCardId?: Id;
  priceSource?: string;
  priceEffectiveAt?: string;
}

export interface DryRunCheck {
  id: string;
  label: string;
  status: "pass" | "warning" | "fail" | "pending";
  detail: string;
  action?: string;
}

export interface DryRunResult {
  ready: boolean;
  checkedAt: string;
  checks: DryRunCheck[];
}

export interface PreviewResult {
  artifactId: Id;
  audioUrl: string;
  text: string;
  durationSeconds: number;
  billable: boolean;
  cached: boolean;
}

export type ExportFormat = "mp3" | "wav" | "m4a" | "m4b";

export interface JobExportSettings {
  format: ExportFormat;
  splitPerChapter: boolean;
  outputDirectory?: string;
  fileName?: string;
  bitrateKbps: number;
  backgroundMusicPath?: string;
  confirmBackgroundMusicOwned: boolean;
  musicGainDb: number;
  ducking: boolean;
}

export interface StartJobRequest {
  projectId: Id;
  allowBudgetOverride: boolean;
  export: JobExportSettings;
}

export interface JobUnit {
  id: Id;
  title: string;
  stage: "detect" | "synthesize" | "assemble" | "mix" | "normalize" | "export";
  status: "queued" | "running" | "paused" | "complete" | "failed" | "cancelled";
  progress: number;
  attempt: number;
  lastError?: string;
}

export interface Job {
  id: Id;
  projectId: Id;
  projectTitle: string;
  status: "queued" | "running" | "pausing" | "paused" | "complete" | "failed" | "cancelled";
  progress: number;
  currentStage?: string;
  startedAt?: string;
  updatedAt: string;
  estimatedRemainingSeconds?: number;
  units: JobUnit[];
  progressivePlaybackUrl?: string;
  uncertainCharge: boolean;
}

export interface ExportArtifact {
  id: Id;
  projectId: Id;
  projectTitle: string;
  format: "mp3" | "wav" | "m4a" | "m4b";
  splitMode: "single" | "per_chapter";
  fileName: string;
  sizeBytes: number;
  durationSeconds: number;
  createdAt: string;
  downloadUrl: string;
  manifestUrl: string;
  chapterMarkers: boolean;
}

export interface UsageSummary {
  periodStart: string;
  periodEnd: string;
  currency?: string;
  monetaryCostMicros?: number;
  characters?: number;
  inputTokens?: number;
  outputTokens?: number;
  credits?: number;
  unknownCostRequests: number;
  rows: UsageRow[];
}

export interface UsageRow {
  id: Id;
  occurredAt: string;
  projectTitle?: string;
  providerName: string;
  operation: "tts" | "character_detection";
  model?: string;
  voice?: string;
  characters?: number;
  inputTokens?: number;
  outputTokens?: number;
  costMicros?: number;
  currency?: string;
  provenance: "reported" | "estimated" | "unknown";
  requestId?: string;
}

export interface Budget {
  id: Id;
  name: string;
  providerProfileId?: Id;
  period: "job" | "daily" | "monthly" | "lifetime";
  metric: "money" | "tokens" | "characters" | "credits";
  limit: number;
  used: number;
  reserved: number;
  hard: boolean;
  currency?: string;
  warningPercent: number;
}

export interface RateCard {
  id: Id;
  providerProfileId: Id;
  model?: string;
  workload: "tts" | "character_detection";
  currency: string;
  effectiveAt: string;
  expiresAt?: string;
  source: string;
  sourceUrl?: string;
  pricing: Record<string, number>;
  userOverridden: boolean;
}

export interface RateCardInput {
  providerProfileId: Id;
  model?: string;
  workload: RateCard["workload"];
  currency: string;
  effectiveAt?: string;
  expiresAt?: string;
  source: string;
  sourceUrl?: string;
  pricing: Record<string, number>;
}

export type DiagnosticLevel = "trace" | "debug" | "info" | "warn" | "error";

export interface DiagnosticEntry {
  sequence: number;
  timestamp: string;
  level: DiagnosticLevel;
  target: string;
  message: string;
  fields: Record<string, string | number | boolean>;
}

export interface DiagnosticQuery {
  level?: DiagnosticLevel;
  target?: string;
  search?: string;
  after?: number;
  limit?: number;
}

export interface DiagnosticPage {
  items: DiagnosticEntry[];
  total: number;
  latestSequence: number;
}

export interface AppSettings {
  language: "en" | "de";
  theme: "system" | "light" | "dark";
  libraryPath: string;
  cachePath: string;
  cacheLimitBytes: number;
  defaultConcurrency: number;
  defaultRetryCount: number;
  defaultLufs: number;
  defaultTruePeakDb: number;
  closeToTray: boolean;
  checkForUpdates: boolean;
  lan: {
    enabled: boolean;
    tls: boolean;
    insecureHttpConfirmed: boolean;
    bindAddress: string;
    port: number;
    certificateChainPath: string;
    privateKeyPath: string;
    advertisedHosts: string[];
    passwordConfigured: boolean;
    apiTokenCount: number;
    activeSessions: number;
    restartRequired: boolean;
  };
  secretStore: "keychain" | "passphrase" | "locked";
  firstRunComplete: boolean;
}

export interface SecretStatus {
  unlocked: boolean;
  backend: "keychain" | "passphrase" | "locked";
}

export interface LanApiToken {
  id: string;
  label: string;
  createdAt: string;
  lastUsedAt?: string;
}

export interface IssuedLanApiToken extends LanApiToken {
  token: string;
}

export interface PageResponse<T> {
  items: T[];
  nextCursor?: string;
  total?: number;
}

export interface ImportDraft {
  draftId: Id;
  sourceName: string;
  title: string;
  author?: string;
  language?: string;
  coverUrl?: string;
  chapters: Chapter[];
  warnings: string[];
}
