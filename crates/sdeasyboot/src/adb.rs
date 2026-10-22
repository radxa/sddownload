//! Native authenticated ADB bootloader reboot using libadb; no adb daemon or
//! executable is started. The transport retains bounded libusb timeouts.
use crate::usb::{wait_for, BulkTransport, UsbLink};
use anyhow::{Context, Result};
use libadb::keys::{rsa::rand_core::OsRng, AdbKey};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::Duration,
};

struct AdbTransport<T: BulkTransport>(T);

impl<T: BulkTransport> embedded_io::ErrorType for AdbTransport<T> {
    type Error = std::io::Error;
}

impl<T: BulkTransport> embedded_io_async::Read for AdbTransport<T> {
    async fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(b).map_err(std::io::Error::other)
    }
}

impl<T: BulkTransport> embedded_io_async::Write for AdbTransport<T> {
    async fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write(b).map_err(std::io::Error::other)
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn key(path: &Path) -> Result<AdbKey> {
    let mut rng = OsRng;
    let name = "sddownload@host";

    if path.exists() {
        let pem = fs::read_to_string(path)?;

        return AdbKey::from_pkcs8_pem(&pem, &mut rng, name)
            .or_else(|_| AdbKey::from_pkcs1_pem(&pem, &mut rng, name))
            .map_err(|e| anyhow::anyhow!("ADB key: {e:?}"));
    }

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }

    directories.create(parent)?;
    let key =
        AdbKey::generate(&mut rng, name).map_err(|e| anyhow::anyhow!("generate ADB key: {e:?}"))?;
    let pem = key
        .to_pkcs8_pem()
        .map_err(|e| anyhow::anyhow!("encode ADB key: {e:?}"))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(pem.as_bytes())?;

    let mut public_name = path
        .file_name()
        .context("invalid ADB key name")?
        .to_os_string();
    public_name.push(".pub");

    let public = path.with_file_name(public_name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);

    if let Ok(mut file) = options.open(public) {
        file.write_all(key.public_key_line())?;
    }

    Ok(key)
}

pub fn reboot_bootloader(serial: &str, key_path: &Path, detect_timeout: Duration) -> Result<()> {
    let info = wait_for(1, serial, detect_timeout)?;
    let auth = key(key_path)?;
    let transport = UsbLink::open(info, Duration::from_secs(5))?;

    futures::executor::block_on(authenticate_and_reboot(transport, auth))
}

async fn authenticate_and_reboot(transport: impl BulkTransport, auth: AdbKey) -> Result<()> {
    let mut connection = libadb::Connection::<_>::connect_with_raw_banner(
        AdbTransport(transport),
        auth,
        b"host::\0",
    )
    .await
    .map_err(|error| anyhow::anyhow!("ADB handshake: {error:?}"))?;

    let _channel = connection
        .open(b"reboot:bootloader\0")
        .await
        .map_err(|error| anyhow::anyhow!("ADB reboot: {error:?}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::ensure;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Default)]
    struct PeerState {
        pending: VecDeque<u8>,
        incoming: Vec<u8>,
        authenticated: bool,
        service: Vec<u8>,
    }

    struct Peer(Arc<Mutex<PeerState>>);

    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn packet(command: &[u8; 4], arg0: u32, arg1: u32, data: &[u8]) -> Vec<u8> {
        let command = u32::from_le_bytes(*command);
        let mut bytes = Vec::new();

        for value in [
            command,
            arg0,
            arg1,
            data.len() as u32,
            data.iter().map(|b| *b as u32).sum(),
            command ^ u32::MAX,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }

        bytes.extend_from_slice(data);
        bytes
    }

    impl BulkTransport for Peer {
        fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
            let mut peer = self.0.lock().unwrap();

            ensure!(
                !peer.pending.is_empty(),
                "ADB client read before a complete request"
            );

            let count = buffer.len().min(peer.pending.len()).min(13);

            for byte in &mut buffer[..count] {
                *byte = peer.pending.pop_front().unwrap();
            }

            Ok(count)
        }

        fn write(&mut self, buffer: &[u8]) -> Result<usize> {
            let mut peer = self.0.lock().unwrap();
            let count = buffer.len().min(13);
            peer.incoming.extend_from_slice(&buffer[..count]);

            while peer.incoming.len() >= 24 {
                let payload = word(&peer.incoming, 12) as usize;

                if peer.incoming.len() < 24 + payload {
                    break;
                }

                let request: Vec<_> = peer.incoming.drain(..24 + payload).collect();
                let command = &request[..4];

                match command {
                    b"CNXN" => {
                        peer.pending.extend(packet(b"AUTH", 1, 0, &[0x5a; 20]));
                    }
                    b"AUTH" => {
                        ensure!(
                            word(&request, 4) == 2 && payload == 256,
                            "expected RSA-2048 AUTH signature"
                        );
                        peer.authenticated = true;
                        peer.pending
                            .extend(packet(b"CNXN", 0x01000000, 4096, b"device::"));
                    }
                    b"OPEN" => {
                        ensure!(peer.authenticated, "service opened before authentication");
                        peer.service = request[24..].to_vec();
                        peer.pending
                            .extend(packet(b"OKAY", 7, word(&request, 4), &[]));
                    }
                    _ => anyhow::bail!("unexpected ADB command"),
                }
            }

            Ok(count)
        }
    }

    #[test]
    fn native_adb_authentication_and_bootloader_service() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("android/adbkey");
        let auth = key(&path).unwrap();
        let public = auth.public_key_line().to_vec();

        assert_eq!(key(&path).unwrap().public_key_line(), public);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let peer = Arc::new(Mutex::new(PeerState::default()));
        futures::executor::block_on(authenticate_and_reboot(Peer(peer.clone()), auth)).unwrap();

        let peer = peer.lock().unwrap();
        assert!(peer.authenticated);
        assert_eq!(peer.service, b"reboot:bootloader\0");
    }
}
