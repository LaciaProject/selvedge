import { describe, expect, it } from "vitest";

import { formatBytes, formatRelative } from "./format";

describe("formatBytes", () => {
  it("picks a unit", () => {
    expect(formatBytes(0, "en")).toBe("0 B");
    expect(formatBytes(1536, "en")).toBe("1.5 KB");
    expect(formatBytes(5 * 1024 ** 3, "en")).toBe("5 GB");
    expect(formatBytes(150 * 1024 ** 2, "zh-CN")).toBe("150 MB");
  });
});

describe("formatRelative", () => {
  const now = new Date("2026-11-18T12:00:00Z");
  it("speaks the language", () => {
    expect(formatRelative(new Date("2026-11-18T11:58:00Z"), now, "en")).toBe("2 minutes ago");
    expect(formatRelative(new Date("2026-11-17T12:00:00Z"), now, "en")).toBe("yesterday");
    expect(formatRelative(new Date("2026-11-18T09:00:00Z"), now, "zh-CN")).toBe("3小时前");
  });
});
