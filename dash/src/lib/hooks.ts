import { type QueryKey, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import { useTranslation } from "react-i18next";

import { accountPath, api, type RequestOptions } from "./api";
import { notifyDone, notifyFailed } from "./toast";

/** The account in the URL. */
export function useAccount(): string {
  const p = useParams({ strict: false }) as { account?: string };
  if (!p.account) throw new Error("not under /accounts/$account");
  return p.account;
}

/** A GET under the current account, cached per account. */
export function useAccountQuery<T>(key: QueryKey, path: string, opts?: { enabled?: boolean; refetchInterval?: number }) {
  const account = useAccount();
  return useQuery({
    queryKey: [account, ...key],
    queryFn: () => api<T>(accountPath(account, path)),
    enabled: opts?.enabled,
    refetchInterval: opts?.refetchInterval,
  });
}

/** Calls under the current account. */
export function useAccountApi() {
  const account = useAccount();
  return {
    account,
    call: <T = unknown>(path: string, opts?: RequestOptions) => api<T>(accountPath(account, path), opts),
  };
}

/**
 * A change: runs it, tells the reader how it went, and refreshes what it
 * touched. For dialogs, which show their own errors, use `mutateAsync`.
 */
export function useChange<V = void, R = unknown>(
  run: (v: V) => Promise<R>,
  opts: { done?: string | ((r: R, v: V) => string); invalidate?: QueryKey[]; silentError?: boolean },
) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const account = useParams({ strict: false }) as { account?: string };
  return useMutation({
    mutationFn: run,
    onSuccess: async (r, v) => {
      for (const k of opts.invalidate ?? []) {
        await qc.invalidateQueries({ queryKey: account.account ? [account.account, ...k] : k });
      }
      const done = typeof opts.done === "function" ? opts.done(r, v) : opts.done;
      if (done) notifyDone(done);
    },
    onError: (e) => {
      if (!opts.silentError) notifyFailed(e, t);
    },
  });
}
