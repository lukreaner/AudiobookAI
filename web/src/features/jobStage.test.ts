import { describe, expect, it } from "vitest";
import i18n from "../i18n";
import { localizeJobStage } from "./jobStage";

describe("job stage localization", () => {
  it("translates known service messages, including patterned ones", async () => {
    await i18n.changeLanguage("de");
    const t = i18n.t.bind(i18n);
    expect(localizeJobStage("Character review required", t)).toBe("Figurenprüfung erforderlich");
    expect(localizeJobStage("Detection batch 3 of 12", t)).toBe("Erkennungsabschnitt 3 von 12");
    expect(localizeJobStage("synthesize", t)).toBe("Sprachsynthese");
  });

  it("keeps unknown service messages visible", async () => {
    await i18n.changeLanguage("de");
    expect(localizeJobStage("A brand-new service stage", i18n.t.bind(i18n))).toBe("A brand-new service stage");
  });
});
