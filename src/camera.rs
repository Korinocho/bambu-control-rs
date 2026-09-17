//! Chamber camera client (P1/A1 protocol): TLS socket on :6000,
//! 80-byte auth packet, then length-prefixed JPEG frames. Frames are
//! decoded off-thread into egui ColorImages.
//!
//! The certificate is verified before the auth packet is written (issue #2,
//! design doc 5.3): the same anchored connector FTPS uses, so the printer's
//! leaf must be issued by the embedded `BBL CA` and carry the configured
//! serial. The access code sits in that packet, which is the first thing
//! this protocol sends, so a refused handshake must leave it unsent.
//!
//! Port 6000 presents the same certificate as 990 and 8883 on all three of
//! the owner's printers, TLS 1.2 with ECDHE-RSA-AES256-GCM-SHA384 and the
//! `BBL CA` alongside the leaf (measured 2026-09-17, design doc 5.3).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tls::{AnchoredConnector, PrinterTls, SessionConns};

/// Longest the camera waits for progress on its socket. Chosen for this
/// stream rather than inherited from the FTP profile: frames arrive
/// continuously, and a limit shorter than the gap between them would cut a
/// working video.
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// The chamber camera's port on every P1 and A1.
const CAMERA_PORT: u16 = 6000;

pub struct Camera {
    /// Latest decoded frame; the GUI takes it and uploads a texture.
    pub frame: Arc<Mutex<Option<egui::ColorImage>>>,
    pub status: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    /// the running session's connections: cancelling it ends a waiting read
    /// within CANCEL_POLL, which a socket shutdown from another thread does
    /// not do on Windows (design doc 5.2)
    conns: Arc<Mutex<Option<Arc<SessionConns>>>>,
}

fn auth_packet(access_code: &str) -> [u8; 80] {
    let mut pkt = [0u8; 80];
    pkt[0..4].copy_from_slice(&0x40u32.to_le_bytes());
    pkt[4..8].copy_from_slice(&0x3000u32.to_le_bytes());
    pkt[16..20].copy_from_slice(b"bblp");
    let code = access_code.as_bytes();
    let n = code.len().min(32);
    pkt[48..48 + n].copy_from_slice(&code[..n]);
    pkt
}

fn read_exact(stream: &mut impl Read, buf: &mut [u8]) -> std::io::Result<()> {
    stream.read_exact(buf)
}

impl Camera {
    pub fn start(ip: String, serial: String, access_code: String,
                 egui_ctx: egui::Context) -> Arc<Self> {
        let me = Arc::new(Self {
            frame: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new("connecting camera…".into())),
            stop: Arc::new(AtomicBool::new(false)),
            conns: Arc::new(Mutex::new(None)),
        });
        let handle = me.clone();
        std::thread::spawn(move || {
            handle.run(&ip, &serial, &access_code, &egui_ctx)
        });
        me
    }

    fn run(&self, ip: &str, serial: &str, access_code: &str,
           ctx: &egui::Context) {
        // the same verifier FTPS uses: anchored on the embedded BBL CA and
        // bound to this printer's serial (issue #2)
        let tls = match PrinterTls::new(serial) {
            Ok(tls) => tls,
            Err(e) => {
                // no serial configured: nothing to bind the certificate to,
                // so the camera does not connect at all
                *self.status.lock().unwrap() = format!("camera: {e}");
                ctx.request_repaint();
                return;
            }
        };
        while !self.stop.load(Ordering::Relaxed) {
            match self.session(ip, CAMERA_PORT, access_code, &tls, ctx) {
                Ok(()) => {}
                Err(e) => {
                    if self.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    *self.status.lock().unwrap() =
                        format!("camera retry: {e}");
                    ctx.request_repaint();
                    for _ in 0..25 {
                        if self.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        }
    }

    /// `port` is CAMERA_PORT in the app; the tests below point it at a
    /// local server. `ip` is both the address and the TLS server name, as
    /// it is for FTPS.
    fn session(&self, ip: &str, port: u16, access_code: &str,
               tls: &Arc<PrinterTls>, ctx: &egui::Context)
               -> anyhow::Result<()> {
        let raw = TcpStream::connect_timeout(
            &format!("{ip}:{port}").parse()?, Duration::from_secs(10))?;
        // its own resumption store, like every FTP session: rustls keys a
        // stored session by server name, not by port, so sharing one would
        // offer 990's ticket on 6000 (design doc 5.3)
        let config = tls.config_for_new_session()?;
        let conns = SessionConns::new();
        *self.conns.lock().unwrap() = Some(conns.clone());
        let connector =
            AnchoredConnector::new(config, conns.clone(), IO_TIMEOUT);
        // connect_stream runs and checks the handshake before it returns,
        // so the auth packet below — which carries the access code — is
        // written only to a printer whose certificate was accepted
        let mut tls = connector.connect_stream(ip, raw)?;
        tls.write_all(&auth_packet(access_code))?;
        *self.status.lock().unwrap() = "live".into();
        ctx.request_repaint();

        let mut header = [0u8; 16];
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            read_exact(&mut tls, &mut header)?;
            let size = u32::from_le_bytes(header[0..4].try_into()?) as usize;
            if size == 0 || size >= 5_000_000 {
                anyhow::bail!("bad frame header");
            }
            let mut payload = vec![0u8; size];
            read_exact(&mut tls, &mut payload)?;
            if payload.get(..2) != Some(&[0xff, 0xd8]) {
                continue;
            }
            let img = image::load_from_memory_with_format(
                &payload, image::ImageFormat::Jpeg)?;
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [w, h], rgba.as_raw());
            *self.frame.lock().unwrap() = Some(color);
            ctx.request_repaint();
        }
    }

    /// Ends the camera without waiting. Cancelling the session is what
    /// wakes a read blocked in the camera thread: on Windows a shutdown
    /// through a cloned handle returns Ok and leaves that read waiting its
    /// full timeout, which is why the FTP sessions stopped doing it
    /// (design doc 5.2).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(conns) = self.conns.lock().unwrap().take() {
            conns.cancel();
        }
    }
}

/// Issue #2: the camera's own first bytes after the handshake carry the
/// access code, so a certificate the verifier refuses must leave them
/// unsent. The assertion is not that the connection fails — it is that
/// nothing was written.
///
/// What these tests assert is therefore what reached the *server*: the TLS
/// records it read, and among them the count of content type 23
/// (application_data). Raw byte counts on the socket would prove nothing,
/// since a handshake and an alert are bytes too.
///
/// That count is only meaningful because both ends negotiate TLS 1.2 here,
/// where every record carries its type in the clear. Under TLS 1.3 the
/// outer type of every record after the ClientHello is 23, so "zero
/// application data" would be false for a working connection and these
/// tests would go quietly empty. That is why the positive control asserts
/// the negotiated version: enable 1.3 and it fails loudly instead.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::testkit::*;
    use crate::tls::{ConnOutcome, PrinterCertError, Refusal, TlsFailure};

    const ACCESS_CODE: &str = "12345678";
    /// The synthetic leaves carry TEST_SERIAL as their CN; this is any
    /// other serial, the one ftp.rs uses for its own mismatch test.
    const OTHER_SERIAL: &str = "01P00Z9X8W7V6U4";
    /// Every one of these answers well inside this.
    const BOUND: Duration = Duration::from_secs(5);

    /// One camera session against `peer`, with the printer TLS anchored on
    /// the test CA and `serial` configured.
    ///
    /// All three tests run through here on purpose: the positive control
    /// counts the same thing, the same way, through the same server as the
    /// two refusals. A control that counted differently would not control
    /// what it is there to control.
    fn camera_session(peer: &RecordingServer, serial: &str)
                      -> (anyhow::Result<()>, Arc<SessionConns>, Recorded) {
        let tls = test_tls(TEST_CA, serial);
        let camera = Camera {
            frame: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new(String::new())),
            stop: Arc::new(AtomicBool::new(false)),
            conns: Arc::new(Mutex::new(None)),
        };
        let result = camera.session("127.0.0.1", peer.port, ACCESS_CODE, &tls,
                                    &egui::Context::default());
        let conns = camera.conns.lock().unwrap().clone()
            .expect("the session published its records before connecting");
        // the server files the connection once the client has closed
        assert!(eventually(BOUND, || !peer.connections().is_empty()),
                "the server saw no connection at all");
        let seen = peer.connections().into_iter().next().expect("one");
        (result, conns, seen)
    }

    /// (a) Refused by the anchor: a certificate that does not hang off the
    /// embedded BBL CA, made by whoever answered on the port.
    #[test]
    fn a_leaf_from_another_authority_never_gets_the_access_code() {
        let impostor = recording_server(
            Arc::new(server(&[EVIL_LEAF_V1, EVIL_CA], OTHER_KEY, TLS12, None)),
            AfterHandshake::Drain);
        let (result, conns, seen) = camera_session(&impostor, TEST_SERIAL);

        assert!(result.is_err(), "the session ended");
        // the claim itself first, so a leak breaks this line and not another
        assert_eq!(seen.application_data(), 0,
                   "the auth packet never reached it: {:?}", seen.records);
        // then why it was not written: the record saying refused is what
        // tells that apart from a socket that simply died
        assert_eq!(conns.failure_since(0), Some(TlsFailure::Refused(
            Refusal::Cert(PrinterCertError::NotAnchored))));
    }

    /// (b) Refused by the CN: a GENUINE printer leaf — issued by the
    /// anchor, signed by the key it names — belonging to a different
    /// printer than the one configured. The attacker here holds a real
    /// certificate from another Bambu machine, so only the serial binding
    /// stands between it and the access code.
    #[test]
    fn a_genuine_leaf_for_another_serial_never_gets_the_access_code() {
        let other_printer = recording_server(
            Arc::new(server(&[LEAF_V1, TEST_CA], LEAF_KEY, TLS12, None)),
            AfterHandshake::Drain);
        let (result, conns, seen) =
            camera_session(&other_printer, OTHER_SERIAL);

        assert!(result.is_err(), "the session ended");
        assert_eq!(seen.application_data(), 0,
                   "the auth packet never reached it: {:?}", seen.records);
        assert_eq!(conns.failure_since(0), Some(TlsFailure::Refused(
            Refusal::Cert(PrinterCertError::SerialMismatch))));
    }

    /// The control that keeps the two above honest: with the genuine leaf
    /// and the serial it carries, the auth packet *is* written, and the
    /// same counting code sees exactly one application_data record. Without
    /// this, "zero records of type 23" would pass just as well against a
    /// tap that recorded nothing.
    ///
    /// The length is asserted on the plaintext, not on the record: a TLS
    /// 1.2 AES-GCM record carries an 8-byte explicit nonce and a 16-byte
    /// tag, so the record on the wire is 104 bytes for this 80-byte packet.
    #[test]
    fn the_auth_packet_is_written_once_the_certificate_is_accepted() {
        let expected = auth_packet(ACCESS_CODE);
        let printer = recording_server(
            Arc::new(server(&[LEAF_V1, TEST_CA], LEAF_KEY, TLS12, None)),
            AfterHandshake::ReadThenClose(expected.len()));
        let (_, conns, seen) = camera_session(&printer, TEST_SERIAL);

        // the guard rail: the counting above means what it says only while
        // this is TLS 1.2, so the version is asserted rather than assumed
        assert!(matches!(conns.records().first().map(|c| c.outcome()),
                         Some(Some(ConnOutcome::Established { version, .. }))
                             if *version == rustls::ProtocolVersion::TLSv1_2),
                "TLS 1.2, where a record's type is in the clear");
        assert_eq!(seen.application_data(), 1, "{:?}", seen.records);
        assert_eq!(seen.plaintext, expected,
                   "and that one record is the 80-byte auth packet");
    }
}
