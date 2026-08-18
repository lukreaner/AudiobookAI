export type Id = string;

export interface ProblemDetails {
  type: string;
  title: string;
  status: number;
  detail?: string;
  instance?: string;
  code?: string;
  errors?: Record<string, string[]>;
  meta?: Record<string, unknown>;
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
  characterRevision: number;
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
  canonicalName: string;
  aliases: string[];
  expectedCharacterRevision: number;
}

export interface Character {
  id: Id;
  role: "narrator" | "character";
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
  performance: PerformanceSettings;
  timing: TimingSettings;
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
  expectedCharacterRevision: number;
}

export interface CharacterPage extends PageResponse<Character> {
  characterRevision: number;
}

export interface CharacterMutationResult {
  character?: Character;
  removedCharacterId?: Id;
  inheritedVoice?: boolean;
  characterRevision: number;
}

export interface CharacterDetectionStatus {
  activeJob?: Job;
  latestJob?: Job;
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
  | "piper"
  | "native_os"
  | "openai_tts"
  | "openai"
  | "openai_compatible"
  | "qwen"
  | "kimi"
  | "moonshot"
  | "lm_studio"
  | "anthropic"
  | "gemini"
  | "ollama";

/** The single workload a provider connection is allowed to serve. */
export type ProviderRole = "tts" | "llm";

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
  /** Exact-model descriptors; an absent model or control is unsupported. */
  modelPerformance: ModelPerformanceCapabilities[];
}

export interface PerformanceRange {
  minimum: number;
  maximum: number;
}

/** Nested domain capability objects retain the backend's snake_case wire names. */
export interface PerformanceCapabilities {
  speed?: PerformanceRange | null;
  pitch?: PerformanceRange | null;
  stability?: PerformanceRange | null;
  similarity?: PerformanceRange | null;
  style?: PerformanceRange | null;
  speaker_boost: boolean;
  delivery_cues: DeliveryCue[];
}

export interface ModelPerformanceCapabilities {
  model: string;
  performance: PerformanceCapabilities;
}

export interface ProviderProfile {
  id: Id;
  name: string;
  kind: ProviderKind;
  role: ProviderRole;
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

export interface NativeProviderAvailability {
  platform: "linux" | "macos" | "windows" | "unsupported";
  providerName: string;
  available: boolean;
  detail: string | null;
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

export interface ProviderProfilePatchInput {
  name?: string;
  kind?: ProviderKind;
  role?: ProviderRole;
  mode?: ProviderProfile["mode"];
  endpoint?: string | null;
  executablePath?: string | null;
  workingDirectory?: string | null;
  arguments?: string[];
  model?: string | null;
  credential?: string;
}

export interface ProviderProfileCreateInput extends ProviderProfilePatchInput {
  kind: ProviderKind;
  role: ProviderRole;
}

export interface AvailableProviderModel {
  id: string;
  name: string;
}

export interface AvailableProviderModels {
  items: AvailableProviderModel[];
  /** When true, every returned item is positively verified for the requested provider role. */
  strict: boolean;
}

export interface ProviderModelDiscoveryInput extends ProviderProfileCreateInput {
  providerId?: Id;
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
  installerStatus: "ready" | "unsupported_platform" | "not_bundled" | "payload_missing" | "unsafe_filesystem" | "invalid_metadata" | "incomplete";
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

export type PiperOperationState = MlxOperationState;

export interface PiperOperation {
  id: Id;
  kind: "install" | "uninstall" | "download_voice" | "remove_voice";
  state: PiperOperationState;
  progressPercent: number;
  phase: string;
  message: string;
  voiceId?: Id;
  bytesDownloaded?: number;
  bytesTotal?: number;
  startedAt: string;
  finishedAt?: string;
}

export interface PiperCatalogVoice {
  id: Id;
  name: string;
  language: string;
  quality: string;
  speakers: number;
  sampleRate: number;
  sizeBytes: number;
  license: string;
  licenseUrl: string;
  licenseSummary: string;
  modelCardUrl: string;
  sourceUrl: string;
}

export interface PiperInstalledVoice {
  id: Id;
  name: string;
  language: string;
  quality: string;
  modelPath: string;
  configPath: string;
  sizeBytes: number;
  license: string;
  installedAt: string;
}

export interface PiperVoiceIssue {
  id: Id;
  status: "incomplete" | "unsafe_filesystem";
  removable: boolean;
  detail: string;
}

export interface PiperManagement {
  supported: boolean;
  supportDetail: string;
  installerStatus: MlxManagement["installerStatus"];
  installed: boolean;
  installedVersion?: string;
  executablePath?: string;
  voicesDir?: string;
  catalog: PiperCatalogVoice[];
  installedVoices: PiperInstalledVoice[];
  voiceIssues: PiperVoiceIssue[];
  activeOperation?: PiperOperation;
  lastOperation?: PiperOperation;
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

export type DeliveryCue = "whisper" | "shout" | "sarcastic" | "curious" | "excited" | "crying" | "mischievous";

/** Domain objects below intentionally use the service's persisted snake_case wire format. */
export interface PerformanceSettings {
  speed?: number;
  pitch?: number;
  stability?: number;
  similarity?: number;
  style?: number;
  speaker_boost?: boolean;
  delivery_cue?: DeliveryCue;
}

export interface TimingSettings {
  pause_before_ms?: number;
  pause_after_ms?: number;
}

export type SegmentSpeaker =
  | { kind: "narrator" }
  | { kind: "character"; id: Id }
  | { kind: "named"; id: string };

export type SegmentReviewState = "unreviewed" | "flagged" | "approved" | "locked";

export interface ProductionSegment {
  id: Id;
  project_id: Id;
  chapter_id?: Id | null;
  paragraph_id?: Id | null;
  source: "epub_range" | "opening_credit" | "closing_credit";
  stable_key: string;
  ordinal: number;
  source_content_hash: string;
  byte_start?: number | null;
  byte_end?: number | null;
  speaker: SegmentSpeaker;
  original_text: string;
  narration_text_override?: string | null;
  effective_text: string;
  context_before?: string | null;
  context_after?: string | null;
  performance_override: PerformanceSettings;
  timing_override: TimingSettings;
  expected_input_hash: string;
  review_state: SegmentReviewState;
  active: boolean;
  revision: number;
  created_at: string;
  updated_at: string;
}

export interface ProofingPlan {
  project_id: Id;
  source_conversion_job_id: Id;
  plan_revision: number;
  plan_hash: string;
  status: "ready" | "dirty" | "incomplete";
  dirty_reasons: string[];
  created_at: string;
  updated_at: string;
}

export interface TakeFinding {
  code: string;
  severity: "warning" | "error";
  message: string;
  start_ms?: number | null;
  end_ms?: number | null;
  actual?: number | null;
  expected?: string | null;
}

export interface SegmentTake {
  id: Id;
  segment_id: Id;
  artifact_id: Id;
  ordinal: number;
  source_job_id: Id;
  source_job_unit_id: Id;
  semantic_input_hash: string;
  duration_ms: number;
  provider_profile_id?: Id | null;
  model?: string | null;
  voice_profile_id?: Id | null;
  dictionary_revision_hash: string;
  normalization_version: string;
  synthesis_provenance: Record<string, unknown>;
  findings: TakeFinding[];
  created_at: string;
}

export interface SegmentSelection {
  segment_id: Id;
  take_id: Id;
  selected_at: string;
  revision: number;
}

export interface ProofingCounts {
  total: number;
  unreviewed: number;
  flagged: number;
  approved: number;
  locked: number;
  stale: number;
  missing: number;
}

export interface ProofingSummary {
  available: boolean;
  requiresNewConversion: boolean;
  plan?: ProofingPlan | null;
  counts: ProofingCounts;
  chapters: { id: Id; title: string; total: number; issueCount: number }[];
  retailerExportReady: boolean;
  genericExportReady: boolean;
}

export interface ProofingSegmentView {
  segment: ProductionSegment;
  selection?: SegmentSelection | null;
  selectedTake?: SegmentTake | null;
  takeCount: number;
  selectedTakeCurrent: boolean;
  audioUrl?: string | null;
}

export interface ProofingSegmentQuery {
  chapterId?: Id;
  state?: SegmentReviewState;
  issuesOnly?: boolean;
  staleOnly?: boolean;
  search?: string;
  cursor?: string;
  limit?: number;
}

export interface ProofingSegmentPage {
  items: ProofingSegmentView[];
  total: number;
  nextCursor?: string | null;
}

export interface SegmentUpdateInput {
  expectedRevision: number;
  textOverride?: string;
  clearTextOverride?: boolean;
  performanceOverride?: PerformanceSettings;
  timingOverride?: TimingSettings;
}

export interface SegmentSelectionInput {
  takeId: Id;
  expectedRevision: number;
  expectedSegmentRevision: number;
}

export interface RegenerationEstimate {
  segmentId: Id;
  segmentRevision: number;
  providerProfileId: Id;
  providerName: string;
  model?: string | null;
  characters: number;
  monetaryCostMicros?: number | null;
  currency?: string | null;
  credits?: number | null;
  unknownPricing: boolean;
  estimateToken: string;
  expiresAt: string;
}

export interface VoiceAuditionCandidateInput {
  candidateId: string;
  providerProfileId: Id;
  voiceId: Id;
  model?: string;
  performance: PerformanceSettings;
}

export interface VoiceAuditionInput {
  text?: string;
  characterId?: Id;
  confirmBillable: boolean;
  candidates: VoiceAuditionCandidateInput[];
}

export interface VoiceAuditionResult {
  candidateId: string;
  providerProfileId: Id;
  voiceId: Id;
  preview?: PreviewResult | null;
  error?: string | null;
}

export interface VoiceAuditionResponse {
  results: VoiceAuditionResult[];
  potentiallyBillable: boolean;
}

export type DistributionTarget = "generic_m4b" | "acx" | "spotify_for_authors" | "google_play";

export interface DistributionPolicyRule {
  code: string;
  level: "required" | "recommended" | "manual_gate";
  automated: boolean;
  expected: unknown;
  description: string;
}

export interface DistributionPolicyView {
  target: DistributionTarget;
  policyVersion: string;
  effectiveDate: string;
  sourceUrls: string[];
  displayName: string;
  rules: DistributionPolicyRule[];
}

export interface ManualAttestations {
  acx_external_authorization?: string | null;
  acx_authorization_reference?: string | null;
  spotify_digital_voice_disclosure?: string | null;
  rights_and_eligibility_confirmed?: string | null;
}

export interface DistributionMetadata {
  subtitle?: string | null;
  authors: string[];
  narrators: string[];
  publisher?: string | null;
  imprint?: string | null;
  description?: string | null;
  language?: string | null;
  abridged?: boolean | null;
  identifier?: string | null;
  identifier_kind?: string | null;
  source_rights?: string | null;
  audio_rights?: string | null;
  release_date?: string | null;
  cover_artifact_id?: Id | null;
  opening_credit_segment_ids: Id[];
  closing_credit_segment_ids: Id[];
  sample_segment_ids: Id[];
  attestations: ManualAttestations;
}

export interface DistributionMetadataView {
  revision: number;
  updatedAt?: string | null;
  metadata: DistributionMetadata;
}

export interface DistributionPolicyRef {
  target: DistributionTarget;
  policy_version: string;
  effective_date: string;
  source_urls: string[];
}

export interface QualityFinding {
  code: string;
  status: "pass" | "warning" | "fail" | "manual";
  scope: string;
  message: string;
  actual?: unknown | null;
  expected?: unknown | null;
  start_ms?: number | null;
  end_ms?: number | null;
  remediation?: string | null;
  acknowledged: boolean;
}

export interface QualityReport {
  id: Id;
  package_id: Id;
  policy: DistributionPolicyRef;
  policy_digest: string;
  policy_snapshot?: unknown | null;
  metadata_revision: number;
  metadata_digest: string;
  metadata_snapshot?: DistributionMetadata | null;
  project_title?: string | null;
  package_digest: string;
  package_snapshot?: DistributionPackage | null;
  export_manifest_artifact_id?: Id | null;
  segment_evidence: DistributionSegmentEvidence[];
  technical_ready: boolean;
  submission_ready: boolean;
  findings: QualityFinding[];
  analyzer_version: string;
  ffmpeg_version: string;
  ffmpeg_build_fingerprint: string;
  file_hashes: Record<string, string>;
  generated_at: string;
}

export interface DistributionSegmentEvidence {
  segment_id: Id;
  source?: ProductionSegment["source"] | null;
  active: boolean;
  selection_revision?: number | null;
  take_id?: Id | null;
  take_artifact_id?: Id | null;
  expected_input_hash?: string | null;
  selected_take_input_hash?: string | null;
  current_input_hash?: string | null;
  current: boolean;
  problem?: string | null;
}

export interface DistributionPackage {
  id: Id;
  project_id: Id;
  job_id: Id;
  target: DistributionTarget;
  output_directory: string;
  upload_artifact_ids: Id[];
  review_artifact_ids: Id[];
  quality_report_id?: Id | null;
  created_at: string;
}

export interface DistributionPackageView {
  package: DistributionPackage;
  latestReport?: QualityReport | null;
  latestReportCurrent: boolean;
}

export interface DistributionQualityRun {
  package: DistributionPackage;
  report: QualityReport;
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
  stage: "detect" | "synthesize" | "assemble" | "mix" | "normalize" | "export" | "quality_control";
  status: "queued" | "running" | "paused" | "complete" | "failed" | "cancelled";
  progress: number;
  attempt: number;
  lastError?: string;
}

export interface Job {
  id: Id;
  projectId: Id;
  projectTitle: string;
  kind: "character_detection" | "preview" | "conversion" | "segment_regeneration" | "export" | "quality_control" | "cache_cleanup";
  status: "queued" | "running" | "pausing" | "paused" | "cancelling" | "complete" | "failed" | "cancelled";
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
  jobId: Id;
  partIndex: number;
  partCount: number;
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
