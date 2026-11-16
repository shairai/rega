//! Looks for a newer Rega on the website: `<SITE_URL>/latest.json`, written
//! by `scripts/build-site.py`. Rega only says that a version is out and opens
//! the site's What's new page; the download and install are the user's.
//!
//! The request goes through the system's own curl (part of macOS, and of
//! Windows 10 since version 1803), so Rega carries no HTTP or TLS library.
//! It sends nothing but Rega's version, in the user agent.

use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::config::SITE_URL;

/// How often a running Rega looks again.
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The parts of `latest.json` the app uses.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Latest {
    pub version: String,
    /// One-line summary of the release.
    #[serde(default)]
    pub title: String,
}

/// The newest version on the website, if it's newer than this build.
pub fn check() -> anyhow::Result<Option<Latest>> {
    let latest: Latest = serde_json::from_slice(&fetch(&latest_url())?)?;
    Ok(is_newer(&latest.version, env!("CARGO_PKG_VERSION")).then_some(latest))
}

/// The website's page with the downloads and every version's notes.
pub fn releases_url() -> String {
    format!("{SITE_URL}/releases")
}

fn latest_url() -> String {
    // A local copy of the site, for trying the notice out.
    std::env::var("REGA_UPDATE_URL").unwrap_or_else(|_| format!("{SITE_URL}/latest.json"))
}

/// `a` is a later version than `b`, compared as numbers ("0.10.0" is later
/// than "0.9.3"). Anything but three numbers is never later.
pub fn is_newer(a: &str, b: &str) -> bool {
    matches!((parse(a), parse(b)), (Some(a), Some(b)) if a > b)
}

fn parse(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.trim().split('.').map(|p| p.parse::<u32>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    let mut cmd = Command::new(curl());
    cmd.args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--max-time", "20", "--max-filesize", "65536"])
        .args(["--user-agent", concat!("Rega/", env!("CARGO_PKG_VERSION"))]);
    if url.starts_with("https://") {
        // Never let a redirect downgrade the real site to plain HTTP.
        cmd.args(["--proto", "=https", "--proto-redir", "=https"]);
    }
    cmd.arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{}", err.trim().trim_start_matches("curl: "));
    }
    Ok(out.stdout)
}

/// The system's curl by full path, so no other `curl` on the PATH is run.
fn curl() -> String {
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        format!(r"{root}\System32\curl.exe")
    } else {
        "/usr/bin/curl".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn versions_compare_as_numbers() {
        assert!(is_newer("0.4.2", "0.4.1"));
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.4.1", "0.4.1"));
        assert!(!is_newer("0.4.0", "0.4.1"));
        // Not a version: never offered.
        assert!(!is_newer("0.5", "0.4.1"));
        assert!(!is_newer("0.5.0-beta", "0.4.1"));
        assert!(!is_newer("", "0.4.1"));
    }

    /// The app reads what `scripts/build-site.py` writes.
    #[test]
    fn reads_the_sites_latest_json() {
        let latest: Latest = serde_json::from_str(include_str!("../site/latest.json")).unwrap();
        assert!(parse(&latest.version).is_some(), "{latest:?}");
        assert!(!latest.title.is_empty());
    }

    /// One request through the system curl, to a server on this machine.
    #[test]
    fn fetches_with_rega_user_agent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let n = stream.read(&mut request).unwrap();
            let body = r#"{"version":"99.0.0","title":"From the future.","notes":[]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        let body = fetch(&format!("http://127.0.0.1:{port}/latest.json")).unwrap();
        let latest: Latest = serde_json::from_slice(&body).unwrap();
        assert_eq!(latest, Latest { version: "99.0.0".into(), title: "From the future.".into() });
        let request = server.join().unwrap();
        assert!(request.contains(concat!("User-Agent: Rega/", env!("CARGO_PKG_VERSION"))), "{request}");
    }

    #[test]
    fn a_failed_request_is_an_error() {
        // Nothing listens on a port that was just freed.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(fetch(&format!("http://127.0.0.1:{port}/latest.json")).is_err());
    }
}
