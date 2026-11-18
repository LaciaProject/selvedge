import { useTranslation } from "react-i18next";

// Numbers, sizes and times in the interface's language (RFC 0003 §8). Times
// show in the browser's time zone; their title shows UTC.

const UNITS = ["B", "KB", "MB", "GB", "TB"];

export function formatBytes(n: number, locale: string): string {
  let v = n;
  let u = 0;
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  const digits = u === 0 || v >= 100 ? 0 : 1;
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: digits }).format(v)} ${UNITS[u]}`;
}

const STEPS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["second", 60],
  ["minute", 60],
  ["hour", 24],
  ["day", 30],
  ["month", 12],
  ["year", Infinity],
];

export function formatRelative(date: Date, now: Date, locale: string): string {
  let delta = (date.getTime() - now.getTime()) / 1000;
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  for (const [unit, size] of STEPS) {
    if (Math.abs(delta) < size) return rtf.format(Math.round(delta), unit);
    delta /= size;
  }
  return rtf.format(Math.round(delta), "year");
}

export function useFormat() {
  const { i18n } = useTranslation();
  const locale = i18n.language;
  return {
    locale,
    number: (n: number) => new Intl.NumberFormat(locale).format(n),
    bytes: (n: number) => formatBytes(n, locale),
    date: (s: string | number | Date) =>
      new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short" }).format(new Date(s)),
    time: (s: string | number | Date) =>
      new Intl.DateTimeFormat(locale, { timeStyle: "medium" }).format(new Date(s)),
    relative: (s: string | number | Date) => formatRelative(new Date(s), new Date(), locale),
    utc: (s: string | number | Date) => new Date(s).toISOString(),
  };
}
