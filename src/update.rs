//! Looks for a newer Rega on the website: `<SITE_URL>/latest.json`, written
//! by `scripts/build-site.py`. Rega only says that a version is out and opens
//! the site's downloads; the download and install are the user's.
//!
//! The request goes through the system's own curl (part of macOS, and of
//! Windows 10 since version 1803), so Rega carries no HTTP or TLS library.
//! It sends nothing but Rega's version, in the user agent.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

use crate::config::SITE_URL;

/// How often a running Rega looks again, by the clock (time asleep counts).
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The parts of `latest.json` the app uses.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Latest {
    pub version: String,
    /// One-line summary of the release.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub downloads: Downloads,
}

/// The installers on the website.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Downloads {
    pub macos: Option<Download>,
    pub windows: Option<Download>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Download {
    pub url: String,
    pub size: u64,
    /// SHA-256 of the file, in hex (listed since 0.4.7).
    #[serde(default)]
    pub sha256: String,
}

impl Latest {
    /// This platform's installer, if the site lists it with a checksum to
    /// check it by: what an update in place installs.
    pub fn installer(&self) -> Option<&Download> {
        let d = if cfg!(target_os = "macos") {
            self.downloads.macos.as_ref()
        } else if cfg!(windows) {
            self.downloads.windows.as_ref()
        } else {
            None
        };
        d.filter(|d| d.size > 0 && d.sha256.len() == 64 && d.sha256.bytes().all(|b| b.is_ascii_hexdigit()))
    }
}

/// The newest version on the website, if it's newer than this build.
pub fn check() -> anyhow::Result<Option<Latest>> {
    let latest: Latest = serde_json::from_slice(&fetch(&latest_url())?)?;
    Ok(is_newer(&latest.version, env!("CARGO_PKG_VERSION")).then_some(latest))
}

/// The website's download section. (Rega 0.4.2 and 0.4.3 open `/releases`,
/// which the site redirects there.)
pub fn download_url() -> String {
    format!("{SITE_URL}/#download")
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
    hide_window(&mut cmd);
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{}", err.trim().trim_start_matches("curl: "));
    }
    Ok(out.stdout)
}

// ---------- updating in place ----------

/// How an update in place is coming along.
pub enum Progress {
    /// The share of the installer downloaded so far.
    Downloading(f32),
    /// Checking its checksum and, on macOS, its signature.
    Checking,
    /// Checked and unpacked: `apply` quits into it.
    Ready(Prepared),
    Failed(String),
}

/// A checked new version, ready to take this one's place.
pub struct Prepared {
    /// macOS: the new Rega.app; Windows: the setup program.
    new: PathBuf,
    /// macOS: the Rega.app it replaces; Windows: rega.exe, started once installed.
    target: PathBuf,
    /// The scratch folder holding the download.
    work: PathBuf,
}

/// Where this copy of Rega can replace itself with a new version, or why it
/// can't (then the download page is the way).
#[cfg(target_os = "macos")]
pub fn install_target() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|b| b.extension().is_some_and(|e| e == "app"))
        .ok_or("not running from an app bundle")?
        .to_path_buf();
    let path = bundle.to_string_lossy();
    if path.starts_with("/Volumes/") {
        return Err("running from the disk image".into());
    }
    if path.contains("/AppTranslocation/") {
        return Err("macOS runs it from a temporary copy until it's moved to Applications".into());
    }
    let parent = bundle.parent().ok_or("no folder")?;
    if !writable(parent) {
        return Err(format!("can't write to {}", parent.display()));
    }
    if team_id(&bundle).is_none() {
        return Err("this build isn't signed with a Developer ID".into());
    }
    Ok(bundle)
}

/// Windows: the copy the installer put in `%LOCALAPPDATA%\Programs\Rega`
/// (installed per user: no administrator needed to replace it).
#[cfg(windows)]
pub fn install_target() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let local = std::env::var_os("LOCALAPPDATA").ok_or("no LOCALAPPDATA")?;
    let installed = Path::new(&local).join("Programs").join("Rega");
    let same = |a: &Path, b: &Path| {
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
        canon(a) == canon(b)
    };
    match exe.parent() {
        Some(dir) if same(dir, &installed) => Ok(exe),
        _ => Err("not the installed copy".into()),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn install_target() -> Result<PathBuf, String> {
    Err("not supported here".into())
}

#[cfg(target_os = "macos")]
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".rega-update-check-{}", std::process::id()));
    let ok = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The Apple team that signed `bundle` ("A1B2C3D4E5"); `None` for an
/// unsigned or ad-hoc signed build.
#[cfg(target_os = "macos")]
fn team_id(bundle: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/codesign").args(["-dv", "--verbose=2"]).arg(bundle).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let team = text.lines().find_map(|l| l.strip_prefix("TeamIdentifier="))?.trim();
    // Ten letters and digits; anything else ("not set") is no team.
    (team.len() == 10 && team.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| team.to_owned())
}

/// Download, check and unpack `installer` (version `version`) on a
/// background thread, to replace `target`; `notify` wakes the UI for each
/// step.
pub fn prepare(version: String, installer: Download, target: PathBuf, notify: impl Fn() + Send + 'static) -> std::sync::mpsc::Receiver<Progress> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let send = |p: Progress| {
            let _ = tx.send(p);
            notify();
        };
        let result = (|| -> anyhow::Result<Prepared> {
            let work = std::env::temp_dir().join(format!("rega-update-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work)?;
            let name: String = installer.url.rsplit('/').next().unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
            let wanted = if cfg!(windows) { ".exe" } else { ".dmg" };
            if !name.ends_with(wanted) || name.starts_with('.') {
                anyhow::bail!("the website lists an unexpected installer ({name})");
            }
            let file = work.join(&name);
            download(&installer, &file, |share| send(Progress::Downloading(share)))?;
            send(Progress::Checking);
            if !sha256_hex(&file)?.eq_ignore_ascii_case(&installer.sha256) {
                anyhow::bail!("the download doesn't match the website's checksum");
            }
            let new = unpack(&file, &work, &version, &target)?;
            Ok(Prepared { new, target, work })
        })();
        send(match result {
            Ok(prepared) => Progress::Ready(prepared),
            Err(e) => Progress::Failed(format!("{e:#}")),
        });
    });
    rx
}

/// Download `d` to `to` through the system curl, reporting the share done.
fn download(d: &Download, to: &Path, progress: impl Fn(f32)) -> anyhow::Result<()> {
    use std::io::Read;
    let local = ["http://127.0.0.1:", "http://localhost:"].iter().any(|p| d.url.starts_with(p));
    if !d.url.starts_with("https://") && !local {
        anyhow::bail!("the installer isn't served over HTTPS");
    }
    let mut cmd = Command::new(curl());
    cmd.args(["--fail", "--silent", "--show-error", "--location", "--max-time", "900"])
        .args(["--max-filesize", &(d.size + 1024).to_string()])
        .args(["--user-agent", concat!("Rega/", env!("CARGO_PKG_VERSION"))]);
    if !local {
        cmd.args(["--proto", "=https", "--proto-redir", "=https"]);
    }
    cmd.arg("--output").arg(to).arg(&d.url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    hide_window(&mut cmd);
    let mut child = cmd.spawn()?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let got = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
        progress((got as f32 / d.size as f32).min(1.0));
        std::thread::sleep(Duration::from_millis(120));
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut err);
        }
        anyhow::bail!("download failed: {}", err.trim().trim_start_matches("curl: "));
    }
    let got = std::fs::metadata(to)?.len();
    if got != d.size {
        anyhow::bail!("the download is {got} bytes, not {}", d.size);
    }
    progress(1.0);
    Ok(())
}

fn sha256_hex(file: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(file)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// macOS: take Rega.app out of the disk image, and accept it only if it is
/// version `version`, intact, and signed by the same developer as `target`
/// (this copy): a tampered website can't hand over anything else.
#[cfg(target_os = "macos")]
fn unpack(dmg: &Path, work: &Path, version: &str, target: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let mount = work.join("mount");
    std::fs::create_dir_all(&mount)?;
    run(Command::new("/usr/bin/hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet", "-mountpoint"]).arg(&mount).arg(dmg))
        .context("couldn't open the disk image")?;
    let new = work.join("Rega.app");
    let copied = run(Command::new("/usr/bin/ditto").arg(mount.join("Rega.app")).arg(&new));
    let _ = run(Command::new("/usr/bin/hdiutil").args(["detach", "-quiet", "-force"]).arg(&mount));
    copied.context("couldn't copy Rega out of the disk image")?;

    let team = team_id(target).context("this copy isn't signed with a Developer ID")?;
    if team_id(&new).as_deref() != Some(team.as_str()) {
        anyhow::bail!("the new version isn't signed by the same developer");
    }
    let requirement = format!("-R=anchor apple generic and certificate leaf[subject.OU] = \"{team}\"");
    run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(requirement).arg(&new))
        .context("the new version's signature doesn't check out")?;
    let plist = new.join("Contents/Info.plist");
    let out = Command::new("/usr/libexec/PlistBuddy").args(["-c", "Print :CFBundleShortVersionString"]).arg(&plist).output()?;
    let got = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if got != version {
        anyhow::bail!("the download is version {got}, not {version}");
    }
    Ok(new)
}

/// Windows: the setup program is installed as it is (the checksum checked it).
#[cfg(not(target_os = "macos"))]
fn unpack(setup: &Path, _work: &Path, _version: &str, _target: &Path) -> anyhow::Result<PathBuf> {
    Ok(setup.to_path_buf())
}

/// Hand over to the new version: a helper that outlives Rega waits for it
/// to quit, puts the new version in place and starts it. The caller quits.
#[cfg(target_os = "macos")]
pub fn apply(p: &Prepared) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    swap_command(std::process::id(), p, "/usr/bin/open")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// The old copy is set aside first and comes back if the new one can't move
/// in, so a failure leaves Rega as it was; then `opener` starts it.
#[cfg(target_os = "macos")]
const SWAP_SCRIPT: &str = r#"
pid=$1; old=$2; new=$3; work=$4; opener=$5
while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
backup="$work/Rega-previous.app"
if mv "$old" "$backup"; then
  if mv "$new" "$old"; then rm -rf "$backup"; else mv "$backup" "$old"; fi
fi
"$opener" "$old"
case "$work" in */rega-update-*) rm -rf "$work" ;; esac
"#;

/// The helper that waits for process `pid` to end, then swaps the new
/// version in and starts it with `opener`.
#[cfg(target_os = "macos")]
fn swap_command(pid: u32, p: &Prepared, opener: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", SWAP_SCRIPT, "rega-update", &pid.to_string()]).args([&p.target, &p.new, &p.work]).arg(opener);
    cmd
}

/// Windows: run the setup silently once Rega has quit (it installs per user,
/// with no administrator prompt), then start the new Rega.
#[cfg(windows)]
pub fn apply(p: &Prepared) -> anyhow::Result<()> {
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "''"));
    let script = format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; Start-Process -FilePath {setup} -ArgumentList '/S' -Wait; Start-Process -FilePath {exe}; Remove-Item -LiteralPath {work} -Recurse -Force -ErrorAction SilentlyContinue",
        pid = std::process::id(),
        setup = quote(&p.new),
        exe = quote(&p.target),
        work = quote(&p.work),
    );
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let mut cmd = Command::new(format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe"));
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_window(&mut cmd);
    cmd.spawn()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn apply(_p: &Prepared) -> anyhow::Result<()> {
    anyhow::bail!("not supported here")
}

#[cfg(target_os = "macos")]
fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Windows: no console window flashing up for a helper program.
fn hide_window(_cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        _cmd.creation_flags(CREATE_NO_WINDOW);
    }
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
        assert_eq!(latest, Latest { version: "99.0.0".into(), title: "From the future.".into(), downloads: Downloads::default() });
        let request = server.join().unwrap();
        assert!(request.contains(concat!("User-Agent: Rega/", env!("CARGO_PKG_VERSION"))), "{request}");
    }

    #[test]
    fn a_failed_request_is_an_error() {
        // Nothing listens on a port that was just freed.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(fetch(&format!("http://127.0.0.1:{port}/latest.json")).is_err());
    }

    /// The installer is offered for updating in place only with a checksum
    /// to check it by.
    #[test]
    fn the_installer_needs_a_checksum() {
        let good = Download { url: "https://example.com/a".into(), size: 10, sha256: "ab".repeat(32) };
        let latest = |sha256: &str| Latest {
            version: "9.0.0".into(),
            title: String::new(),
            downloads: Downloads {
                macos: Some(Download { sha256: sha256.into(), ..good.clone() }),
                windows: Some(Download { sha256: sha256.into(), ..good.clone() }),
            },
        };
        let here = cfg!(any(target_os = "macos", windows));
        assert_eq!(latest(&good.sha256).installer().is_some(), here);
        assert!(latest("").installer().is_none(), "a site from before 0.4.7");
        assert!(latest(&"zz".repeat(32)).installer().is_none(), "not hex");
    }

    /// Answers one request on this machine with `body`; its address.
    fn serve(body: Vec<u8>, name: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(&body);
        });
        format!("http://127.0.0.1:{port}/{name}")
    }

    /// Waits for an update's last step.
    fn outcome(rx: std::sync::mpsc::Receiver<Progress>) -> Progress {
        loop {
            match rx.recv_timeout(Duration::from_secs(120)).expect("an outcome") {
                Progress::Downloading(_) | Progress::Checking => continue,
                last => return last,
            }
        }
    }

    /// A download that isn't the file the website vouches for is refused
    /// before anything is installed.
    #[test]
    fn a_download_that_doesnt_match_is_refused() {
        let body = b"not the installer you were promised".to_vec();
        let name = if cfg!(windows) { "Rega-9.0.0-setup.exe" } else { "Rega-9.0.0.dmg" };
        let installer = Download { url: serve(body.clone(), name), size: body.len() as u64, sha256: "00".repeat(32) };
        let rx = prepare("9.0.0".into(), installer, PathBuf::from("unused"), || {});
        match outcome(rx) {
            Progress::Failed(why) => assert!(why.contains("checksum"), "{why}"),
            _ => panic!("installed a file that doesn't match its checksum"),
        }
    }

    /// The helper waits for Rega to quit, then swaps the new version in; if
    /// the new one can't move in, the old one goes back where it was.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_swap_puts_the_new_version_in_place() {
        let root = std::env::temp_dir().join(format!("rega-swap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let swap = |make_new: bool| {
            let work = root.join(format!("rega-update-{make_new}"));
            let (old, new) = (root.join(format!("Rega-{make_new}.app")), work.join("Rega.app"));
            std::fs::create_dir_all(&old).unwrap();
            std::fs::write(old.join("version"), "old").unwrap();
            std::fs::create_dir_all(&work).unwrap();
            if make_new {
                std::fs::create_dir_all(&new).unwrap();
                std::fs::write(new.join("version"), "new").unwrap();
            }
            // "Rega": a process that quits a moment later. Reaped as soon as it
            // does, as the system reaps the real one: a zombie would still
            // answer `kill -0` and keep the helper waiting.
            let mut rega = Command::new("/bin/sleep").arg("0.3").spawn().unwrap();
            let pid = rega.id();
            let reaper = std::thread::spawn(move || rega.wait());
            let p = Prepared { new, target: old.clone(), work: work.clone() };
            let status = swap_command(pid, &p, "/usr/bin/true").status().unwrap();
            let _ = reaper.join();
            assert!(status.success());
            (std::fs::read_to_string(old.join("version")).unwrap(), work.exists())
        };
        assert_eq!(swap(true), ("new".into(), false), "the new version is in place, the scratch folder gone");
        assert_eq!(swap(false), ("old".into(), false), "nothing to move in: the old one is back");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole of an update in place from this version's notarized disk
    /// image in `dist/`, short of the swap: downloaded, checked and taken
    /// out, signed by the same developer as `target/release/Rega.app`. Build
    /// both first; `cargo test --release updates_from_the_real_disk_image -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn updates_from_the_real_disk_image() {
        let version = env!("CARGO_PKG_VERSION");
        let dmg = std::fs::read(format!("dist/Rega-{version}-macos-universal.dmg")).expect("run scripts/package-macos.sh first");
        let sha256 = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&dmg).iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        // An installed copy to update: the signed build, copied.
        let installed = std::env::temp_dir().join(format!("rega-installed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&installed);
        std::fs::create_dir_all(&installed).unwrap();
        let target = installed.join("Rega.app");
        assert!(Command::new("/usr/bin/ditto").arg("target/release/Rega.app").arg(&target).status().unwrap().success());

        let installer = Download { url: serve(dmg.clone(), "Rega.dmg"), size: dmg.len() as u64, sha256 };
        match outcome(prepare(version.into(), installer, target, || {})) {
            Progress::Ready(p) => {
                assert!(p.new.join("Contents/MacOS/Rega").exists());
                let _ = std::fs::remove_dir_all(&p.work);
            }
            Progress::Failed(why) => panic!("{why}"),
            _ => unreachable!(),
        }
        let _ = std::fs::remove_dir_all(&installed);
    }
}
