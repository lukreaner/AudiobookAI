import type { JobExportSettings } from "../api/types";

export interface ExportFormState extends Omit<JobExportSettings, "outputDirectory" | "fileName" | "backgroundMusicPath"> {
  outputDirectory: string;
  fileName: string;
  backgroundMusicPath: string;
}

export const DEFAULT_EXPORT_SETTINGS: ExportFormState = {
  format: "m4b",
  splitPerChapter: false,
  outputDirectory: "",
  fileName: "",
  bitrateKbps: 128,
  backgroundMusicPath: "",
  confirmBackgroundMusicOwned: false,
  musicGainDb: -24,
  ducking: true,
};

export function requiresMusicOwnership(settings: ExportFormState): boolean {
  return Boolean(settings.backgroundMusicPath.trim()) && !settings.confirmBackgroundMusicOwned;
}

export function toJobExportSettings(settings: ExportFormState): JobExportSettings {
  const outputDirectory = settings.outputDirectory.trim();
  const fileName = settings.fileName.trim();
  const backgroundMusicPath = settings.backgroundMusicPath.trim();

  return {
    format: settings.format,
    splitPerChapter: settings.splitPerChapter,
    ...(outputDirectory ? { outputDirectory } : {}),
    ...(fileName ? { fileName } : {}),
    bitrateKbps: settings.bitrateKbps,
    ...(backgroundMusicPath ? { backgroundMusicPath } : {}),
    confirmBackgroundMusicOwned: backgroundMusicPath ? settings.confirmBackgroundMusicOwned : false,
    musicGainDb: settings.musicGainDb,
    ducking: settings.ducking,
  };
}

const EXPORT_FORMATS: ExportFormState["format"][] = ["m4b", "mp3", "m4a", "wav"];

function storageKey(projectId: string): string {
  return `audiobookai.exportSettings.${projectId}`;
}

/** Restores the export settings last used for a project, falling back to the defaults field by field. */
export function readExportSettings(projectId: string): ExportFormState {
  const settings = { ...DEFAULT_EXPORT_SETTINGS };
  try {
    const raw = localStorage.getItem(storageKey(projectId));
    const value: unknown = raw ? JSON.parse(raw) : undefined;
    if (!value || typeof value !== "object") return settings;
    const stored = value as Record<string, unknown>;
    if (EXPORT_FORMATS.includes(stored.format as ExportFormState["format"])) settings.format = stored.format as ExportFormState["format"];
    if (typeof stored.splitPerChapter === "boolean") settings.splitPerChapter = stored.splitPerChapter;
    if (typeof stored.outputDirectory === "string") settings.outputDirectory = stored.outputDirectory;
    if (typeof stored.fileName === "string") settings.fileName = stored.fileName;
    if (typeof stored.bitrateKbps === "number" && Number.isFinite(stored.bitrateKbps)) settings.bitrateKbps = stored.bitrateKbps;
    if (typeof stored.backgroundMusicPath === "string") settings.backgroundMusicPath = stored.backgroundMusicPath;
    if (typeof stored.musicGainDb === "number" && Number.isFinite(stored.musicGainDb)) settings.musicGainDb = stored.musicGainDb;
    if (typeof stored.ducking === "boolean") settings.ducking = stored.ducking;
    return settings;
  } catch {
    return settings;
  }
}

export function writeExportSettings(projectId: string, settings: ExportFormState): void {
  // The music ownership confirmation is a per-run statement and is never remembered.
  const { confirmBackgroundMusicOwned: _confirmation, ...remembered } = settings;
  try {
    localStorage.setItem(storageKey(projectId), JSON.stringify(remembered));
  } catch {
    // Remembering the settings is a convenience; export works without it.
  }
}

export function forgetExportSettings(projectId: string): void {
  try {
    localStorage.removeItem(storageKey(projectId));
  } catch {
    // Nothing to clean up when storage is unavailable.
  }
}
