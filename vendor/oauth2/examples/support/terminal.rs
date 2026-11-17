//! Interactive authorization handoffs never use captured stdout or stderr.
use std::fmt::Arguments;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};

pub(crate) fn display(prompt: Arguments<'_>) -> io::Result<()> {
    #[cfg(unix)]
    let path = "/dev/tty";
    #[cfg(windows)]
    let path = "CONOUT$";
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "an interactive terminal is required",
    ));

    #[cfg(any(unix, windows))]
    display_to(OpenOptions::new().write(true).open(path)?, prompt)
}

pub(crate) fn display_to(mut terminal: File, prompt: Arguments<'_>) -> io::Result<()> {
    if !terminal.is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "authorization prompts require an interactive terminal",
        ));
    }
    let prompt = prompt.to_string();
    if prompt.chars().any(|c| c.is_control() && c != '\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authorization prompt contains a control character",
        ));
    }
    writeln!(terminal, "{prompt}")?;
    terminal.flush()
}
