import { describe, expect, it } from "vitest";
import { clientVersionIsSupported } from "../src/service";

describe("minimum privacy client version", () => {
  it.each([
    ["0.1.0", false],
    ["0.1.1-alpha", false],
    ["0.1.1", true],
    ["0.1.1+build.7", true],
    ["0.1.2", true],
    ["0.1.2-alpha.1", true],
    ["00.1.1", false],
  ])("classifies %s", (version, expected) => {
    expect(clientVersionIsSupported(version)).toBe(expected);
  });
});
