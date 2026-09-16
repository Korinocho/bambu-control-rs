//! Chamber camera client (P1/A1 protocol): TLS socket on :6000,
//! 64-byte auth packet, then length-prefixed JPEG frames. Frames are
//! decoded off-thread into egui ColorImages.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Camera {
    /// Latest decoded frame; the GUI takes it and uploads a texture.
    pub frame: Arc<Mutex<Option<egui::ColorImage>>>,
    pub status: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    raw: Arc<Mutex<Option<TcpStream>>>,
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
    pub fn start(ip: String, access_code: String,
                 egui_ctx: egui::Context) -> Arc<Self> {
        let me = Arc::new(Self {
            frame: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new("connecting camera…".into())),
            stop: Arc::new(AtomicBool::new(false)),
            raw: Arc::new(Mutex::new(None)),
        });
        let handle = me.clone();
        std::thread::spawn(move || handle.run(&ip, &access_code, &egui_ctx));
        me
    }

    fn run(&self, ip: &str, access_code: &str, ctx: &egui::Context) {
        // Accepts any certificate, so the access code goes to whoever
        // answers on 6000. GitHub issue #2 moves the camera to the printer
        // certificate verifier in src/tls.rs, which FTPS already uses.
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .expect("tls connector");
        while !self.stop.load(Ordering::Relaxed) {
            match self.session(ip, access_code, &connector, ctx) {
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

    fn session(&self, ip: &str, access_code: &str,
               connector: &native_tls::TlsConnector,
               ctx: &egui::Context) -> anyhow::Result<()> {
        let raw = TcpStream::connect_timeout(
            &format!("{ip}:6000").parse()?, Duration::from_secs(10))?;
        raw.set_read_timeout(Some(Duration::from_secs(15)))?;
        *self.raw.lock().unwrap() = Some(raw.try_clone()?);
        let mut tls = connector.connect(ip, raw)?;
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

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(sock) = self.raw.lock().unwrap().take() {
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
    }
}
