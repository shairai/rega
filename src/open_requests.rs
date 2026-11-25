//! Files the system asks Rega to open: a `.regaguide` double-clicked in
//! Finder or Explorer, dropped on the Dock icon, or chosen with "Open with".
//!
//! They arrive in different ways. macOS sends the app an open-documents event
//! (running or just launched), which reaches [`mac::install`]'s method on the
//! application delegate. Windows starts a new `Rega.exe` with the file as its
//! argument; when Rega is already running (it usually is, in the tray), that
//! new copy hands the file over with [`forward`] and exits.
//!
//! Either way the path lands in a queue the app empties with [`take`].

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static QUEUE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static WAKE: OnceLock<eframe::egui::Context> = OnceLock::new();

/// Ask for `path` to be opened. Safe from any thread, before or after the
/// app exists.
pub fn push(path: PathBuf) {
    log::info!("asked to open {}", path.display());
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(path);
    if let Some(ctx) = WAKE.get() {
        ctx.request_repaint();
    }
}

/// The files asked for since the last call.
pub fn take() -> Vec<PathBuf> {
    std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()))
}

pub fn pending() -> bool {
    !QUEUE.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
}

/// Wake the app (it may be hidden and asleep) whenever a file is asked for.
pub fn wake(ctx: &eframe::egui::Context) {
    let _ = WAKE.set(ctx.clone());
    if pending() {
        ctx.request_repaint();
    }
}

#[cfg(target_os = "macos")]
pub mod mac {
    use std::path::PathBuf;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{ClassType, define_class, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSApplicationWillFinishLaunchingNotification};
    use objc2_foundation::{MainThreadMarker, NSArray, NSNotification, NSNotificationCenter, NSObject, NSURL};

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no ivars, no Drop.
        #[unsafe(super(NSObject))]
        #[name = "RegaFileOpener"]
        struct FileOpener;

        impl FileOpener {
            /// `-[NSApplicationDelegate application:openURLs:]`. Lent to
            /// winit's delegate (see `will_finish_launching`), so `self` is
            /// that delegate and is left alone.
            #[unsafe(method(application:openURLs:))]
            fn open_urls(&self, _app: &AnyObject, urls: &NSArray<NSURL>) {
                for url in urls {
                    if let Some(path) = url.path() {
                        super::push(PathBuf::from(path.to_string()));
                    }
                }
            }

            /// The application exists and has winit's delegate, and nothing
            /// has been sent to it yet: the file Rega was launched to open
            /// comes before `applicationDidFinishLaunching:`.
            #[unsafe(method(willFinishLaunching:))]
            fn will_finish_launching(&self, _notification: &NSNotification) {
                let Some(mtm) = MainThreadMarker::new() else { return };
                let app = NSApplication::sharedApplication(mtm);
                let Some(delegate) = app.delegate() else {
                    log::warn!("no application delegate: files opened from Finder won't reach Rega");
                    return;
                };
                let open = sel!(application:openURLs:);
                // SAFETY: the method is copied, with its own type encoding,
                // onto a live class; it never touches `self`, so the class it
                // runs on doesn't matter.
                let added = unsafe {
                    let method = objc2::ffi::class_getInstanceMethod(Self::class(), open);
                    let delegate_ptr = Retained::as_ptr(&delegate) as *const AnyObject;
                    let class = objc2::ffi::object_getClass(delegate_ptr) as *mut AnyClass;
                    match objc2::ffi::method_getImplementation(method) {
                        Some(imp) => objc2::ffi::class_addMethod(class, open, imp, objc2::ffi::method_getTypeEncoding(method)).as_bool(),
                        None => false,
                    }
                };
                if !added {
                    log::warn!("could not take over opening files from the application delegate");
                    return;
                }
                // AppKit may have noted what the delegate answers to when it was set.
                app.setDelegate(None);
                app.setDelegate(Some(&delegate));
            }
        }
    );

    /// Tell Launch Services about this copy of Rega again, so the file types
    /// in its Info.plist hold. It reads them when it first sees an app, and
    /// may not look again at one that was replaced where it stood (an update
    /// from an older Rega that opened no files): a double-clicked guide would
    /// then find no application.
    fn register_file_types() {
        #[link(name = "CoreServices", kind = "framework")]
        unsafe extern "C" {
            fn LSRegisterURL(url: *const std::ffi::c_void, update: u8) -> i32;
        }
        let bundle = objc2_foundation::NSBundle::mainBundle().bundleURL();
        // Run straight from `target/` there is no bundle to speak of.
        if !bundle.path().is_some_and(|p| p.to_string().ends_with(".app")) {
            return;
        }
        // SAFETY: NSURL is toll-free bridged to the CFURLRef this takes.
        let status = unsafe { LSRegisterURL(Retained::as_ptr(&bundle).cast(), 1) };
        if status != 0 {
            log::warn!("registering Rega's file types failed ({status})");
        }
    }

    /// Call before the event loop is made: Rega's delegate is winit's, which
    /// knows nothing of files, so the method that takes them is added to it
    /// as soon as it is in place.
    pub fn install() {
        register_file_types();
        // SAFETY: plain `+new` of an NSObject subclass; the observer stays
        // alive for the life of the app (forgotten below).
        unsafe {
            let opener: Retained<FileOpener> = msg_send![FileOpener::class(), new];
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &opener,
                sel!(willFinishLaunching:),
                Some(NSApplicationWillFinishLaunchingNotification),
                None,
            );
            std::mem::forget(opener);
        }
    }
}

/// Handing a file from a second copy of Rega to the one already running, over
/// a connection to this computer only. The running copy listens on a port of
/// the system's choosing and leaves its number in a file next to the settings.
#[cfg(any(windows, test))]
pub mod handoff {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const HELLO: &str = "rega-open 1\n";
    const TAKEN: &[u8] = b"taken\n";

    pub fn port_file() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join(crate::config::APP_NAME).join("open.port"))
    }

    /// Start taking files from later copies. Each one goes to `open`.
    pub fn listen(port_file: &Path, open: impl Fn(PathBuf) + Send + 'static) -> std::io::Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        if let Some(dir) = port_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(port_file, listener.local_addr()?.port().to_string())?;
        std::thread::Builder::new().name("open-requests".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                match receive(stream) {
                    Ok(path) => open(path),
                    Err(e) => log::warn!("a request to open a file was not understood: {e}"),
                }
            }
        })?;
        Ok(())
    }

    fn receive(mut stream: TcpStream) -> std::io::Result<PathBuf> {
        use std::io::{Error, ErrorKind::InvalidData};
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        // The sender closes its side after the path, which ends the read.
        let mut text = String::new();
        (&stream).take(64 * 1024).read_to_string(&mut text)?;
        let path = text.strip_prefix(HELLO).ok_or_else(|| Error::new(InvalidData, "not from Rega"))?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(Error::new(InvalidData, "not a full path"));
        }
        stream.write_all(TAKEN)?;
        Ok(path)
    }

    /// Give `path` to the copy of Rega already running. False when there is
    /// none (or it didn't answer): this copy is to open the file itself.
    pub fn forward(port_file: &Path, path: &Path) -> bool {
        let Some(path) = path.to_str() else { return false };
        let Some(port) = std::fs::read_to_string(port_file).ok().and_then(|p| p.trim().parse::<u16>().ok()) else { return false };
        let send = || -> std::io::Result<bool> {
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.write_all(format!("{HELLO}{path}").as_bytes())?;
            stream.shutdown(std::net::Shutdown::Write)?;
            // Something else may have that port now: only Rega answers so.
            let mut answer = Vec::new();
            stream.take(64).read_to_end(&mut answer)?;
            Ok(answer == TAKEN)
        };
        send().unwrap_or(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A second copy's file reaches the first; with nobody listening
        /// (no port file, a stale one, or another program on the port) the
        /// second copy is told to open it itself.
        #[test]
        fn a_file_is_handed_to_the_running_copy() {
            let dir = std::env::temp_dir().join(format!("rega-handoff-{}", std::process::id()));
            let port_file = dir.join("open.port");
            let file = dir.join("How to ship שלום.regaguide");
            assert!(!forward(&port_file, &file), "no port file");

            let (tx, rx) = std::sync::mpsc::channel();
            listen(&port_file, move |p| tx.send(p).unwrap()).unwrap();
            assert!(forward(&port_file, &file));
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), file);
            assert!(forward(&port_file, &file), "and again");
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), file);

            // Another program on the port: it doesn't answer as Rega does.
            let other = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            std::fs::write(&port_file, other.local_addr().unwrap().port().to_string()).unwrap();
            std::thread::spawn(move || {
                if let Ok((mut s, _)) = other.accept() {
                    let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
                }
            });
            assert!(!forward(&port_file, &file), "not Rega");

            // A port nobody listens on any more.
            let gone = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
            std::fs::write(&port_file, gone.to_string()).unwrap();
            assert!(!forward(&port_file, &file), "stale port");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
