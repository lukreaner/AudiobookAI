import type { Character, DialogueEvidence, Id, SpeakerOverrideInput } from "../api/types";

export const AUTO_SPEAKER = "__auto__";
export const NARRATOR_SPEAKER = "__narrator__";

export function parseAliases(value: string, canonicalName = ""): string[] {
  const canonical = canonicalName.trim().toLocaleLowerCase();
  const seen = new Set<string>();
  return value
    .split(/[\n,]/)
    .map((alias) => alias.trim())
    .filter((alias) => {
      if (!alias) return false;
      const normalized = alias.toLocaleLowerCase();
      if (normalized === canonical || seen.has(normalized)) return false;
      seen.add(normalized);
      return true;
    });
}

export function paragraphIdFor(evidence: DialogueEvidence): Id {
  return evidence.paragraphId;
}

export function storedSpeakerSelection(evidence: DialogueEvidence, characters: Character[]): string {
  const override = evidence.speakerOverride;
  if (typeof override === "object" && override !== null) {
    return override.characterId ?? NARRATOR_SPEAKER;
  }
  if (typeof override === "string") {
    if (override === NARRATOR_SPEAKER || override.toLocaleLowerCase() === "narrator") return NARRATOR_SPEAKER;
    return characters.find((character) =>
      character.id === override || character.canonicalName.toLocaleLowerCase() === override.toLocaleLowerCase()
    )?.id ?? AUTO_SPEAKER;
  }
  if (override === null && evidence.speakerOverrideActive) return NARRATOR_SPEAKER;
  return AUTO_SPEAKER;
}

export function speakerOverrideInput(
  evidence: DialogueEvidence,
  selection: string,
): SpeakerOverrideInput {
  return {
    characterId: selection === NARRATOR_SPEAKER ? null : selection,
    startOffset: evidence.startOffset,
    endOffset: evidence.endOffset,
  };
}
