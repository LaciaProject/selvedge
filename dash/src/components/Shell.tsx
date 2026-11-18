import { Button } from "@cloudflare/kumo/components/button";
import { DropdownMenu } from "@cloudflare/kumo/components/dropdown";
import { Select } from "@cloudflare/kumo/components/select";
import { Sidebar } from "@cloudflare/kumo/components/sidebar";
import {
  CodeIcon,
  CubeIcon,
  DatabaseIcon,
  FlowArrowIcon,
  GearIcon,
  HouseIcon,
  KeyIcon,
  PackageIcon,
  QueueIcon,
  ShapesIcon,
  ShieldCheckIcon,
  SignOutIcon,
  TranslateIcon,
  UserCircleIcon,
} from "@phosphor-icons/react";
import { useQuery } from "@tanstack/react-query";
import { Outlet, useNavigate, useParams, useRouterState } from "@tanstack/react-router";
import { useTranslation } from "react-i18next";

import { type Language, LANGUAGES, setLanguage } from "../i18n";
import { logout, meQuery } from "../lib/session";
import { Logo } from "./Logo";

const SECTIONS = [
  { path: "", icon: HouseIcon, label: "nav.overview" },
  { path: "/workers", icon: CodeIcon, label: "nav.workers" },
  { path: "/kv", icon: KeyIcon, label: "nav.kv" },
  { path: "/d1", icon: DatabaseIcon, label: "nav.d1" },
  { path: "/r2", icon: PackageIcon, label: "nav.r2" },
  { path: "/queues", icon: QueueIcon, label: "nav.queues" },
  { path: "/workflows", icon: FlowArrowIcon, label: "nav.workflows" },
  { path: "/durable-objects", icon: ShapesIcon, label: "nav.durableObjects" },
  { path: "/containers", icon: CubeIcon, label: "nav.containers" },
  { path: "/settings", icon: GearIcon, label: "nav.settings" },
] as const;

export function Shell() {
  const { t } = useTranslation();
  const me = useQuery(meQuery).data;
  const params = useParams({ strict: false }) as { account?: string };
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const navigate = useNavigate();
  const account = params.account;
  const base = account ? `/accounts/${account}` : undefined;
  const active = (path: string) =>
    base !== undefined && (path === "" ? pathname === base || pathname === `${base}/` : pathname.startsWith(base + path));

  return (
    <Sidebar.Provider defaultOpen className="h-svh">
      <Sidebar>
        <Sidebar.Header>
          <a href="/" className="flex items-center gap-2 px-2 py-1 font-semibold">
            <Logo className="size-6 shrink-0" />
            <span>{t("app.name")}</span>
          </a>
        </Sidebar.Header>
        <Sidebar.Content>
          {me && me.accounts.length > 0 && (
            <div className="px-2 pb-2">
              <Select
                aria-label={t("nav.account")}
                value={account ?? null}
                placeholder={t("nav.chooseAccount")}
                onValueChange={(v) => v && navigate({ to: "/accounts/$account", params: { account: String(v) } })}
                className="w-full"
                items={Object.fromEntries(me.accounts.map((a) => [a.id, a.name]))}
              >
                {me.accounts.map((a) => (
                  <Select.Option key={a.id} value={a.id}>
                    {a.name}
                  </Select.Option>
                ))}
              </Select>
            </div>
          )}
          {base && (
            <Sidebar.Group>
              <Sidebar.Menu>
                {SECTIONS.map((s) => (
                  <Sidebar.MenuButton key={s.path} icon={s.icon} href={base + s.path} active={active(s.path)}>
                    {t(s.label)}
                  </Sidebar.MenuButton>
                ))}
              </Sidebar.Menu>
            </Sidebar.Group>
          )}
          {me?.admin && (
            <Sidebar.Group>
              <Sidebar.GroupLabel>{t("nav.platform")}</Sidebar.GroupLabel>
              <Sidebar.Menu>
                <Sidebar.MenuButton icon={ShieldCheckIcon} href="/admin" active={pathname.startsWith("/admin")}>
                  {t("nav.admin")}
                </Sidebar.MenuButton>
              </Sidebar.Menu>
            </Sidebar.Group>
          )}
        </Sidebar.Content>
        <Sidebar.Footer>
          <p className="px-2 text-xs whitespace-normal text-kumo-subtle">{t("app.independent")}</p>
        </Sidebar.Footer>
      </Sidebar>
      <div className="flex h-svh min-w-0 flex-1 flex-col overflow-y-auto">
        <header className="sticky top-0 z-10 flex items-center justify-between gap-2 border-b border-kumo-hairline bg-kumo-base px-4 py-2">
          <Sidebar.Trigger aria-label={t("nav.toggle")} />
          <div className="flex items-center gap-1">
            <LanguageMenu />
            <UserMenu />
          </div>
        </header>
        <main className="min-w-0 flex-1">
          <Outlet />
        </main>
      </div>
    </Sidebar.Provider>
  );
}

function LanguageMenu() {
  const { t, i18n } = useTranslation();
  return (
    <DropdownMenu>
      <DropdownMenu.Trigger
        render={
          <Button variant="ghost" size="sm" icon={TranslateIcon} aria-label={t("nav.language")}>
            {t(`languages.${i18n.language}`)}
          </Button>
        }
      />
      <DropdownMenu.Content>
        {LANGUAGES.map((l: Language) => (
          <DropdownMenu.Item key={l} selected={i18n.language === l} onClick={() => setLanguage(l)}>
            {t(`languages.${l}`)}
          </DropdownMenu.Item>
        ))}
      </DropdownMenu.Content>
    </DropdownMenu>
  );
}

function UserMenu() {
  const { t } = useTranslation();
  const me = useQuery(meQuery).data;
  const who = me?.user ? (me.user.name ?? me.user.email ?? me.user.id) : me?.token ? t("nav.tokenSession", { name: me.token.name }) : "";
  return (
    <DropdownMenu>
      <DropdownMenu.Trigger
        render={
          <Button variant="ghost" size="sm" icon={UserCircleIcon}>
            <span className="max-w-48 truncate">{who}</span>
          </Button>
        }
      />
      <DropdownMenu.Content>
        {me?.user?.email && (
          <div className="px-3 py-2 text-sm text-kumo-subtle">{me.user.email}</div>
        )}
        {me?.admin && <div className="px-3 pb-2 text-sm text-kumo-subtle">{t("nav.platformAdmin")}</div>}
        <DropdownMenu.Item icon={SignOutIcon} onClick={() => logout()}>
          {t("nav.logout")}
        </DropdownMenu.Item>
      </DropdownMenu.Content>
    </DropdownMenu>
  );
}
