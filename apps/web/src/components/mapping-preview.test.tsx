// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { useRef, useState } from "react";
import { MappingReviewDialog, type MappingReviewRequest } from "./mapping-preview";
import { ActionProvider, Button } from "./ui";
import { testClient } from "../lib/test-fixtures";

const noImmediate = { applies: false, blocked_by_last_admin: false, platform_role_revoked: [], workspace_membership_revoked: [], users_suspended: [], keys_revoked_count: 0 };
// Realistic digests (`mapping_digest`, directory.rs): 64 lowercase hex characters.
const DIGEST_A = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".slice(0, 64);
const DIGEST_B = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".slice(0, 64);
const basePreview = {
  membership_basis: "scim" as const,
  complete: true,
  digest: DIGEST_A,
  immediate: noImmediate,
  diff: {
    platform_role_gained: [{ user_id: "u1", email: "a@example.test", display_name: null, before: null, after: "auditor", workspace_id: null }],
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
  const originRef = useRef<HTMLElement | null>(null);
  return (
    <>
      <Button
        onClick={event => {
          originRef.current = event.currentTarget;
          setOrigin(event.currentTarget);
        }}
      >
        Open review
      </Button>
      {/* onStale (#89 review item 6): a 412 reopens a fresh review on the same origin, remounting (not just re-rendering) so it refetches. */}
      {origin && <MappingReviewDialog request={{ ...request, returnFocus: origin, onStale: () => setOrigin(originRef.current) }} onClose={() => setOrigin(null)} />}
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

  it("shows what the real write revokes immediately, separately from the net diff", async () => {
    const immediate = { ...noImmediate, applies: true, platform_role_revoked: [{ user_id: "u2", email: "b@example.test", display_name: null, role: "admin" }], keys_revoked_count: 2 };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ ...basePreview, immediate })));
    const user = view(createRequest);
    await user.click(screen.getByRole("button", { name: /^Open review$/ }));
    await screen.findByText(/Applied immediately, not at next sign-in/);
    expect(screen.getByText(/b@example\.test/)).toBeTruthy();
    expect(screen.getByText(/2 API keys would be permanently revoked/)).toBeTruthy();
  });

  it("sends If-Match with the reviewed digest, and reopens a fresh review on a stale 412", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(Response.json(basePreview))
      .mockResolvedValueOnce(Response.json({ error: { code: "412", message: "The mapping was changed since this proposal was reviewed", reason: "precondition_failed" } }, { status: 412 }))
      .mockResolvedValueOnce(Response.json({ ...basePreview, digest: DIGEST_B }));
    vi.stubGlobal("fetch", fetch);
    const user = view(createRequest);
    await user.click(screen.getByRole("button", { name: /^Open review$/ }));
    await screen.findByText(/^Platform role gained \(1\)$/);
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await screen.findByRole("dialog", { name: /^Create this SSO group mapping\?$/ });
    await user.click(screen.getByRole("button", { name: /^Create mapping$/ }));
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(2));
    // The server requires a quoted ETag-shaped value (directory.rs's
    // `check_if_match`); an unquoted digest is refused as malformed.
    expect((fetch.mock.calls[1][1] as RequestInit).headers).toMatchObject({ "If-Match": `"${DIGEST_A}"` });
    await screen.findByText(/was changed since this proposal was reviewed/);
    // onStale reopened a fresh review behind the error dialog.
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(3));
    expect(fetch.mock.calls[2][1]).toMatchObject({ method: "POST" });
  });

  it("a stale 412 triggers a fresh re-preview, not just a retried apply", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(Response.json(basePreview))
      .mockResolvedValueOnce(Response.json({ error: { code: "412", message: "The mapping was changed since this proposal was reviewed", reason: "precondition_failed" } }, { status: 412 }))
      .mockResolvedValueOnce(Response.json({ ...basePreview, digest: DIGEST_B }));
    vi.stubGlobal("fetch", fetch);
    const user = view(createRequest);
    await user.click(screen.getByRole("button", { name: /^Open review$/ }));
    await screen.findByText(/^Platform role gained \(1\)$/);
    await user.click(screen.getByRole("button", { name: /^Continue to confirmation$/ }));
    await screen.findByRole("dialog", { name: /^Create this SSO group mapping\?$/ });
    await user.click(screen.getByRole("button", { name: /^Create mapping$/ }));
    await screen.findByText(/was changed since this proposal was reviewed/);
    // The reopened review is a genuine re-preview -- a third POST to the
    // preview path, not a retried apply -- and it renders the freshly
    // fetched content, not the stale one.
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(3));
    expect(fetch.mock.calls[2][0]).toBe(createRequest.previewPath);
    expect(fetch.mock.calls[2][1]).toMatchObject({ method: "POST" });
    await screen.findByText(/^Platform role gained \(1\)$/);
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
