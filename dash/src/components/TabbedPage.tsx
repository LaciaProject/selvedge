import { Tabs } from "@cloudflare/kumo/components/tabs";
import { useNavigate } from "@tanstack/react-router";
import type { ReactNode } from "react";

/** Tabs kept in the URL's `?tab=`, so a tab can be linked and reloaded. */
export function UrlTabs(props: {
  tabs: { value: string; label: string; render: () => ReactNode }[];
  current: string | undefined;
}) {
  const navigate = useNavigate();
  const value = props.tabs.some((t) => t.value === props.current) ? props.current! : props.tabs[0].value;
  const active = props.tabs.find((t) => t.value === value)!;
  return (
    <div className="flex flex-col gap-6">
      <Tabs
        variant="underline"
        value={value}
        tabs={props.tabs.map((t) => ({ value: t.value, label: t.label }))}
        onValueChange={(v) =>
          navigate({ to: ".", search: (s: Record<string, unknown>) => ({ ...s, tab: v }), replace: true })
        }
      />
      {active.render()}
    </div>
  );
}
