import { describe, expect, it } from "vitest";
import { activeJobCount } from "./updatePolicy";

describe("update installation policy", () => {
  it("blocks queued, running, pausing, and paused jobs", () => {
    expect(activeJobCount([
      { status: "queued" },
      { status: "running" },
      { status: "pausing" },
      { status: "paused" },
    ])).toBe(4);
  });

  it("allows installation when every job is terminal", () => {
    expect(activeJobCount([
      { status: "complete" },
      { status: "failed" },
      { status: "cancelled" },
    ])).toBe(0);
  });
});
