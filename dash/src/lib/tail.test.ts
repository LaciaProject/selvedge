import { describe, expect, it } from "vitest";

import { eventText, isError, type TailEvent } from "./tail";

const event = (over: Partial<TailEvent>): TailEvent => ({
  outcome: "ok",
  eventTimestamp: 0,
  event: { request: { url: "https://w.example/a?b=1", method: "GET" }, response: { status: 200 } },
  logs: [],
  exceptions: [],
  ...over,
});

describe("tail events", () => {
  it("count exceptions, bad outcomes and 5xx as errors", () => {
    expect(isError(event({}))).toBe(false);
    expect(isError(event({ outcome: "exception" }))).toBe(true);
    expect(isError(event({ exceptions: [{ name: "Error", message: "x", timestamp: 0 }] }))).toBe(true);
    expect(isError(event({ event: { request: { url: "/", method: "GET" }, response: { status: 502 } } }))).toBe(true);
    expect(isError(event({ event: { request: { url: "/", method: "GET" }, response: { status: 404 } } }))).toBe(false);
  });

  it("filter on the request, the logs and the exceptions", () => {
    const e = event({
      logs: [{ message: ["user", { id: 7 }], level: "log", timestamp: 0 }],
      exceptions: [{ name: "TypeError", message: "boom", timestamp: 0 }],
    });
    const text = eventText(e);
    for (const needle of ["get", "w.example/a", "user", '{"id":7}', "typeerror", "boom"]) {
      expect(text).toContain(needle);
    }
  });
});
