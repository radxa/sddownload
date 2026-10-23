use super::*;
use std::{
    fs::OpenOptions,
    os::fd::{AsFd, AsRawFd},
    os::unix::fs::OpenOptionsExt,
};

// Do not use buffered Stdin: poll must see every byte we are waiting for.
pub(super) struct Input {
    pub(super) file: File,
    pub(super) stop: Arc<Stop>,
}

impl Read for Input {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            self.stop.check()?;
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: descriptor is valid for the duration of this call.
            let ready = unsafe { libc::poll(&mut descriptor, 1, POLL.as_millis() as i32) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready != 0 {
                self.stop.check()?;
                match self.file.read(buffer) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue
                    }
                    result => return result,
                }
            }
        }
    }
}

pub(super) fn open_input(path: &Path, stop: &Arc<Stop>) -> Result<Box<dyn Read + Send>> {
    let file = if path == Path::new("-") {
        File::from(io::stdin().as_fd().try_clone_to_owned()?)
    } else {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("open PAC {}", path.display()))?
    };
    Ok(Box::new(Input {
        file,
        stop: stop.clone(),
    }))
}
