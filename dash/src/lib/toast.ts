import { createKumoToastManager } from "@cloudflare/kumo/components/toast";
import type { TFunction } from "i18next";

import { describeError } from "./errors";

/** One toast manager for the app, usable outside components too. */
export const toasts = createKumoToastManager();

export function notifyDone(title: string, description?: string) {
  toasts.add({ title, description, variant: "success" });
}

export function notifyFailed(e: unknown, t: TFunction) {
  const { title, detail } = describeError(e, t);
  toasts.add({ title, description: detail, variant: "error" });
}
