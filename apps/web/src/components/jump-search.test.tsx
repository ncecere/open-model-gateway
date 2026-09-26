import { afterEach, describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import type { Session } from "../lib/api";
import type { CommandPaletteProps } from "./ui/command-palette/command-palette";
import { JumpSearch } from "./jump-search";

const captured = vi.hoisted(() => ({ palette: undefined as CommandPaletteProps | undefined }));
vi.mock("./ui/command-palette/command-palette", () => ({
  CommandPalette: (props: CommandPaletteProps) => { captured.palette = props; return null; },
  CommandPaletteTrigger: () => <button>Search</button>,
  useCommandPaletteShortcut: () => {},
}));
const session: Session = { user: { id: "operator", email: "operator@example.invalid", platform_admin: true }, organizations: [], workspaces: [] };
afterEach(() => { captured.palette = undefined; vi.unstubAllGlobals(); });
function setup() {
  const navigate = vi.fn(), refresh = vi.fn();
  renderToStaticMarkup(<JumpSearch session={session} navigate={navigate} refresh={refresh} />);
  const palette = captured.palette!;
  const finalFocus = palette.finalFocus as (interaction: "keyboard") => unknown;
  return { palette, finalFocus, navigate, refresh };
}
describe("jump search focus after command selection", () => {
  it("uses default focus restoration on dismissal and non-navigation actions", () => {
    const { palette, finalFocus, refresh, navigate } = setup();
    expect(finalFocus("keyboard")).toBe(true);
    palette.groups.flatMap(group => group.items).find(item => item.id === "refresh")!.onSelect();
    expect(refresh).toHaveBeenCalledOnce(); expect(navigate).not.toHaveBeenCalled();
    expect(finalFocus("keyboard")).toBe(true);
  });
  it("uses the destination heading after navigation without adding a sequential tab stop", () => {
    const heading = { tabIndex: 0 };
    const querySelector = vi.fn().mockReturnValue(heading);
    vi.stubGlobal("document", { querySelector });
    const { palette, finalFocus, navigate } = setup();
    palette.groups.flatMap(group => group.items).find(item => item.id === "page:models")!.onSelect();
    expect(navigate).toHaveBeenCalledWith({ page: "models", org: undefined, ws: undefined });
    expect(finalFocus("keyboard")).toBe(heading);
    expect(querySelector).toHaveBeenCalledWith("#main h1"); expect(heading.tabIndex).toBe(-1);
  });
  it("falls back safely if navigation has not produced a heading", () => {
    vi.stubGlobal("document", { querySelector: () => null });
    const { palette, finalFocus } = setup();
    palette.groups.flatMap(group => group.items).find(item => item.id === "page:models")!.onSelect();
    expect(finalFocus("keyboard")).toBe(true);
  });
});
