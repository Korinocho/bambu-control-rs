//! MQTT client for one Bambu printer in LAN Developer Mode.
//!
//! rumqttc over TLS :8883 (user `bblp`, self-signed cert accepted).
//! Delta "print" reports are merged into a shared state map; the GUI
//! reads it under a mutex and gets a repaint request on every update.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{Client, Event, MqttOptions, Packet, QoS, Transport};
use serde_json::{Map, Value, json};

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
    let mut options = MqttOptions::new(
        format!("bambu-control-probe-{}", std::process::id()), ip, 8883);
    options.set_credentials("bblp", access_code);
    options.set_keep_alive(Duration::from_secs(30));
    // the same certificate handling as the rest of MQTT until issue #1
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .expect("tls connector");
    options.set_transport(Transport::tls_with_config(
        rumqttc::TlsConfiguration::NativeConnector(connector)));

    type Seen = (Option<String>, usize, bool,
                 std::collections::BTreeMap<String, (String, String)>);
    let (client, mut connection) = Client::new(options, 32);
    let seen: Arc<Mutex<Seen>> =
        Arc::new(Mutex::new((None, 0, false, Default::default())));
    let slot = seen.clone();
    let topic = format!("device/{serial}/report");
    let subscriber = client.clone();
    let reader = std::thread::spawn(move || {
        for notification in connection.iter() {
            match notification {
                // SUBSCRIBE is not a publish: nothing is asked of the
                // printer, and no command is ever sent
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    slot.lock().unwrap().2 = true;
                    let _ = subscriber.subscribe(topic.clone(),
                                                 QoS::AtMostOnce);
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    let Ok(data) =
                        serde_json::from_slice::<Value>(&p.payload)
                    else { continue };
                    let Some(print) = data.get("print") else { continue };
                    let mut slot = slot.lock().unwrap();
                    slot.1 += 1;
                    for field in WATCHED {
                        let Some(value) = print.get(field) else { continue };
                        let text = match value {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        slot.3.entry(field.to_string())
                            .and_modify(|(_, last)| last.clone_from(&text))
                            .or_insert_with(|| (text.clone(), text));
                    }
                    if let Some(state) = print.get("gcode_state")
                        .and_then(|state| state.as_str())
                    {
                        slot.0 = Some(state.to_string());
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if seen.lock().unwrap().0.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = client.disconnect();
    // the reader is deliberately not joined: rumqttc's iterator can stay
    // in a blocking read after a disconnect, and this probe must return at
    // its deadline rather than hold the caller for ever. The thread ends
    // with the test process.
    drop(reader);
    let (gcode_state, reports, connected, watched) =
        seen.lock().unwrap().clone();
    let watched = watched.into_iter()
        .map(|(field, (first, last))| (field, first, last))
        .collect();
    StateProbe { gcode_state, reports, connected, watched }
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
    client: Client,
    serial: String,
    seq: AtomicU64,
    stop: Arc<AtomicBool>,
}

impl PrinterClient {
    pub fn start(ip: &str, serial: &str, access_code: &str,
                 egui_ctx: egui::Context) -> Arc<Self> {
        let mut options = MqttOptions::new(
            format!("bambu-control-{serial}"), ip, 8883);
        options.set_credentials("bblp", access_code);
        options.set_keep_alive(Duration::from_secs(30));
        // Accepts any certificate, so the access code goes to whoever
        // answers on 8883. GitHub issue #1 moves MQTT to the printer
        // certificate verifier in src/tls.rs, which FTPS already uses.
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .expect("tls connector");
        options.set_transport(Transport::tls_with_config(
            rumqttc::TlsConfiguration::NativeConnector(connector)));

        let (client, mut connection) = Client::new(options, 32);
        let me = Arc::new(Self {
            state: Arc::new(Mutex::new(Map::new())),
            device_info: Arc::new(Mutex::new(Value::Null)),
            conn: Arc::new(Mutex::new((false, "connecting…".into()))),
            client,
            serial: serial.to_string(),
            seq: AtomicU64::new(0),
            stop: Arc::new(AtomicBool::new(false)),
        });

        let handle = me.clone();
        std::thread::spawn(move || {
            for notification in connection.iter() {
                if handle.stop.load(Ordering::Relaxed) {
                    break;
                }
                match notification {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        *handle.conn.lock().unwrap() = (true, "online".into());
                        let _ = handle.client.subscribe(
                            format!("device/{}/report", handle.serial),
                            QoS::AtMostOnce);
                        handle.push_all();
                        handle.publish(json!({"info": {
                            "sequence_id": "0", "command": "get_version"}}));
                        egui_ctx.request_repaint();
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        if handle.ingest(&p.payload) {
                            egui_ctx.request_repaint();
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        *handle.conn.lock().unwrap() =
                            (false, format!("offline ({e})"));
                        egui_ctx.request_repaint();
                        // connection.iter() retries; pace the loop
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        });
        me
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

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.client.disconnect();
    }

    // --- plumbing ---------------------------------------------------------
    fn publish(&self, payload: Value) {
        let _ = self.client.try_publish(
            format!("device/{}/request", self.serial),
            QoS::AtMostOnce, false, payload.to_string().into_bytes());
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
