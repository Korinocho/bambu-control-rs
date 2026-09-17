//! MQTT client for one Bambu printer in LAN Developer Mode.
//!
//! Port 8883 over the app's own anchored TLS (issue #1, design doc 5.3):
//! the printer's certificate must be issued by the embedded `BBL CA`, its
//! CN must equal the configured serial, and the handshake must be signed
//! with that leaf's key. CONNECT carries the access code and is the first
//! thing this protocol sends, so a refused handshake leaves it unsent.
//!
//! The connection loop is the app's own. rumqttc is used only as a packet
//! codec (`Packet::read` / `Packet::write`), with no TLS feature, so it
//! brings no TLS stack of its own. Delta "print" reports are merged into a
//! shared state map; the GUI reads it under a mutex and gets a repaint
//! request on every update.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use rumqttc::{Connect, ConnectReturnCode, Login, Packet, QoS, Subscribe,
              SubscribeReasonCode};
use serde_json::{Map, Value, json};

use crate::tls::{AnchoredConnector, AnchoredStream, PrinterTls, SessionConns,
                 TlsFailure};

/// Largest MQTT packet this client will frame. A printer's `pushall`
/// snapshot is tens of kilobytes; this sits far above that and is still
/// bounded, so a peer cannot make the app buffer without limit.
const MQTT_MAX: usize = 1024 * 1024;
const MQTT_PORT: u16 = 8883;
/// TCP connect only; every byte after it is bounded by `Timing::io_timeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The packet identifier this client puts on its one SUBSCRIBE. Any
/// non-zero value is legal; 0 is not, and that is the whole point.
const SUBSCRIBE_PKID: u16 = 1;

/// The one place this client's timing comes from. `ping_every` and
/// `io_timeout` are derived here and nowhere else, so moving the keep-alive
/// moves both and they cannot drift apart: an independent timeout would
/// start killing healthy connections the day someone raised the keep-alive,
/// with nothing in the code relating the two (design doc 5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timing {
    /// what CONNECT negotiates; the wire carries whole seconds
    keep_alive: Duration,
    /// PINGREQ at half the keep-alive, so one lost ping still leaves a
    /// whole interval before the broker may drop the connection
    ping_every: Duration,
    /// a read may legitimately wait a ping interval plus the ping's own
    /// round trip, so the socket budget is one and a half keep-alives.
    /// Erring short costs a reconnection, never security: resumption is
    /// disabled, so every reconnect re-runs the whole verification.
    io_timeout: Duration,
}

impl Timing {
    const fn from_keep_alive(millis: u64) -> Self {
        Self {
            keep_alive: Duration::from_millis(millis),
            ping_every: Duration::from_millis(millis / 2),
            io_timeout: Duration::from_millis(millis * 3 / 2),
        }
    }

    /// 30 s, as these printers are configured.
    const PRINTER: Timing = Timing::from_keep_alive(30_000);

    /// Whole seconds, for CONNECT. A keep-alive under one second goes on
    /// the wire as 0, which MQTT reads as "no keep-alive"; only the tests
    /// use such a value, and what they assert is this client's own pinging,
    /// never the broker's enforcement of it.
    fn keep_alive_secs(self) -> u16 {
        self.keep_alive.as_secs().min(u16::MAX as u64) as u16
    }
}

/// Why a connection ended.
///
/// Exactly one variant means the client actually observed the printer.
/// Every other one is a refusal, and a caller must treat a variant it does
/// not recognise as a refusal too: the outcome is what denies, never what
/// permits (design doc 5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// CONNACK accepted and the SUBACK granted the subscription. Only from
    /// here on is silence evidence about the printer rather than about us.
    ///
    /// Built only by the `#[cfg(test)]` probe: a production connection that
    /// reaches this state never returns, so `run` yields only failures.
    /// Worth keeping in mind that if this were unconstructed *everywhere*,
    /// `idle_from` could never authorise a live run, and a gate that always
    /// refuses looks exactly like a gate that is merely careful.
    #[cfg_attr(not(test), allow(dead_code,
        reason = "constructed by the idle probe, which is test-only"))]
    Subscribed,
    /// no serial configured, so there is no CN to bind a certificate to.
    /// The check is not skipped, it is impossible to define, so no socket
    /// is opened at all.
    NoSerial,
    /// the certificate was refused, or TLS failed
    Tls(String),
    /// the broker refused the CONNECT
    ConnectRefused(String),
    /// no SUBACK arrived before the deadline
    NoSubAck,
    /// a SUBACK arrived for a packet identifier this client never sent
    SubAckPkidMismatch,
    /// the SUBACK acknowledged the SUBSCRIBE and granted nothing. The
    /// nastiest of these: something came back, so it reads as success to
    /// anything that only checks whether a SUBACK arrived.
    SubscriptionRefused,
    /// the socket or the protocol failed
    Failed(String),
}

/// The TLS outcome of a session, as its own connection records tell it.
fn refusal(conns: &SessionConns) -> ProbeOutcome {
    match conns.failure_since(0) {
        Some(TlsFailure::Refused(refusal)) =>
            ProbeOutcome::Tls(format!("{refusal:?}")),
        Some(other) => ProbeOutcome::Tls(format!("{other:?}")),
        None => ProbeOutcome::Tls("handshake failed".into()),
    }
}

/// One MQTT connection over the anchored stream: the loop both callers
/// drive. Production and the `#[cfg(test)]` idle probe share this code, so
/// a difference between them is a difference in arguments and never in
/// behaviour -- and the mutations aim here, not at either caller.
struct Session {
    tls: AnchoredStream,
    conns: Arc<SessionConns>,
    buf: BytesMut,
    timing: Timing,
    next_ping: Instant,
}

impl Session {
    /// Opens the anchored connection and completes CONNECT/CONNACK.
    ///
    /// An empty serial fails before any socket: there is no CN to bind the
    /// certificate to, so the check is not merely skipped but impossible to
    /// state, and connecting anyway would be the hole issue #1 closes.
    /// `port` is `MQTT_PORT` at both call sites; the tests point it at an
    /// in-process broker. It is a parameter of this private function and
    /// nothing more -- no configuration reaches it. A test cannot assert
    /// that the callers pass the right port, because reaching a broker
    /// means passing a different one by construction, so
    /// `production_mqtt_sites_pass_the_port_constant` asserts it from the
    /// source instead. Without that, this parameter is the one way the app
    /// could talk to the wrong port with every test green.
    fn open(tls: &Arc<PrinterTls>, ip: &str, port: u16, access_code: &str,
            client_id: String, timing: Timing, conns: Arc<SessionConns>)
            -> Result<Self, ProbeOutcome> {
        let config = tls.config_for_new_session()
            .map_err(|e| ProbeOutcome::Tls(e.to_string()))?;
        let addr = format!("{ip}:{port}").parse()
            .map_err(|_| ProbeOutcome::Failed("not an IP address".into()))?;
        let raw = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| ProbeOutcome::Failed(e.to_string()))?;
        let connector =
            AnchoredConnector::new(config, conns.clone(), timing.io_timeout);
        // connect_stream runs and checks the handshake before it returns,
        // so the CONNECT below -- which carries the access code -- is
        // written only to a printer whose certificate was accepted
        let mut stream = connector.connect_stream(ip, raw)
            .map_err(|_| refusal(&conns))?;
        // each read waits at most one ping interval, so a quiet connection
        // wakes to ping instead of sitting out the whole socket budget
        stream.set_command_budget(Some(timing.ping_every));
        let mut me = Session {
            tls: stream,
            conns,
            buf: BytesMut::new(),
            timing,
            next_ping: Instant::now() + timing.ping_every,
        };
        let mut connect = Connect::new(client_id);
        connect.keep_alive = timing.keep_alive_secs();
        connect.login = Some(Login::new("bblp", access_code));
        me.send(&Packet::Connect(connect))?;
        let deadline = Instant::now() + timing.io_timeout;
        match me.attempt(deadline,
                         ProbeOutcome::Failed("no CONNACK".into()))? {
            Packet::ConnAck(ack) if ack.code == ConnectReturnCode::Success =>
                Ok(me),
            Packet::ConnAck(ack) =>
                Err(ProbeOutcome::ConnectRefused(format!("{:?}", ack.code))),
            other =>
                Err(ProbeOutcome::Failed(format!("{other:?} before CONNACK"))),
        }
    }

    /// SUBSCRIBE, and require the acknowledgement.
    ///
    /// `Subscribe::new` leaves `pkid: 0` because rumqttc's own state
    /// machine assigns it downstream; this loop must assign it. A packet
    /// identifier of 0 is illegal for SUBSCRIBE in MQTT 3.1.1 and a broker
    /// may discard such a packet in silence. Assigning one is necessary and
    /// not sufficient: the SUBACK must carry that same identifier *and*
    /// grant the subscription, since a SUBACK that acknowledges and grants
    /// nothing looks more like success than no SUBACK at all.
    fn subscribe(&mut self, topic: &str) -> Result<(), ProbeOutcome> {
        let mut subscribe = Subscribe::new(topic, QoS::AtMostOnce);
        subscribe.pkid = SUBSCRIBE_PKID;
        self.send(&Packet::Subscribe(subscribe))?;
        let deadline = Instant::now() + self.timing.io_timeout;
        loop {
            // this loop is where the retrying lives: `attempt` is one shot,
            // and comes back here until the deadline passes
            match self.attempt(deadline, ProbeOutcome::NoSubAck)? {
                Packet::SubAck(ack) => {
                    if ack.pkid != SUBSCRIBE_PKID {
                        return Err(ProbeOutcome::SubAckPkidMismatch);
                    }
                    let granted = !ack.return_codes.is_empty()
                        && ack.return_codes.iter().all(|code|
                            matches!(code, SubscribeReasonCode::Success(_)));
                    return if granted {
                        Ok(())
                    } else {
                        Err(ProbeOutcome::SubscriptionRefused)
                    };
                }
                Packet::PingResp => {}
                other => return Err(ProbeOutcome::Failed(
                    format!("{other:?} before SUBACK"))),
            }
        }
    }

    /// The next packet, or `None` when the session was cancelled. Sends a
    /// PINGREQ whenever one is due, which is what keeps an idle connection
    /// alive across keep-alive intervals.
    fn next(&mut self) -> Result<Option<Packet>, ProbeOutcome> {
        loop {
            match Packet::read(&mut self.buf, MQTT_MAX) {
                Ok(packet) => return Ok(Some(packet)),
                Err(rumqttc::Error::InsufficientBytes(_)) => {}
                Err(e) => return Err(ProbeOutcome::Failed(e.to_string())),
            }
            if self.conns.is_cancelled() {
                return Ok(None);
            }
            if Instant::now() >= self.next_ping {
                self.send(&Packet::PingReq)?;
                self.next_ping = Instant::now() + self.timing.ping_every;
            }
            let mut chunk = [0u8; 4096];
            match self.tls.read(&mut chunk) {
                Ok(0) => return Err(ProbeOutcome::Failed(
                    "the printer closed the connection".into())),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                // the command budget expired: nothing arrived within a ping
                // interval, which is the normal state of an idle connection
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) if self.conns.is_cancelled() => return Ok(None),
                Err(e) => return Err(ProbeOutcome::Failed(e.to_string())),
            }
        }
    }

    /// The next packet before `deadline`, or `missing` if none arrives.
    ///
    /// The deadline is enforced inside the wait, not only between packets:
    /// `next` returns on a ping interval as well as on a packet, so a broker
    /// that answers pings for ever while withholding a SUBACK would
    /// otherwise be bounded only by how the ping interval happens to divide
    /// the socket budget. That is a coincidence of the derivation, not a
    /// guarantee, and `a_withheld_suback_is_an_error_never_idle` is the test
    /// that refuses to rely on it.
    /// One bounded attempt at the next packet: `missing` once `deadline`
    /// has passed, otherwise whatever `next` gives.
    ///
    /// This does not retry, and must not pretend to. `next` already returns
    /// on a ping interval as well as on a packet, and the caller's own loop
    /// is what comes back here until the deadline -- `subscribe` is the one
    /// that keeps asking. Two earlier versions wrapped this body in `loop`
    /// and then `while`, both of which clippy flagged as never looping,
    /// because every arm returns. The bound is real: stretching
    /// `subscribe`'s deadline twentyfold makes
    /// `a_withheld_suback_is_an_error_never_subscribed` fail at 12 s
    /// instead of passing at 600 ms. It just does not live here.
    fn attempt(&mut self, deadline: Instant, missing: ProbeOutcome)
               -> Result<Packet, ProbeOutcome> {
        if Instant::now() >= deadline {
            return Err(missing);
        }
        match self.next()? {
            Some(packet) => Ok(packet),
            // a cancel during a handshake step is a stop, not a failure
            None => Err(ProbeOutcome::Failed("cancelled".into())),
        }
    }

    fn send(&mut self, packet: &Packet) -> Result<(), ProbeOutcome> {
        let mut out = BytesMut::new();
        packet.write(&mut out, MQTT_MAX)
            .map_err(|e| ProbeOutcome::Failed(e.to_string()))?;
        self.tls.write_all(&out)
            .and_then(|()| self.tls.flush())
            .map_err(|e| ProbeOutcome::Failed(e.to_string()))
    }
}

pub const SPEED_LEVELS: &[(i64, &str)] = &[
    (1, "Silent"),
    (2, "Standard"),
    (3, "Sport"),
    (4, "Ludicrous"),
];

/// Telemetry the read-only probe follows to tell a printing machine from an
/// idle one. A running print advances at least one of these within a
/// couple of minutes. Deliberately no `subtask_name` or `gcode_file`: those
/// are file names, which the live checks never print.
#[cfg(test)]
const WATCHED: [&str; 12] = [
    "gcode_state", "mc_print_stage", "mc_percent", "mc_remaining_time",
    "layer_num", "total_layer_num", "nozzle_temper", "nozzle_target_temper",
    "bed_temper", "bed_target_temper", "print_type", "print_error",
];

/// What a read-only state probe saw.
#[cfg(test)]
pub struct StateProbe {
    /// the printer's `gcode_state`, when it reported one. Bambu pushes
    /// deltas, so this field only appears when it changes: an idle printer
    /// never sends it, and its absence is not evidence of anything.
    pub gcode_state: Option<String>,
    /// `print` reports received
    pub reports: usize,
    /// the broker accepted the connection: tells a silent printer apart
    /// from one that was never reached
    pub connected: bool,
    /// (field, first value seen, last value seen) for `WATCHED`
    pub watched: Vec<(String, String, String)>,
    /// how the connection ended. `Subscribed` is the only value that lets a
    /// caller read anything into the fields above: without a granted
    /// subscription, "no reports" says nothing about the printer, only
    /// about us.
    pub outcome: ProbeOutcome,
}

/// Reads one printer's `gcode_state` without ever publishing, for the live
/// checks of the file browser (design doc 5.4). `PrinterClient::start`
/// sends `pushall` and `get_version` as soon as it connects, and the live
/// rules of the transfer-lane stage allow no MQTT publish at all, so this
/// probe only subscribes and waits for the printer's own report.
///
/// A `gcode_state` of `None` means the printer reported none within
/// `wait`, which the caller must treat as "state unknown", never as "not
/// printing": an idle printer sends deltas only when something changes, so
/// silence is the expected case exactly when nothing is happening.
#[cfg(test)]
pub fn subscribe_gcode_state(ip: &str, serial: &str, access_code: &str,
                             wait: Duration) -> StateProbe {
    let tls = match PrinterTls::new(serial) {
        Ok(tls) => tls,
        Err(_) => return StateProbe {
            gcode_state: None, reports: 0, connected: false,
            watched: Vec::new(), outcome: ProbeOutcome::NoSerial },
    };
    subscribe_gcode_state_at(&tls, ip, MQTT_PORT, serial, access_code, wait)
}

/// `subscribe_gcode_state` with the anchor and the port taken as arguments,
/// so the parity test can reach an in-process broker under the test CA.
///
/// Both seams are invisible to that test by construction -- it must pass a
/// different port and a different anchor to run at all -- so what the entry
/// point above passes is asserted from the source by
/// `production_mqtt_sites_pass_the_port_constant`, which pins this exact
/// delegation. That rule is the only thing standing between a wrong port or
/// a wrong trust anchor and a fully green suite.
#[cfg(test)]
pub fn subscribe_gcode_state_at(tls: &Arc<PrinterTls>, ip: &str, port: u16,
                                serial: &str, access_code: &str,
                                wait: Duration) -> StateProbe {
    let conns = SessionConns::new();
    let client_id = format!("bambu-control-probe-{}", std::process::id());
    let refused = |outcome| StateProbe {
        gcode_state: None, reports: 0, connected: false,
        watched: Vec::new(), outcome,
    };
    // the probe runs production's loop with production's timing, so a
    // difference between the two callers is a difference in arguments and
    // cannot hide here
    let mut session = match Session::open(tls, ip, port, access_code,
                                          client_id, Timing::PRINTER,
                                          conns.clone()) {
        Ok(session) => session,
        Err(outcome) => return refused(outcome),
    };
    // SUBSCRIBE is not a publish: nothing is asked of the printer, and no
    // command is ever sent
    if let Err(outcome) = session.subscribe(&format!("device/{serial}/report"))
    {
        return StateProbe { connected: true, ..refused(outcome) };
    }

    let mut gcode_state = None;
    let mut reports = 0usize;
    let mut watched: std::collections::BTreeMap<String, (String, String)> =
        Default::default();
    let mut outcome = ProbeOutcome::Subscribed;
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline && gcode_state.is_none() {
        match session.next() {
            Ok(Some(Packet::Publish(publish))) => {
                let Ok(data) =
                    serde_json::from_slice::<Value>(&publish.payload)
                else { continue };
                let Some(print) = data.get("print") else { continue };
                reports += 1;
                for field in WATCHED {
                    let Some(value) = print.get(field) else { continue };
                    let text = match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    watched.entry(field.to_string())
                        .and_modify(|(_, last)| last.clone_from(&text))
                        .or_insert_with(|| (text.clone(), text));
                }
                if let Some(state) = print.get("gcode_state")
                    .and_then(|state| state.as_str())
                {
                    gcode_state = Some(state.to_string());
                }
            }
            Ok(Some(_)) => {}
            // cancelled: the subscription was granted, so what was seen
            // before it still counts as evidence
            Ok(None) => break,
            Err(why) => {
                outcome = why;
                break;
            }
        }
    }
    // the reader is this thread now, and a cancel is seen within
    // CANCEL_POLL, so there is nothing left to leak: the old probe could
    // not join its reader because rumqttc's iterator stayed in a blocking
    // read after a disconnect
    conns.cancel();
    let watched = watched.into_iter()
        .map(|(field, (first, last))| (field, first, last))
        .collect();
    StateProbe { gcode_state, reports, connected: true, watched, outcome }
}

pub fn speed_name(level: i64) -> &'static str {
    SPEED_LEVELS
        .iter()
        .find(|(l, _)| *l == level)
        .map(|(_, n)| *n)
        .unwrap_or("Standard")
}

pub struct PrinterClient {
    pub state: Arc<Mutex<Map<String, Value>>>,
    pub device_info: Arc<Mutex<Value>>,
    /// (connected, detail)
    pub conn: Arc<Mutex<(bool, String)>>,
    /// Commands for the connection thread, which owns the socket. The UI
    /// calls from its own thread and never touches the stream.
    commands: Sender<Vec<u8>>,
    /// the running connection's records: cancelling ends a waiting read
    /// within CANCEL_POLL, which a socket shutdown from another thread does
    /// not do on Windows (design doc 5.2)
    conns: Arc<Mutex<Option<Arc<SessionConns>>>>,
    serial: String,
    seq: AtomicU64,
    stop: Arc<AtomicBool>,
}

impl PrinterClient {
    pub fn start(ip: &str, serial: &str, access_code: &str,
                 egui_ctx: egui::Context) -> Arc<Self> {
        let (commands, inbox) = crossbeam_channel::unbounded();
        let me = Arc::new(Self {
            state: Arc::new(Mutex::new(Map::new())),
            device_info: Arc::new(Mutex::new(Value::Null)),
            conn: Arc::new(Mutex::new((false, "connecting…".into()))),
            commands,
            conns: Arc::new(Mutex::new(None)),
            serial: serial.to_string(),
            seq: AtomicU64::new(0),
            stop: Arc::new(AtomicBool::new(false)),
        });
        // No serial means no CN to bind the certificate to. The check is not
        // skipped here, it cannot even be stated: there is nothing to
        // compare the leaf's subject against. Connecting anyway would hand
        // the access code to whatever answered, which is precisely the hole
        // issue #1 closes, so this fails closed and opens no socket at all.
        if serial.is_empty() {
            *me.conn.lock().unwrap() = (false, "no serial configured".into());
            egui_ctx.request_repaint();
            return me;
        }

        let handle = me.clone();
        let (ip, serial, access_code) =
            (ip.to_string(), serial.to_string(), access_code.to_string());
        std::thread::spawn(move || {
            while !handle.stop.load(Ordering::Relaxed) {
                let why = handle.run(&ip, &serial, &access_code, &inbox,
                                     &egui_ctx);
                if handle.stop.load(Ordering::Relaxed) {
                    break;
                }
                *handle.conn.lock().unwrap() =
                    (false, format!("offline ({why:?})"));
                egui_ctx.request_repaint();
                // every reconnect runs the whole verification again, since
                // resumption is disabled for this port (design doc 5.3)
                for _ in 0..10 {
                    if handle.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
        me
    }

    /// One connection, from the handshake to whatever ends it.
    fn run(&self, ip: &str, serial: &str, access_code: &str,
           inbox: &Receiver<Vec<u8>>, egui_ctx: &egui::Context)
           -> ProbeOutcome {
        let conns = SessionConns::new();
        *self.conns.lock().unwrap() = Some(conns.clone());
        let client_id = format!("bambu-control-{serial}");
        let tls = match PrinterTls::new(serial) {
            Ok(tls) => tls,
            Err(_) => return ProbeOutcome::NoSerial,
        };
        let mut session = match Session::open(&tls, ip, MQTT_PORT,
                                              access_code, client_id,
                                              Timing::PRINTER,
                                              conns.clone()) {
            Ok(session) => session,
            Err(outcome) => return outcome,
        };
        if let Err(outcome) =
            session.subscribe(&format!("device/{serial}/report"))
        {
            return outcome;
        }
        *self.conn.lock().unwrap() = (true, "online".into());
        self.push_all();
        self.publish(json!({"info": {
            "sequence_id": "0", "command": "get_version"}}));
        egui_ctx.request_repaint();
        loop {
            // commands the UI queued while this thread was reading
            loop {
                match inbox.try_recv() {
                    Ok(payload) => {
                        let topic = format!("device/{}/request", self.serial);
                        let packet = Packet::Publish(rumqttc::Publish::new(
                            topic, QoS::AtMostOnce, payload));
                        if let Err(outcome) = session.send(&packet) {
                            return outcome;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) =>
                        return ProbeOutcome::Failed("ui gone".into()),
                }
            }
            match session.next() {
                Ok(Some(Packet::Publish(publish))) => {
                    if self.ingest(&publish.payload) {
                        egui_ctx.request_repaint();
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => return ProbeOutcome::Failed("cancelled".into()),
                Err(outcome) => return outcome,
            }
        }
    }

    /// Merge one report payload; true when something changed.
    fn ingest(&self, payload: &[u8]) -> bool {
        let Ok(data) = serde_json::from_slice::<Value>(payload) else {
            return false;
        };
        let mut changed = false;
        if let Some(pr) = data.get("print").and_then(|v| v.as_object()) {
            let mut state = self.state.lock().unwrap();
            for (k, v) in pr {
                state.insert(k.clone(), v.clone());
            }
            changed = true;
        }
        if let Some(inf) = data.get("info")
            && inf.get("command").and_then(|c| c.as_str())
                == Some("get_version")
        {
            *self.device_info.lock().unwrap() = inf.clone();
            changed = true;
        }
        changed
    }

    /// Ends the client without waiting. Cancelling the session is what wakes
    /// a read blocked in the connection thread: on Windows a shutdown
    /// through a cloned handle returns Ok and leaves that read waiting out
    /// its full timeout (design doc 5.2).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(conns) = self.conns.lock().unwrap().take() {
            conns.cancel();
        }
    }

    // --- plumbing ---------------------------------------------------------
    /// Queues one command for the connection thread, which owns the socket.
    /// Dropping it when the thread is gone is deliberate: a command sent to
    /// a printer that is not connected is lost, exactly as it was before.
    fn publish(&self, payload: Value) {
        let _ = self.commands.send(payload.to_string().into_bytes());
    }

    fn print_cmd(&self, command: &str, extra: Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut body = json!({
            "sequence_id": seq.to_string(), "command": command});
        if let (Some(dst), Some(src)) =
            (body.as_object_mut(), extra.as_object())
        {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        self.publish(json!({"print": body}));
    }

    pub fn push_all(&self) {
        self.publish(json!({"pushing": {
            "sequence_id": "0", "command": "pushall"}}));
    }

    // --- commands ---------------------------------------------------------
    pub fn pause(&self) {
        self.print_cmd("pause", json!({"param": ""}));
    }

    pub fn resume(&self) {
        self.print_cmd("resume", json!({"param": ""}));
    }

    pub fn stop_print(&self) {
        self.print_cmd("stop", json!({"param": ""}));
    }

    pub fn set_speed(&self, level: i64) {
        self.print_cmd("print_speed", json!({"param": level.to_string()}));
    }

    pub fn gcode(&self, line: &str) {
        let mut line = line.to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        self.print_cmd("gcode_line", json!({"param": line}));
    }

    pub fn set_nozzle_temp(&self, temp: i64) {
        self.gcode(&format!("M104 S{temp}"));
    }

    pub fn set_bed_temp(&self, temp: i64) {
        self.gcode(&format!("M140 S{temp}"));
    }

    /// fan_index: 1 = part, 2 = aux, 3 = chamber.
    pub fn set_fan(&self, fan_index: i64, percent: i64) {
        let s = (percent * 255 / 100).clamp(0, 255);
        self.gcode(&format!("M106 P{fan_index} S{s}"));
    }

    pub fn set_light(&self, on: bool) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.publish(json!({"system": {
            "sequence_id": seq.to_string(),
            "command": "ledctrl",
            "led_node": "chamber_light",
            "led_mode": if on { "on" } else { "off" },
            "led_on_time": 500, "led_off_time": 500,
            "loop_times": 0, "interval_time": 0,
        }}));
    }

    pub fn skip_objects(&self, ids: &[i64]) {
        self.print_cmd("skip_objects", json!({"obj_list": ids}));
    }

    // --- screen-menu replacements (broken-display support) ----------------
    /// option bitmask: 1 = motor noise, 2 = bed leveling, 4 = vibration.
    pub fn calibrate(&self, option: i64) {
        self.print_cmd("calibration", json!({"option": option}));
    }

    /// slot: None = external spool (254), Some(1..=4) = AMS Lite slot.
    pub fn load_filament(&self, slot: Option<u8>, temp: i64) {
        let target = slot.map(|s| s as i64 - 1).unwrap_or(254);
        self.print_cmd("ams_change_filament", json!({
            "target": target, "curr_temp": temp, "tar_temp": temp}));
    }

    pub fn unload_filament(&self, temp: i64) {
        self.print_cmd("ams_change_filament", json!({
            "target": 255, "curr_temp": temp, "tar_temp": temp}));
    }

    /// nozzle_type: "stainless_steel" | "hardened_steel".
    /// Verified working over LAN (result:success, fw 01.08.01.00).
    pub fn set_nozzle(&self, nozzle_type: &str, diameter: f64) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.publish(json!({"system": {
            "sequence_id": seq.to_string(),
            "command": "set_accessories",
            "accessory_type": "nozzle",
            "nozzle_type": nozzle_type,
            "nozzle_diameter": diameter,
        }}));
    }

    // --- movement (idle only; guarded by caller) ---------------------------
    pub fn home(&self) {
        self.gcode("G28");
    }

    pub fn jog(&self, axis: &str, dist: f64, feed: i64) {
        self.gcode(&format!("G91\nG1 {axis}{dist} F{feed}\nG90"));
    }

    pub fn extrude(&self, mm: f64, feed: i64) {
        self.gcode(&format!("M83\nG1 E{mm} F{feed}"));
    }
}

/// The subscribe handshake, the keep-alive, and the parity of the two
/// callers (design doc 5.3).
///
/// Four of these are about one packet: a SUBSCRIBE whose acknowledgement is
/// missing, misaddressed, or refuses while looking like an acceptance. They
/// drive `Session` directly rather than the probe, because at
/// `Timing::PRINTER` a withheld SUBACK takes 45 s to fail, and because the
/// shared loop is what the mutations aim at. The parity test is the one that
/// goes through `subscribe_gcode_state`, so it is the only one that can
/// catch the two callers drifting apart.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::testkit::*;

    const ACCESS_CODE: &str = "12345678";
    /// Derived exactly as production's is, just smaller: ping at 200 ms,
    /// socket budget 600 ms. Its CONNECT carries keep_alive 0, since MQTT
    /// counts whole seconds -- so these tests assert this client's own
    /// pinging, never a broker's enforcement of it. Only the live P1S run
    /// can check that.
    const FAST: Timing = Timing::from_keep_alive(400);

    fn broker(subscribe: SubAckMode, reports: Vec<Vec<u8>>) -> MqttBroker {
        let mut spec = MqttSpec::new(
            ticket_server(&[LEAF_V1, TEST_CA], LEAF_KEY), reports);
        spec.subscribe = subscribe;
        mqtt_broker(spec)
    }

    /// A session on the shared loop, anchored on the test CA.
    fn session(peer: &MqttBroker, timing: Timing)
               -> Result<Session, ProbeOutcome> {
        let tls = test_tls(TEST_CA, TEST_SERIAL);
        Session::open(&tls, "127.0.0.1", peer.port, ACCESS_CODE,
                      "bambu-control-test".into(), timing,
                      SessionConns::new())
    }

    fn idle_report() -> Vec<u8> {
        br#"{"print":{"gcode_state":"IDLE","mc_percent":0}}"#.to_vec()
    }

    /// `Subscribe::new` leaves pkid 0 and rumqttc's own state machine would
    /// have filled it in; this loop must. A SUBSCRIBE with identifier 0 is
    /// illegal in MQTT 3.1.1 and a broker may drop it without a word.
    #[test]
    fn a_subscribe_carries_a_non_zero_packet_identifier() {
        let peer = broker(SubAckMode::Grant, vec![idle_report()]);
        let mut session = session(&peer, FAST).expect("connected");
        session.subscribe("device/test/report").expect("granted");
        let pkids = peer.subscribe_pkids();
        assert_eq!(pkids.len(), 1, "one SUBSCRIBE");
        assert_ne!(pkids[0], 0, "a packet identifier of 0 is illegal here");
    }

    /// No SUBACK at all: the subscription was never granted, so the loop
    /// must say so rather than fall through to waiting for reports that can
    /// never arrive.
    #[test]
    fn a_withheld_suback_is_an_error_never_subscribed() {
        let peer = broker(SubAckMode::Withhold, vec![]);
        let mut session = session(&peer, FAST).expect("connected");
        let started = std::time::Instant::now();
        assert_eq!(session.subscribe("device/test/report"),
                   Err(ProbeOutcome::NoSubAck));
        // bounded by the deadline inside the wait, not by how the ping
        // interval happens to divide the socket budget
        assert!(started.elapsed() < FAST.io_timeout * 3,
                "{:?}", started.elapsed());
    }

    /// The retry in `subscribe`'s loop is a separate property from the
    /// deadline that bounds it, and for a while only the deadline was
    /// tested: with `FAST`'s 600 ms budget against a 200 ms ping interval,
    /// `attempt` returned `NoSubAck` before any PINGRESP arrived, so the
    /// `PingResp => {}` arm was never once executed. A mutation that made a
    /// PINGRESP abort the subscribe left all six tests green.
    ///
    /// Here the broker answers pings and withholds the SUBACK, and the
    /// deadline is wide enough that several PINGRESPs must be consumed and
    /// looped past. If the loop stops retrying, this fails long before the
    /// deadline, with a PINGRESP as the cause.
    #[test]
    fn a_pingresp_while_waiting_for_a_suback_does_not_end_the_wait() {
        let peer = broker(SubAckMode::Withhold, vec![]);
        // ping every 200 ms, subscribe deadline 3 s: pings must be seen and
        // survived before NoSubAck is the answer
        let timing = Timing::from_keep_alive(2_000);
        let mut session = session(&peer, timing).expect("connected");
        let started = std::time::Instant::now();
        assert_eq!(session.subscribe("device/test/report"),
                   Err(ProbeOutcome::NoSubAck));
        let elapsed = started.elapsed();
        assert!(peer.pings() >= 2,
                "the wait must have outlasted pings to test the retry, saw {}",
                peer.pings());
        assert!(elapsed >= timing.ping_every * 2,
                "ended before a PINGRESP could be looped past: {elapsed:?}");
    }

    /// A SUBACK for an identifier this client never sent acknowledges
    /// somebody else's subscription, not ours.
    #[test]
    fn a_suback_for_another_packet_identifier_is_an_error() {
        let peer = broker(SubAckMode::WrongPkid, vec![]);
        let mut session = session(&peer, FAST).expect("connected");
        assert_eq!(session.subscribe("device/test/report"),
                   Err(ProbeOutcome::SubAckPkidMismatch));
    }

    /// The nastiest of the four: the identifier matches and the broker
    /// granted nothing. Something came back, so anything that only checks
    /// "did a SUBACK arrive" reads this as success.
    #[test]
    fn a_suback_that_grants_nothing_is_an_error() {
        let peer = broker(SubAckMode::Failure, vec![]);
        let mut session = session(&peer, FAST).expect("connected");
        assert_eq!(session.subscribe("device/test/report"),
                   Err(ProbeOutcome::SubscriptionRefused));
    }

    /// A healthy idle connection outlives more than one keep-alive interval.
    /// This is what stops anyone decoupling ping_every from keep_alive
    /// later: raise one without the other and this goes red.
    #[test]
    fn a_healthy_idle_connection_survives_more_than_one_keep_alive() {
        let peer = broker(SubAckMode::Grant, vec![]);
        let mut session = session(&peer, FAST).expect("connected");
        session.subscribe("device/test/report").expect("granted");
        let until = std::time::Instant::now() + FAST.keep_alive * 3;
        while std::time::Instant::now() < until {
            match session.next() {
                // nothing to read is the normal state of an idle connection
                Ok(Some(_)) | Ok(None) => {}
                Err(why) => panic!("a healthy idle connection dropped: {why:?}"),
            }
        }
        assert!(peer.pings() >= 2,
                "crossed {:?} of keep-alive on {} pings",
                FAST.keep_alive * 3, peer.pings());
    }

    /// The probe and production must reach the loop with the same
    /// parameters, or a difference between the callers hides. Asserted from
    /// what the broker saw on the wire, not by re-reading the constants:
    /// comparing Timing::PRINTER with itself would prove nothing.
    #[test]
    fn the_probe_reaches_the_loop_with_productions_parameters() {
        let peer = broker(SubAckMode::Grant, vec![idle_report()]);
        let probe = subscribe_gcode_state_at(
            &test_tls(TEST_CA, TEST_SERIAL), "127.0.0.1", peer.port,
            TEST_SERIAL, ACCESS_CODE, Duration::from_secs(2));
        assert_eq!(probe.outcome, ProbeOutcome::Subscribed);
        assert_eq!(probe.gcode_state.as_deref(), Some("IDLE"));
        // 30 written out, deliberately, not Timing::PRINTER.keep_alive_secs().
        // Reading the constant here compares it with itself: a mutation that
        // changes what the callers pass changes both sides of the assertion
        // and the test stays green. That exact mutation survived once, which
        // is how this line came to be a literal.
        assert_eq!(peer.keep_alives(), vec![30u16],
                   "the probe must negotiate production's 30 s keep-alive");
        let logins = peer.logins();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].1, "bblp", "production's username");
        assert_eq!(logins[0].2, ACCESS_CODE);
    }
}
