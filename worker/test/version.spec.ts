import { describe, expect, it } from "vitest";
import { clientVersionAtLeast } from "../src/service";

describe("minimum EPI client version", () => {
  it.each([
    ["0.4.9", "0.5.0", false],
    ["0.5.0-alpha.1", "0.5.0", false],
    ["0.5.0", "0.5.0", true],
    ["0.5.0+build.7", "0.5.0", true],
    ["0.6.0", "0.5.0", true],
    ["00.5.0", "0.5.0", false],
  ])("compares %s against %s", (version, minimum, expected) => {
    expect(clientVersionAtLeast(version, minimum)).toBe(expected);
  });
});
