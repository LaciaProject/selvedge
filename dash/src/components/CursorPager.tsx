import { Button } from "@cloudflare/kumo/components/button";
import { CaretLeftIcon, CaretRightIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

/** Paging through a cursor-based listing: the cursors seen so far. */
export function useCursors() {
  const [stack, setStack] = useState<string[]>([]);
  return {
    cursor: stack[stack.length - 1],
    page: stack.length + 1,
    next: (c: string) => setStack((s) => [...s, c]),
    back: () => setStack((s) => s.slice(0, -1)),
    reset: () => setStack([]),
  };
}

export function CursorPager(props: { page: number; hasNext: boolean; onNext: () => void; onBack: () => void }) {
  const { t } = useTranslation();
  if (props.page === 1 && !props.hasNext) return null;
  return (
    <div className="flex items-center justify-end gap-2 border-t border-kumo-hairline p-2">
      <span className="text-sm text-kumo-subtle">{t("common.page", { page: props.page })}</span>
      <Button size="sm" variant="ghost" icon={CaretLeftIcon} disabled={props.page === 1} onClick={props.onBack}>
        {t("common.previous")}
      </Button>
      <Button size="sm" variant="ghost" disabled={!props.hasNext} onClick={props.onNext}>
        {t("common.next")}
        <CaretRightIcon size={14} />
      </Button>
    </div>
  );
}
