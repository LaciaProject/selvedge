import { Breadcrumbs } from "@cloudflare/kumo/components/breadcrumbs";
import { Fragment } from "react";

/** Where a page sits: links up to the current page's name. */
export function Crumbs({ items, current }: { items: { label: string; href: string }[]; current: string }) {
  return (
    <Breadcrumbs size="sm">
      {items.map((i) => (
        <Fragment key={i.href}>
          <Breadcrumbs.Link href={i.href}>{i.label}</Breadcrumbs.Link>
          <Breadcrumbs.Separator />
        </Fragment>
      ))}
      <Breadcrumbs.Current>{current}</Breadcrumbs.Current>
    </Breadcrumbs>
  );
}
