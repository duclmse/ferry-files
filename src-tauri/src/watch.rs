//! Telling the window when something is plugged in or taken out, so it looks for devices then
//! instead of every few seconds: macOS reports each USB device added or removed (IOKit), and
//! `/Volumes` is checked every 2 seconds for disks mounted or ejected (disk images aren't on USB).
//! Each change sends "devices-changed" three times over about 5 seconds: adb only sees a phone
//! a moment after it's on USB, and a phone plugged in is connected on the second look.

use std::ffi::{c_char, c_void};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

type Port = *mut c_void;
/// `io_object_t` and `io_iterator_t` are Mach ports.
type Object = u32;
type Callback = extern "C" fn(refcon: *mut c_void, iterator: Object);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IONotificationPortCreate(main_port: u32) -> Port;
    fn IONotificationPortGetRunLoopSource(port: Port) -> *mut c_void;
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceAddMatchingNotification(
        port: Port,
        kind: *const c_char,
        matching: *mut c_void,
        callback: Callback,
        refcon: *mut c_void,
        iterator: *mut Object,
    ) -> i32;
    fn IOIteratorNext(iterator: Object) -> Object;
    fn IOObjectRelease(object: Object) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
}

/// Releases what's in `iterator`, which also re-arms its notification. Tells whether there was any.
fn drain(iterator: Object) -> bool {
    let mut any = false;
    loop {
        let o = unsafe { IOIteratorNext(iterator) };
        if o == 0 {
            return any;
        }
        any = true;
        unsafe { IOObjectRelease(o) };
    }
}

extern "C" fn usb_changed(refcon: *mut c_void, iterator: Object) {
    if drain(iterator) {
        let tx = unsafe { &*(refcon as *const Sender<()>) };
        let _ = tx.send(());
    }
}

/// Listens for USB devices added and removed, on a thread of its own. Whether it could.
fn watch_usb(tx: Sender<()>) -> bool {
    let (ok_tx, ok_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Lives as long as the app: the callbacks get it as `refcon`.
        let refcon = Box::into_raw(Box::new(tx)) as *mut c_void;
        unsafe {
            let port = IONotificationPortCreate(0);
            if port.is_null() {
                let _ = ok_tx.send(false);
                return;
            }
            let mut ok = true;
            for kind in [c"IOServiceFirstMatch", c"IOServiceTerminate"] {
                // Each notification takes (and releases) its own matching dictionary.
                let matching = IOServiceMatching(c"IOUSBHostDevice".as_ptr());
                let mut iterator: Object = 0;
                if matching.is_null() || IOServiceAddMatchingNotification(port, kind.as_ptr(), matching, usb_changed, refcon, &mut iterator) != 0 {
                    ok = false;
                    break;
                }
                // The devices already there; draining arms the notification.
                drain(iterator);
            }
            let _ = ok_tx.send(ok);
            if !ok {
                return;
            }
            CFRunLoopAddSource(CFRunLoopGetCurrent(), IONotificationPortGetRunLoopSource(port), kCFRunLoopDefaultMode);
            CFRunLoopRun();
        }
    });
    ok_rx.recv().unwrap_or(false)
}

/// Starts watching, once. Whether USB devices are watched (the window looks every few seconds
/// otherwise).
pub fn start(app: &AppHandle) -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<()>();
        let usb = watch_usb(tx.clone());
        std::thread::spawn(move || {
            let mut drives = crate::connect::drives();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let now = crate::connect::drives();
                if now != drives {
                    drives = now;
                    if tx.send(()).is_err() {
                        return;
                    }
                }
            }
        });
        let app = app.clone();
        std::thread::spawn(move || {
            // Waits for changes to stop for a moment, then sends; twice more if nothing else changes.
            const WAITS: [u64; 3] = [500, 1500, 3000];
            while rx.recv().is_ok() {
                let mut i = 0;
                while i < WAITS.len() {
                    match rx.recv_timeout(Duration::from_millis(WAITS[i])) {
                        Ok(()) => i = 0,
                        Err(RecvTimeoutError::Timeout) => {
                            let _ = app.emit("devices-changed", ());
                            i += 1;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        });
        usb
    })
}
