// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { useState } from "react";
import { MappingReviewDialog, type MappingReviewRequest } from "./mapping-preview";
import { ActionProvider, Button } from "./ui";
import { testClient } from "../lib/test-fixtures";

const basePreview = {
  membership_basis: "scim" as const,
  diff: {
    platform_role_gained: [{ user_id: "u1", email: "a@example.test", display_name: null, role: "auditor", workspace_id: null }],
    platform_role_lost: [],
    workspace_membership_gained: [],
    workspace_membership_lost: [],
  },
  counts: { gained: 1, lost: 0, truncated: false },
  warnings: ["Configuration estimate, not a reservation. Apply independently revalidates live mapping state, membership and authority."],
};

type ReviewRequest = Omit<MappingReviewRequest, "returnFocus" | "fallbackFocus">;
/** Mirrors the real call site (pages/hierarchy.tsx's OidcMappings): the
 * button that opens the review is the preserved focus origin for the whole
 * review → confirm chain. */
function Harness({ request }: { request: ReviewRequest }) {
  const [origin, setOrigin] = useState<HTMLElement | null>(null);
  return (
    <>
      <Button onClick={event => setOrigin(event.currentTarget)}>Open review</Button>
      {origin && <MappingReviewDialog request={{ ...request, returnFocus: origin }} onClose={() => setOrigin(null)} />}
    </>
  );
}

function view(request: ReviewRequest) {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  const client = testClient();
  render(
    <QueryClientProvider client={client}>
      <ActionProvider>
        <Harness request={request} />
      </ActionProvider>
    </QueryClientProvider>,
  );
  return userEvent.setup();
}

beforeEach(() => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }),
  });
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const createRequest: ReviewRequest = {
  previewPath: "/api/v1/platform/oidc/group-mappings/preview",
  body: { issuer: "https://idp.example.test", group_value: "eng", target_kind: "platform", platform_role: "auditor", workspace_id: null, workspace_role: null, enabled: true },
  title: "Create SSO group mapping",
  confirmTitle: "Create this SSO group mapping?",
  confirmLabel: "Create mapping",
  applyPath: "/api/v1/platform/oidc/group-mappings",
  applyMethod: "POST",
};

describe("group-mapping review", () => {
  it("previews without applying, shows the bounded diff, then uses the existing save path", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(Response.json(basePreview)).mockResolvedValueOnce(Response.json({ id: "m1" }));
    vi.stubGlobal("fetch", fetch);
    const user = view(createRequest);
    expect(fetch).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: /^Open review$/ }));
    await screen.findByText(/^Platform role gained \(1\)$/);
    expect(screen.getByText(/a@example\.test/)).toBeTruthy();
    expect(screen.getByText(/Configuration estimate, not a reservation/)).toBeTruthy();
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe(createRequest.previewPath);
    expect(fetch.mock.calls[0][1]).toMatchObject({ method: "POST", body: JSON.stringify(createRequest.body) });
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await screen.findByRole("dialog", { name: /^Create this SSO group mapping\?$/ });
    expect(fetch).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole("button", { name: /^Create mapping$/ }));
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(2));
    expect(fetch.mock.calls[1][0]).toBe(createRequest.applyPath);
    expect(fetch.mock.calls[1][1]).toMatchObject({ method: "POST", body: JSON.stringify(createRequest.body) });
  });

  it("cancelling the review never applies, and restores focus to the originating trigger", async () => {
    const fetch = vi.fn().mockResolvedValue(Response.json(basePreview));
    vi.stubGlobal("fetch", fetch);
    const user = view(createRequest);
    const origin = screen.getByRole("button", { name: /^Open review$/ });
    await user.click(origin);
    await screen.findByText(/^Platform role gained \(1\)$/);
    await user.click(screen.getByRole("button", { name: /^Cancel$/ }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(origin));
    expect(origin.isConnected).toBe(true);
    expect(fetch.mock.calls.every(([, init]) => (init as RequestInit).method === "POST" && (init as RequestInit).body === JSON.stringify(createRequest.body))).toBe(true);
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("restores focus to the originating trigger after a full review → confirm → save round trip", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(Response.json(basePreview)).mockResolvedValueOnce(Response.json({ id: "m1" }));
    vi.stubGlobal("fetch", fetch);
    const user = view(createRequest);
    const origin = screen.getByRole("button", { name: /^Open review$/ });
    await user.click(origin);
    await screen.findByText(/^Platform role gained \(1\)$/);
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await screen.findByRole("dialog", { name: /^Create this SSO group mapping\?$/ });
    await user.click(screen.getByRole("button", { name: /^Create mapping$/ }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(origin));
    expect(origin.isConnected).toBe(true);
  });

  it("disables confirmation while the preview is refused and shows the server's error", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ error: { code: "403", message: "Access denied" } }, { status: 403 })));
    const user = view(createRequest);
    await user.click(screen.getByRole("button", { name: /^Open review$/ }));
    await screen.findByText(/Access denied/);
    expect(screen.getByRole("button", { name: /^Continue to confirmation$/ }).hasAttribute("disabled")).toBe(true);
  });

  it("shows the sign-in-derived warning when the server reports that basis", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        Response.json({ ...basePreview, membership_basis: "last_sign_in_derived", warnings: [...basePreview.warnings, "No SCIM roster exists for this group value; membership reflects only users who have already signed in with it."] }),
      ),
    );
    const user = view(createRequest);
    await user.click(screen.getByRole("button", { name: /^Open review$/ }));
    await screen.findByText(/Sign-in derived/);
    expect(screen.getByText(/No SCIM roster exists/)).toBeTruthy();
  });
});
