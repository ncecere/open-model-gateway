//! Actual file and controlling-terminal boundaries for the interactive examples.
#![cfg(unix)]

#[path = "../examples/support/callback_state.rs"]
mod callback_state;
#[path = "../examples/support/terminal.rs"]
mod terminal;

use oauth2::{CsrfToken, UserCode};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;

#[test]
fn callback_state_requires_the_complete_matching_value() {
    let expected = CsrfToken::new("expected-state-canary".into());
    assert!(callback_state::matches(&expected, &expected));
    for other in [
        "",
        "expected-state-canarz",
        "expected-state-canary-extra",
        "unexpected-state-canary",
    ] {
        assert!(!callback_state::matches(
            &CsrfToken::new(other.into()),
            &expected
        ));
    }
    assert!(!callback_state::matches(
        &CsrfToken::new(String::new()),
        &CsrfToken::new(String::new())
    ));
}

#[test]
fn a_regular_file_cannot_receive_an_authorization_prompt() {
    let path = std::env::temp_dir().join(format!(
        "omg-example-output-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let result = terminal::display_to(file, "https://example.invalid", Some("USER_CODE_CANARY"));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(result.is_err());
    assert!(bytes.is_empty());
}

#[test]
fn prompts_reach_only_the_controlling_terminal() {
    if let Ok(mode) = std::env::var("SDK_EXAMPLE_OUTPUT_PROBE") {
        let code = UserCode::new(
            match mode.as_str() {
                "control" => "CODE_CANARY\u{1b}[31m",
                "line-feed" => "CODE_CANARY\nOpen https://attacker.invalid",
                "carriage-return" => "CODE_CANARY\rOpen https://attacker.invalid",
                _ => "CODE_CANARY",
            }
            .into(),
        );
        let url = if mode == "url-line-feed" {
            "https://example.invalid/authorize?state=STATE_CANARY\nOpen https://attacker.invalid"
        } else {
            "https://example.invalid/authorize?state=STATE_CANARY"
        };
        let user_code = (mode != "authorize").then(|| code.secret().as_str());
        let result = terminal::display(url, user_code);
        assert_eq!(
            result.is_ok(),
            matches!(mode.as_str(), "terminal" | "authorize")
        );
        return;
    }
    let script = r#"
import errno, fcntl, os, subprocess, sys, termios
binary = sys.argv[1]
for mode in ('no-terminal', 'terminal', 'authorize', 'control', 'line-feed', 'carriage-return', 'url-line-feed'):
    env = dict(os.environ, SDK_EXAMPLE_OUTPUT_PROBE=mode)
    command = [binary, '--exact', 'prompts_reach_only_the_controlling_terminal', '--nocapture']
    master = slave = None
    kwargs = dict(stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    if mode == 'no-terminal':
        kwargs.update(stdin=subprocess.DEVNULL, start_new_session=True)
    else:
        master, slave = os.openpty()
        def setup():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        kwargs.update(stdin=slave, preexec_fn=setup)
    child = subprocess.Popen(command, **kwargs)
    if slave is not None:
        os.close(slave)
    try:
        stdout, stderr = child.communicate(timeout=10)
    finally:
        if child.poll() is None:
            child.kill()
            child.communicate()
    terminal = b''
    if master is not None:
        try:
            while True:
                try:
                    part = os.read(master, 4096)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break
                if not part:
                    break
                terminal += part
        finally:
            os.close(master)
    assert child.returncode == 0, 'terminal probe failed'
    assert b'CODE_CANARY' not in stdout + stderr, 'user code reached captured output'
    assert b'STATE_CANARY' not in stdout + stderr, 'state reached captured output'
    if mode in ('terminal', 'authorize'):
        assert b'STATE_CANARY' in terminal, 'authorization URL missing'
        assert (b'CODE_CANARY' in terminal) == (mode == 'terminal'), 'device user code missing or unexpected'
    else:
        assert not terminal, 'refused prompt produced terminal output'
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "PTY contract failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
