/** Selvedge's mark: woven rows and the selvedge's dashed edge. */
export function Logo({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" className={className} aria-hidden="true">
      <rect width="32" height="32" rx="7" className="fill-kumo-brand" />
      <path d="M7 9h18M7 16h18M7 23h18" stroke="white" strokeOpacity="0.8" strokeWidth="2.5" strokeLinecap="round" />
      <path d="M25 4v24" stroke="#d6453d" strokeWidth="3" strokeDasharray="2 2.5" />
    </svg>
  );
}
