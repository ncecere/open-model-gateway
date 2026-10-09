/*
 * The installation's uploaded logo (Admin › Settings › General), served same-origin by the gateway at
 * /api/v1/branding/logo; the Portal mark when none is set or the image fails to load. Never an external URL.
 */
import { useState } from "react";
import type { InstallationLogo as Logo } from "../../lib/api";
import { PortalMark } from "./portal-mark";
import s from "./layout.module.css";

/** Only same-origin logo paths from the gateway are drawn. */
export function logoSrc(logo: Logo | null | undefined): string | undefined {
  return logo && /^\/api\/v1\/branding\/logo(?:\?v=[0-9a-f]{1,32})?$/.test(logo.url) ? logo.url : undefined;
}

/**
 * `alt`: the installation's name, or "" (decorative) where that name is adjacent live text or the mark sits in
 * an aria-hidden wrapper (the sidebar Brand).
 */
export function InstallationLogo({ logo, alt, className }: { logo?: Logo | null; alt: string; className?: string }) {
  const src = logoSrc(logo), [failed, setFailed] = useState<string>();
  if (!src || failed === src) return <PortalMark className={className} />;
  return <img src={src} alt={alt} className={[s.customLogo, className].filter(Boolean).join(" ")} decoding="async" onError={() => setFailed(src)} data-mark="custom" />;
}
