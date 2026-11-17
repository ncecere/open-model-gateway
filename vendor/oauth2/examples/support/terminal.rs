//! Interactive authorization handoffs never use captured stdout or stderr.
#[cfg(any(unix, windows))]
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(any(unix, windows))]
use std::io::{IsTerminal, Write};

pub(crate) fn display(url: &str, user_code: Option<&str>) -> io::Result<()> {
    #[cfg(unix)]
    let path = "/dev/tty";
    #[cfg(windows)]
    let path = "CONOUT$";
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (url, user_code);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "an interactive terminal is required",
        ))
    }

    #[cfg(any(unix, windows))]
    display_to(
        // Windows GetConsoleMode needs read access even for an output handle.
        OpenOptions::new()
            .read(cfg!(windows))
            .write(true)
            .open(path)?,
        url,
        user_code,
    )
}

#[cfg(any(unix, windows))]
pub(crate) fn display_to(mut terminal: File, url: &str, user_code: Option<&str>) -> io::Result<()> {
    if !terminal.is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "authorization prompts require an interactive terminal",
        ));
    }
    // Validate values before adding trusted layout, including its line feeds.
    if url.chars().any(char::is_control)
        || user_code.is_some_and(|code| code.chars().any(char::is_control))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authorization prompt contains a control character",
        ));
    }
    writeln!(terminal, "Open this URL in your browser:\n{url}")?;
    if let Some(code) = user_code {
        writeln!(terminal, "and enter the code: {code}")?;
    }
    terminal.flush()
}
