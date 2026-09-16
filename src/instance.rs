//! Single instance (design doc 4, rule 6). A named mutex keeps a second
//! launch from doubling every printer's FTP session count: the second
//! process shows a native message and exits without touching a printer.

/// The name of the mutex, in the session namespace: one instance per
/// Windows session, which is what the session budget is about.
pub const MUTEX_NAME: &str = r"Local\BambuControl.SingleInstance";

/// Another instance of the app already holds the mutex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyRunning;

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS,
                                         GetLastError, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION,
                                                      MB_OK,
                                                      MB_SETFOREGROUND,
                                                      MessageBoxW};

    use super::AlreadyRunning;

    /// Holds the named mutex for the process' lifetime.
    #[derive(Debug)]
    pub struct Instance {
        handle: HANDLE,
    }

    // the handle is only closed, from the thread that drops the value
    unsafe impl Send for Instance {}

    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Takes the named mutex. `Err(AlreadyRunning)` means another instance
    /// holds it; a mutex that cannot be created at all (a locked-down
    /// session) lets the app run, since blocking the only instance would be
    /// worse than the session budget it protects.
    pub fn acquire(name: &str) -> Result<Instance, AlreadyRunning> {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated UTF-16 and outlives the call; a
        // null security descriptor and no initial owner are the documented
        // arguments for a plain named mutex.
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Ok(Instance { handle });
        }
        // SAFETY: called right after CreateMutexW on the same thread
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: a handle CreateMutexW returned, closed once
            unsafe { CloseHandle(handle) };
            return Err(AlreadyRunning);
        }
        Ok(Instance { handle })
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                // SAFETY: a handle CreateMutexW returned, closed once
                unsafe { CloseHandle(self.handle) };
            }
        }
    }

    /// The message the second launch shows. It names no printer and touches
    /// no network.
    pub fn show_already_running() {
        let text = wide("Bambu Control is already running.\n\nOnly one \
                         window talks to the printers at a time, so this \
                         one will close.");
        let title = wide("Bambu Control");
        // SAFETY: both strings are NUL-terminated UTF-16 and outlive the
        // call; a null window handle is a message box with no owner
        unsafe {
            MessageBoxW(ptr::null_mut(), text.as_ptr(), title.as_ptr(),
                        MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::AlreadyRunning;

    /// Nothing to hold: the app ships on Windows, and the other platforms
    /// exist for `cargo test` only.
    #[derive(Debug)]
    pub struct Instance;

    pub fn acquire(_name: &str) -> Result<Instance, AlreadyRunning> {
        Ok(Instance)
    }

    pub fn show_already_running() {
        eprintln!("Bambu Control is already running.");
    }
}

pub use imp::{Instance, acquire, show_already_running};

#[cfg(test)]
mod tests {
    use super::{AlreadyRunning, MUTEX_NAME, acquire};

    /// A name of this test process, so a running app keeps its own mutex.
    fn test_name(label: &str) -> String {
        format!(r"Local\BambuControl.Test.{label}.{}", std::process::id())
    }

    #[test]
    fn a_second_instance_is_refused_while_the_first_holds_the_mutex() {
        let name = test_name("second");
        let first = acquire(&name).expect("the first instance takes it");
        if cfg!(windows) {
            assert_eq!(acquire(&name).err(), Some(AlreadyRunning));
        }
        // and the name is free again once the first instance is gone
        drop(first);
        assert!(acquire(&name).is_ok());
    }

    #[test]
    fn the_mutex_name_is_the_documented_one() {
        // design doc 4, rule 6
        assert_eq!(MUTEX_NAME, r"Local\BambuControl.SingleInstance");
    }
}
