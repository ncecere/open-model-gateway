//! Native console regression; executed only on a Windows host.
#![cfg(windows)]

#[path = "../examples/support/terminal.rs"]
mod terminal;

use std::process::Command;

#[test]
fn a_windows_console_accepts_prompts_without_captured_secrets() {
    if std::env::var_os("SDK_WINDOWS_CONSOLE_PROBE").is_some() {
        terminal::display(
            "https://example.invalid/authorize?state=STATE_CANARY",
            Some("CODE_CANARY"),
        )
        .expect("the allocated Windows console must accept the prompt");
        return;
    }
    // Allocate a real console even when the test runner has redirected output.
    let script = r#"
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
public static class ExampleConsole {
    [DllImport("kernel32.dll")] public static extern bool AllocConsole();
    [DllImport("kernel32.dll")] public static extern bool FreeConsole();
}
'@
[void][ExampleConsole]::FreeConsole()
if (-not [ExampleConsole]::AllocConsole()) { throw 'Could not allocate the test console' }
try {
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $env:SDK_WINDOWS_PROBE_BINARY
    $start.Arguments = '--exact a_windows_console_accepts_prompts_without_captured_secrets --nocapture'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::Start($start)
    if (-not $process.WaitForExit(10000)) {
        $process.Kill()
        throw 'Console prompt probe timed out'
    }
    $captured = $process.StandardOutput.ReadToEnd() + $process.StandardError.ReadToEnd()
    if ($process.ExitCode -ne 0) { throw 'Console prompt probe failed' }
    if ($captured.Contains('CODE_CANARY') -or $captured.Contains('STATE_CANARY')) {
        throw 'An authorization value reached captured output'
    }
} finally {
    [void][ExampleConsole]::FreeConsole()
}
"#;
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("SDK_WINDOWS_CONSOLE_PROBE", "1")
        .env("SDK_WINDOWS_PROBE_BINARY", std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Windows console contract failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
