import type { TFunction } from "i18next";

/**
 * The service reports a job's current stage either as a stable stage code (`synthesize`) or as a
 * short English status sentence. Known sentences are translated here; anything unrecognized is
 * shown unchanged so new service messages never disappear.
 */
const exactStages: Record<string, string> = {
  "Queued for character detection": "stageText.queuedDetection",
  "Queued for conversion": "stageText.queuedConversion",
  "Queued for retry": "stageText.queuedRetry",
  "Queued segment regeneration": "stageText.queuedRegeneration",
  "Starting conversion": "stageText.startingConversion",
  "Character review required": "stageText.characterReviewRequired",
  "Paused": "stageText.paused",
  "Paused after restart": "stageText.pausedAfterRestart",
  "Paused at a character-detection batch boundary": "stageText.pausedAtDetectionBatch",
  "Cancelled": "stageText.cancelled",
  "Cancelling after the active request": "stageText.cancellingAfterRequest",
  "Synthesizing": "stageText.synthesizing",
  "Synthesizing billable preview": "stageText.synthesizingPreview",
  "Export audiobook": "stageText.exportAudiobook",
  "Normalize audio": "stageText.normalizeAudio",
};

const patternStages: [RegExp, string, (match: RegExpMatchArray) => Record<string, string>][] = [
  [/^Detection batch (\d+) of (\d+)$/, "stageText.detectionBatch", (match) => ({ current: match[1], total: match[2] })],
  [/^Synthesizing (.+) with (.+)$/, "stageText.synthesizingWith", (match) => ({ item: match[1], provider: match[2] })],
  [/^Assembling (.+)$/, "stageText.assembling", (match) => ({ chapter: match[1] })],
];

export function localizeJobStage(stage: string, t: TFunction): string {
  const exact = exactStages[stage];
  if (exact) return t(exact);
  for (const [pattern, key, values] of patternStages) {
    const match = stage.match(pattern);
    if (match) return t(key, values(match));
  }
  return t(`stage.${stage}`, { defaultValue: stage });
}
