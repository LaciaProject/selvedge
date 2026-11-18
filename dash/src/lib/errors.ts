import type { TFunction } from "i18next";

import { ApiError } from "./api";

// The API answers in English with Cloudflare's numeric codes. Common ones get
// a translated sentence; the server's own message follows as the detail
// (RFC 0003 §8).
const BY_CODE: Record<number, string> = {
  10000: "errors.auth",
  10007: "errors.notFound",
  10013: "errors.internal",
  10021: "errors.invalid",
  1001: "errors.conflict",
  1061: "errors.conflict",
  10405: "errors.invalid",
};

export function describeError(e: unknown, t: TFunction): { title: string; detail?: string } {
  if (e instanceof ApiError) {
    const key =
      BY_CODE[e.code] ??
      (e.status === 404 ? "errors.notFound" : e.status === 409 ? "errors.conflict" : e.status >= 500 ? "errors.internal" : "errors.failed");
    const title = t(key);
    return { title, detail: e.message && e.message !== title ? e.message : undefined };
  }
  if (e instanceof TypeError) return { title: t("errors.network") };
  return { title: t("errors.failed"), detail: e instanceof Error ? e.message : String(e) };
}

export function errorText(e: unknown, t: TFunction): string {
  const { title, detail } = describeError(e, t);
  return detail ? `${title} ${detail}` : title;
}
