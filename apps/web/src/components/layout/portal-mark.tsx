/*
 * The Open Model Gateway mark, "Portal": a tunnel seen slightly off-axis, two light bands receding to an
 * indigo core, on a dark tile. Inline SVG (no request): mark-small.svg from omg-assets (logo/portal/), the
 * drawing for 20 to 47 px, the same markup as omg-website and omg-docs. Fixed colours (#161a22, #f7f8fa,
 * #7f86f2): never themed or recoloured. Decorative: the product or installation name is always beside it.
 */
export function PortalMark({ className }: { className?: string }) {
  return <svg viewBox="0 0 64 64" aria-hidden="true" focusable="false" className={className} data-mark="portal">
    <rect width="64" height="64" rx="14" fill="#161a22" />
    <path d="M11.5 32A20.5 20.5 0 1 0 52.5 32A20.5 20.5 0 1 0 11.5 32ZM19.6 30.4A14 14 0 1 0 47.6 30.4A14 14 0 1 0 19.6 30.4Z" fill="#f7f8fa" fillRule="evenodd" />
    <path d="M25.2 28.8A10 10 0 1 0 45.2 28.8A10 10 0 1 0 25.2 28.8ZM30.05 27.7A6.25 6.25 0 1 0 42.55 27.7A6.25 6.25 0 1 0 30.05 27.7Z" fill="#f7f8fa" fillRule="evenodd" />
    <circle cx="37.1" cy="26.9" r="3.25" fill="#7f86f2" />
  </svg>;
}
