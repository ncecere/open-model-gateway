//! A session's device summary for "Your sessions" (#79): a browser and OS
//! family from a fixed vocabulary, such as "Firefox on macOS". The raw
//! User-Agent and the client address are never stored.

/// `None` when nothing recognisable was sent.
pub(crate) fn client_label(user_agent: Option<&str>) -> Option<String> {
    let ua = user_agent?;
    // Order matters: Edge and Opera also say "Chrome"; Chrome also says "Safari".
    let browser = [
        ("Edg/", "Edge"),
        ("EdgA/", "Edge"),
        ("EdgiOS/", "Edge"),
        ("OPR/", "Opera"),
        ("Firefox/", "Firefox"),
        ("FxiOS/", "Firefox"),
        ("CriOS/", "Chrome"),
        ("Chrome/", "Chrome"),
        ("Safari/", "Safari"),
    ]
    .iter()
    .find(|(marker, _)| ua.contains(marker))
    .map(|(_, name)| *name);
    // iPadOS and iOS both claim "like Mac OS X"; Android also says "Linux".
    let os = [
        ("iPad", "iPadOS"),
        ("iPhone", "iOS"),
        ("Android", "Android"),
        ("CrOS", "ChromeOS"),
        ("Windows", "Windows"),
        ("Mac OS X", "macOS"),
        ("Macintosh", "macOS"),
        ("Linux", "Linux"),
    ]
    .iter()
    .find(|(marker, _)| ua.contains(marker))
    .map(|(_, name)| *name);
    match (browser, os) {
        (Some(b), Some(o)) => Some(format!("{b} on {o}")),
        (Some(b), None) => Some(b.to_owned()),
        (None, Some(o)) => Some(format!("Browser on {o}")),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::client_label;

    #[test]
    fn labels_come_from_a_fixed_vocabulary() {
        let cases = [
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
                Some("Safari on macOS"),
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36 Edg/129.0.0.0",
                Some("Edge on Windows"),
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0",
                Some("Firefox on Linux"),
            ),
            (
                "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0 Mobile Safari/537.36",
                Some("Chrome on Android"),
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/129.0 Mobile/15E148 Safari/604.1",
                Some("Chrome on iOS"),
            ),
            (
                "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Version/17.5 Mobile/15E148 Safari/604.1",
                Some("Safari on iPadOS"),
            ),
            (
                "Mozilla/5.0 (X11; CrOS x86_64 14541.0.0) AppleWebKit/537.36 Chrome/129.0.0.0 Safari/537.36",
                Some("Chrome on ChromeOS"),
            ),
            (
                "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/129.0 Mobile Safari/537.36 EdgA/129.0",
                Some("Edge on Android"),
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Version/17.0 EdgiOS/129.0 Mobile/15E148 Safari/604.1",
                Some("Edge on iOS"),
            ),
            ("curl/8.7.1", None),
            (
                "<script>alert(1)</script> Windows",
                Some("Browser on Windows"),
            ),
        ];
        for (ua, want) in cases {
            assert_eq!(client_label(Some(ua)).as_deref(), want, "{ua}");
        }
        assert_eq!(client_label(None), None);
        // Every possible label fits the column's CHECK (^[A-Za-z0-9 ]{1,40}$).
        for ua in [
            "Edg/ Windows",
            "FxiOS/ iPad",
            "OPR/ CrOS",
            "Safari/",
            "Macintosh",
        ] {
            let label = client_label(Some(ua)).unwrap();
            assert!(
                label.len() <= 40 && label.chars().all(|c| c.is_ascii_alphanumeric() || c == ' '),
                "{label}"
            );
        }
    }
}
