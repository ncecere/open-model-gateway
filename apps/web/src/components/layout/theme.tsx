import { Monitor, Moon, Sun } from "lucide-react";
import { setColorMode, useColorMode, type ColorMode } from "../ui/color-mode/color-mode";
import { MenuRadioGroup, MenuRadioItem } from "../ui/menu/menu";
import type { CommandGroup } from "../ui/command-palette/command-palette";
import s from "./layout.module.css";
const modes = [{ value: "system", label: "System", icon: Monitor }, { value: "light", label: "Light", icon: Sun }, { value: "dark", label: "Dark", icon: Moon }] as const;
export function ColorModeSync() { useColorMode(); return null; }
export function ThemeMenuGroup() { const { mode, setMode } = useColorMode(); return <MenuRadioGroup label="Theme" value={mode} onValueChange={v => { if (v === "system" || v === "light" || v === "dark") setMode(v); }}>{modes.map(m => <MenuRadioItem key={m.value} value={m.value}><span className={s.themeItem}><m.icon aria-hidden />{m.label}</span></MenuRadioItem>)}</MenuRadioGroup>; }
export function themeCommands(current: ColorMode): CommandGroup["items"] { return modes.map(m => ({ id: `theme:${m.value}`, label: `Theme: ${m.label}`, icon: <m.icon aria-hidden />, hint: m.value === current ? "Current theme" : "Theme", keywords: ["theme", "appearance", m.value], onSelect: () => setColorMode(m.value) })); }
