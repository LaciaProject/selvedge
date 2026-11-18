import type { TFunction } from "i18next";
import { describe, expect, it } from "vitest";

import { ApiError } from "./api";
import { describeError } from "./errors";

const t = ((k: string) => `<${k}>`) as unknown as TFunction;

describe("describeError", () => {
  it("translates the API's codes and keeps its message as the detail", () => {
    expect(describeError(new ApiError(404, 10007, "Account not found"), t)).toEqual({
      title: "<errors.notFound>",
      detail: "Account not found",
    });
    expect(describeError(new ApiError(403, 10000, "Authentication error"), t).title).toBe("<errors.auth>");
  });

  it("falls back on the status", () => {
    expect(describeError(new ApiError(409, 4242, "taken"), t).title).toBe("<errors.conflict>");
    expect(describeError(new ApiError(503, 0, ""), t)).toEqual({ title: "<errors.internal>", detail: undefined });
    expect(describeError(new ApiError(400, 0, "nope"), t).title).toBe("<errors.failed>");
  });

  it("names network failures", () => {
    expect(describeError(new TypeError("Failed to fetch"), t).title).toBe("<errors.network>");
  });
});
