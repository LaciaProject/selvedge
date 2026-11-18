import { Banner } from "@cloudflare/kumo/components/banner";
import { Button } from "@cloudflare/kumo/components/button";
import { LayerCard } from "@cloudflare/kumo/components/layer-card";
import { SensitiveInput } from "@cloudflare/kumo/components/sensitive-input";
import { Text } from "@cloudflare/kumo/components/text";
import { SignInIcon, WarningCircleIcon } from "@phosphor-icons/react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";
import { useTranslation } from "react-i18next";

import { Logo } from "../components/Logo";
import { ErrorBanner, Loading } from "../components/ui";
import { LANGUAGES, setLanguage } from "../i18n";
import { ApiError } from "../lib/api";
import { authConfigQuery, loginWithToken } from "../lib/session";

/** Login failures the server reports by redirecting here with ?error=. */
const KNOWN_ERRORS = [
  "access_denied",
  "provider_error",
  "provider_unavailable",
  "login_expired",
  "login_failed",
  "server_error",
  "oidc_disabled",
];

/** Only paths on this site. */
function safeReturn(to: string | undefined) {
  return to && to.startsWith("/") && !to.startsWith("//") && !to.startsWith("/login") ? to : "/";
}

export function Login() {
  const { t, i18n } = useTranslation();
  const search = useSearch({ from: "/login" });
  const navigate = useNavigate();
  const config = useQuery(authConfigQuery);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [tokenError, setTokenError] = useState<string>();
  const returnTo = safeReturn(search.return_to);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setTokenError(undefined);
    try {
      await loginWithToken(token.trim());
      navigate({ to: returnTo });
    } catch (err) {
      setTokenError(err instanceof ApiError && err.status === 401 ? t("login.badToken") : t("errors.failed"));
    } finally {
      setBusy(false);
    }
  };

  const error = search.error && (KNOWN_ERRORS.includes(search.error) ? t(`login.errors.${search.error}`) : t("login.errors.login_failed"));

  return (
    <div className="flex min-h-full items-center justify-center p-4">
      <div className="flex w-full max-w-sm flex-col gap-6">
        <div className="flex flex-col items-center gap-3">
          <Logo className="size-12" />
          <Text variant="heading" size="lg" as="h1">
            {t("login.title")}
          </Text>
        </div>
        <LayerCard>
          <LayerCard.Primary className="flex flex-col gap-4 p-6">
            {error && <Banner variant="error" icon={<WarningCircleIcon weight="fill" />} description={error} />}
            {config.isPending && <Loading />}
            {config.isError && <ErrorBanner error={config.error} onRetry={() => config.refetch()} />}
            {config.data?.oidc && (
              <Button
                variant="primary"
                size="lg"
                icon={SignInIcon}
                className="w-full justify-center"
                onClick={() => window.location.assign(`/auth/login?return_to=${encodeURIComponent(returnTo)}`)}
              >
                {t("login.withProvider", { name: config.data.oidc.name })}
              </Button>
            )}
            {config.data?.oidc && config.data.token_login && (
              <div className="flex items-center gap-3 text-sm text-kumo-subtle">
                <div className="h-px flex-1 bg-kumo-line" />
                {t("login.or")}
                <div className="h-px flex-1 bg-kumo-line" />
              </div>
            )}
            {config.data?.token_login && (
              <form onSubmit={submit} className="flex flex-col gap-3">
                <SensitiveInput
                  label={t("login.token")}
                  value={token}
                  onValueChange={setToken}
                  autoComplete="off"
                  error={tokenError}
                />
                <Button type="submit" variant={config.data.oidc ? "secondary" : "primary"} loading={busy} disabled={!token.trim()}>
                  {t("login.withToken")}
                </Button>
              </form>
            )}
          </LayerCard.Primary>
        </LayerCard>
        <div className="flex items-center justify-between text-sm text-kumo-subtle">
          <span>{t("app.independent")}</span>
          <span className="flex gap-2 whitespace-nowrap">
            {LANGUAGES.map((l) => (
              <button
                key={l}
                type="button"
                className={l === i18n.language ? "font-medium text-kumo-default" : "hover:text-kumo-default"}
                onClick={() => setLanguage(l)}
              >
                {t(`languages.${l}`)}
              </button>
            ))}
          </span>
        </div>
      </div>
    </div>
  );
}
