import { Banner } from "@cloudflare/kumo/components/banner";
import { Button } from "@cloudflare/kumo/components/button";
import { Dialog } from "@cloudflare/kumo/components/dialog";
import { Input } from "@cloudflare/kumo/components/input";
import { CheckIcon, CopyIcon, WarningCircleIcon, XIcon } from "@phosphor-icons/react";
import { type FormEvent, type ReactNode, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { errorText } from "../lib/errors";

function DialogHeader({ title, busy }: { title: ReactNode; busy?: boolean }) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center justify-between border-b border-kumo-line px-6 py-4">
      <Dialog.Title className="text-lg font-semibold">{title}</Dialog.Title>
      <Dialog.Close
        render={(p) => (
          <Button {...p} variant="ghost" shape="square" size="sm" aria-label={t("common.close")} disabled={busy}>
            <XIcon size={18} />
          </Button>
        )}
      />
    </div>
  );
}

/**
 * A form in a dialog. `onSubmit` may throw; its error shows in the dialog,
 * which stays open. On success it closes.
 */
export function FormDialog(props: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  submitLabel: string;
  onSubmit: () => Promise<void>;
  children?: ReactNode;
  size?: "sm" | "base" | "lg" | "xl";
  destructive?: boolean;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const openChange = (o: boolean) => {
    if (busy) return;
    if (!o) setError(undefined);
    props.onOpenChange(o);
  };
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      await props.onSubmit();
      setError(undefined);
      props.onOpenChange(false);
    } catch (err) {
      setError(errorText(err, t));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog.Root open={props.open} onOpenChange={openChange}>
      <Dialog size={props.size ?? "lg"} className="p-0">
        <form onSubmit={submit}>
          <DialogHeader title={props.title} busy={busy} />
          <div className="flex flex-col gap-4 p-6">
            {error && <Banner variant="error" icon={<WarningCircleIcon weight="fill" />} description={error} />}
            {props.description && <p className="text-kumo-subtle">{props.description}</p>}
            {props.children}
          </div>
          <div className="flex justify-end gap-3 border-t border-kumo-line px-6 py-4">
            <Dialog.Close
              render={(p) => (
                <Button {...p} variant="secondary" disabled={busy}>
                  {t("common.cancel")}
                </Button>
              )}
            />
            <Button
              type="submit"
              variant={props.destructive ? "destructive" : "primary"}
              loading={busy}
              disabled={props.disabled || busy}
            >
              {props.submitLabel}
            </Button>
          </div>
        </form>
      </Dialog>
    </Dialog.Root>
  );
}

/**
 * Deleting (or emptying) something that cannot come back: the reader types
 * its name to confirm. Kumo's DeleteResource block, translated.
 */
export function ConfirmDelete(props: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The kind of thing, as the sentence needs it ("Worker", "存储桶"). */
  kind: string;
  name: string;
  onConfirm: () => Promise<void>;
  /** Replaces "This will permanently delete …". */
  consequence?: ReactNode;
  confirmLabel?: string;
}) {
  const { t } = useTranslation();
  const [typed, setTyped] = useState("");
  const [copied, setCopied] = useState(false);
  const openChange = (o: boolean) => {
    if (!o) {
      setTyped("");
      setCopied(false);
    }
    props.onOpenChange(o);
  };
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(props.name);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard refused (no permission, insecure context): nothing to show.
    }
  };
  return (
    <FormDialog
      open={props.open}
      onOpenChange={openChange}
      title={t("confirm.title", { name: props.name })}
      submitLabel={props.confirmLabel ?? t("confirm.delete", { kind: props.kind })}
      destructive
      disabled={typed !== props.name}
      onSubmit={props.onConfirm}
    >
      <p className="max-w-prose text-kumo-subtle">
        {props.consequence ?? (
          <Trans
            i18nKey="confirm.consequence"
            values={{ name: props.name, kind: props.kind }}
            components={{ strong: <span className="font-medium text-kumo-default" /> }}
          />
        )}
      </p>
      <div className="flex flex-col gap-2">
        <span>
          <Trans
            i18nKey="confirm.type"
            values={{ name: props.name }}
            components={{
              name: (
                <button
                  type="button"
                  className="group inline rounded-md bg-kumo-tint px-2 py-1 font-mono text-sm font-semibold hover:cursor-pointer"
                  onClick={copy}
                  aria-label={t("confirm.copy", { name: props.name })}
                />
              ),
            }}
          />
          {copied ? (
            <CheckIcon size={12} weight="bold" className="ml-1 inline" />
          ) : (
            <CopyIcon size={12} weight="bold" className="ml-1 inline text-kumo-subtle" />
          )}
        </span>
        <Input
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          placeholder={props.name}
          autoComplete="off"
          spellCheck={false}
          aria-label={t("confirm.typeLabel", { name: props.name })}
        />
      </div>
    </FormDialog>
  );
}
