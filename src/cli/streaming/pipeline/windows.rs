use super::*;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::{
    Foundation::{
        DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF,
        ERROR_OPERATION_ABORTED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        Threading::{GetCurrentProcess, GetCurrentThread, WaitForSingleObject},
        IO::CancelSynchronousIo,
    },
};

#[derive(Default)]
pub(super) struct Cancellation {
    worker: Mutex<Option<Arc<OwnedHandle>>>,
}

impl Cancellation {
    pub(super) fn register(&self) -> io::Result<()> {
        let mut duplicate = std::ptr::null_mut();
        // SAFETY: pseudo-handles refer to this process/current input thread;
        // DuplicateHandle returns a real owned handle with synchronization and
        // cancellation rights. No pseudo-handle is passed to OwnedHandle.
        let success = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                GetCurrentThread(),
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful DuplicateHandle returned a fresh handle we own.
        let handle = unsafe { OwnedHandle::from_raw_handle(duplicate) };
        *self.worker.lock().unwrap() = Some(Arc::new(handle));
        Ok(())
    }

    pub(super) fn cancel(&self) {
        let handle = self.worker.lock().unwrap().clone();
        if let Some(handle) = handle {
            // SAFETY: the Arc keeps the registered input thread handle alive.
            // ERROR_NOT_FOUND simply means no synchronous I/O is pending yet.
            unsafe {
                CancelSynchronousIo(handle.as_raw_handle());
            }
        }
    }

    pub(super) fn cancel_until_finished(&self) {
        let handle = self.worker.lock().unwrap().clone();
        let Some(handle) = handle else {
            return;
        };
        loop {
            // SAFETY: handle is a duplicated thread handle with SYNCHRONIZE.
            let status = unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) };
            if status == WAIT_OBJECT_0 {
                return;
            }
            if status != WAIT_TIMEOUT {
                return;
            }
            // Cancellation can arrive between check() and ReadFile. Retry
            // until thread completion instead of trusting ERROR_NOT_FOUND.
            // SAFETY: the handle remains owned by this Arc throughout the wait.
            unsafe {
                CancelSynchronousIo(handle.as_raw_handle());
            }
            // SAFETY: same live thread handle, finite wait.
            if unsafe { WaitForSingleObject(handle.as_raw_handle(), POLL.as_millis() as u32) }
                == WAIT_OBJECT_0
            {
                return;
            }
        }
    }
}

pub(super) struct Input {
    pub(super) file: File,
    pub(super) stop: Arc<Stop>,
}

impl Read for Input {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.stop.check()?;
        // File's Windows backend performs synchronous ReadFile on ordinary
        // inherited stdin/pipe handles. The input worker is dedicated to us.
        let result = self.file.read(buffer);
        self.stop.check()?;
        match result {
            Err(error) if matches!(error.raw_os_error(), Some(code) if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_HANDLE_EOF as i32) => {
                Ok(0)
            }
            Err(error) if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => {
                Err(io::Error::other(error))
            }
            result => result,
        }
    }
}

pub(super) fn open_input(path: &Path, stop: &Arc<Stop>) -> Result<Box<dyn Read + Send>> {
    stop.check()?;
    let file = if path == Path::new("-") {
        // Duplicate the raw handle, preserving binary bytes and leaving the
        // caller's standard-input handle open when this operation completes.
        File::from(io::stdin().as_handle().try_clone_to_owned()?)
    } else {
        File::open(path).with_context(|| format!("open PAC {}", path.display()))?
    };
    stop.check()?;
    Ok(Box::new(Input {
        file,
        stop: stop.clone(),
    }))
}
