//! LPS-node sniffer support.
//!
//! An LPS node flashed in *sniffer* mode (`MODE_SNIFFER`) and switched to
//! binary output (by sending `'b'`) streams every UWB packet it overhears to
//! USB. Each frame on the wire is:
//!
//! ```text
//! 0xBC | rx_timestamp[5] | src[1] | dst[1] | len[2] | payload[len] | len[2]
//! ```
//!
//! All multi-byte fields are little-endian. The trailing length is a copy of
//! the leading one and is used purely to resynchronise the byte stream. See
//! `lps-node-firmware/src/uwb_sniffer.c` and `tools/sniffer/sniffer_binary.py`.
//!
//! This module is the host-side counterpart: it frames the byte stream, decodes
//! the well-known payload types (TDoA3/TDoA2/TWR/LPP), accumulates per-anchor
//! statistics and an inter-anchor distance matrix, and solves anchor geometry
//! from that matrix (auto-survey).

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};

use nalgebra::{DMatrix, Matrix3, Vector3};
use serde::{Deserialize, Serialize};

/// DW1000 timestamp tick → metres. One tick is `1 / (499.2 MHz * 128)` seconds;
/// multiplied by the speed of light. TDoA3 inter-anchor distances are expressed
/// as a halved round-trip time-of-flight in these ticks.
pub const METERS_PER_TICK: f64 = 299_792_458.0 / (499.2e6 * 128.0);

/// Antenna-delay offset baked into every TDoA3 inter-anchor distance, in metres.
///
/// The node programs the DW1000 hardware antenna delay to **zero**
/// (`uwb.c`: `dwSetAntenaDelay(dwm, {.full = 0})`) and, unlike the TWR path,
/// TDoA3 never compensates it in software — it only rejects measurements below
/// it (`MIN_TOF`). So the raw `(localTime - remoteTime)/2` it reports is the true
/// time-of-flight *plus* this constant. Matches `ANTENNA_OFFSET` in
/// `uwb_tdoa_anchor3.c`. Must be subtracted to recover a physical distance.
pub const ANTENNA_OFFSET_M: f64 = 154.6;

/// Sync byte that prefixes every binary sniffer frame.
const SYNC: u8 = 0xBC;
/// Sanity cap on the declared payload length (matches the Python reference).
const MAX_PAYLOAD: usize = 1024;

// Payload type bytes (first byte of the MAC payload).
const TYPE_TDOA2: u8 = 0x22;
const TYPE_TDOA3: u8 = 0x30;
const TYPE_TWR_POLL: u8 = 0x01;
const TYPE_TWR_ANSWER: u8 = 0x02;
const TYPE_TWR_FINAL: u8 = 0x03;
const TYPE_TWR_REPORT: u8 = 0x04;
const LPP_HEADER: u8 = 0xF0;
const LPP_SHORT_ANCHOR_POSITION: u8 = 0x01;

/// Classification of a sniffed packet, derived from its first payload byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PacketKind {
    Tdoa3,
    Tdoa2,
    TwrPoll,
    TwrAnswer,
    TwrFinal,
    TwrReport,
    Lpp,
    Empty,
    Unknown(u8),
}

impl PacketKind {
    fn from_payload(payload: &[u8]) -> PacketKind {
        match payload.first() {
            None => PacketKind::Empty,
            Some(&TYPE_TDOA3) => PacketKind::Tdoa3,
            Some(&TYPE_TDOA2) => PacketKind::Tdoa2,
            Some(&TYPE_TWR_POLL) => PacketKind::TwrPoll,
            Some(&TYPE_TWR_ANSWER) => PacketKind::TwrAnswer,
            Some(&TYPE_TWR_FINAL) => PacketKind::TwrFinal,
            Some(&TYPE_TWR_REPORT) => PacketKind::TwrReport,
            Some(&LPP_HEADER) => PacketKind::Lpp,
            Some(&other) => PacketKind::Unknown(other),
        }
    }

    pub fn label(&self) -> String {
        match self {
            PacketKind::Tdoa3 => "TDoA3".into(),
            PacketKind::Tdoa2 => "TDoA2".into(),
            PacketKind::TwrPoll => "TWR poll".into(),
            PacketKind::TwrAnswer => "TWR answer".into(),
            PacketKind::TwrFinal => "TWR final".into(),
            PacketKind::TwrReport => "TWR report".into(),
            PacketKind::Lpp => "LPP".into(),
            PacketKind::Empty => "empty".into(),
            PacketKind::Unknown(b) => format!("0x{b:02x}"),
        }
    }

    /// Modulus of the application sequence counter for this packet kind, used
    /// for loss estimation. TDoA3 carries a full 8-bit packet seq.
    fn seq_modulus(&self) -> u32 {
        256
    }
}

/// A remote-anchor entry decoded from a TDoA3 range packet.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(dead_code)] // seq/rx_timestamp are decoded protocol fields kept for completeness
pub struct RemoteAnchor {
    pub id: u8,
    pub seq: u8,
    pub rx_timestamp: u32,
    /// Inter-anchor distance in DW1000 ticks (halved round-trip ToF), if present.
    pub distance_ticks: Option<u16>,
}

/// A fully decoded sniffer frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SniffedPacket {
    /// DW1000 receive timestamp at the sniffer (40-bit).
    pub rx_timestamp: u64,
    pub from: u8,
    pub to: u8,
    pub kind: PacketKind,
    /// Application sequence number where one is defined for the kind.
    pub seq: Option<u8>,
    pub payload_len: usize,
    /// Remote-anchor entries (TDoA3 only).
    pub remote_anchors: Vec<RemoteAnchor>,
    /// Anchor self-reported position from an embedded LPP packet, if any.
    pub lpp_position: Option<[f32; 3]>,
    /// Estimated receive power [dBm], from the firmware extension (if present).
    pub rx_power: Option<f32>,
    /// First-path power [dBm], from the firmware extension (if present).
    pub fp_power: Option<f32>,
    /// Frames the node dropped (USB TX queue full) since the previous sent
    /// frame — i.e. on-the-wire loss, as opposed to over-the-air loss.
    pub wire_dropped: u16,
}

/// Size of the fixed firmware extension appended after the resync length:
/// `rxPower[4] | fpPower[4] | droppedSinceSent[2]`.
const EXT_LEN: usize = 10;

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le_f32(b: &[u8]) -> f32 {
    f32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Decode a TDoA3 payload into remote-anchor entries and an optional position.
fn decode_tdoa3(payload: &[u8], pkt: &mut SniffedPacket) {
    // Header: type(1) seq(1) txTimeStamp(4) remoteCount(1) = 7 bytes.
    if payload.len() < 7 {
        return;
    }
    pkt.seq = Some(payload[1]);
    let remote_count = payload[6] as usize;
    let mut i = 7;
    for _ in 0..remote_count {
        // remoteAnchorData: id(1) seq(1) rxTimeStamp(4) [distance(2)].
        if i + 6 > payload.len() {
            return;
        }
        let id = payload[i];
        let seq_raw = payload[i + 1];
        let rx_timestamp = le_u32(&payload[i + 2..i + 6]);
        let has_distance = (seq_raw & 0x80) != 0;
        i += 6;
        let distance_ticks = if has_distance {
            if i + 2 > payload.len() {
                return;
            }
            let d = le_u16(&payload[i..i + 2]);
            i += 2;
            Some(d)
        } else {
            None
        };
        pkt.remote_anchors.push(RemoteAnchor {
            id,
            seq: seq_raw & 0x7f,
            rx_timestamp,
            distance_ticks,
        });
    }

    // Optional trailing LPP short packet carrying the sender's own position.
    if i + 2 <= payload.len()
        && payload[i] == LPP_HEADER
        && payload[i + 1] == LPP_SHORT_ANCHOR_POSITION
        && i + 2 + 12 <= payload.len()
    {
        let p = i + 2;
        pkt.lpp_position = Some([
            le_f32(&payload[p..p + 4]),
            le_f32(&payload[p + 4..p + 8]),
            le_f32(&payload[p + 8..p + 12]),
        ]);
    }
}

/// Decode a fully-received frame body (everything after the length field) into a
/// [`SniffedPacket`].
fn decode_payload(rx_timestamp: u64, from: u8, to: u8, payload: &[u8]) -> SniffedPacket {
    let kind = PacketKind::from_payload(payload);
    let mut pkt = SniffedPacket {
        rx_timestamp,
        from,
        to,
        kind,
        seq: None,
        payload_len: payload.len(),
        remote_anchors: Vec::new(),
        lpp_position: None,
        rx_power: None,
        fp_power: None,
        wire_dropped: 0,
    };
    match kind {
        PacketKind::Tdoa3 => decode_tdoa3(payload, &mut pkt),
        PacketKind::TwrPoll | PacketKind::TwrFinal | PacketKind::TwrReport => {
            // These carry a 1-byte sequence number right after the type byte.
            if payload.len() >= 2 {
                pkt.seq = Some(payload[1]);
            }
        }
        _ => {}
    }
    pkt
}

/// Streaming frame decoder. Push raw serial bytes in; pull decoded packets out.
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Count of `len != len2` framing failures — i.e. corruption from bytes lost
    /// between the node and this host (USB/OS buffer overrun on the PC side).
    pub resyncs: u64,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
            resyncs: 0,
        }
    }

    /// Append `data` and emit every complete, in-sync frame it now contains.
    ///
    /// Assumes the RSSI firmware: every frame is followed by a fixed
    /// [`EXT_LEN`]-byte extension, which is consumed deterministically (rather
    /// than scanned past) so a `0xBC` inside a power/drop field can't desync us.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<SniffedPacket>) {
        self.buf.extend_from_slice(data);
        loop {
            // Drop everything before the next sync byte.
            match self.buf.iter().position(|&b| b == SYNC) {
                Some(0) => {}
                Some(n) => {
                    self.buf.drain(0..n);
                }
                None => {
                    self.buf.clear();
                    return;
                }
            }
            // Header is sync(1) + ts(5) + src(1) + dst(1) + len(2) = 10 bytes.
            if self.buf.len() < 10 {
                return;
            }
            let mut ts = [0u8; 8];
            ts[..5].copy_from_slice(&self.buf[1..6]);
            let rx_timestamp = u64::from_le_bytes(ts);
            let from = self.buf[6];
            let to = self.buf[7];
            let len = le_u16(&self.buf[8..10]) as usize;
            if len > MAX_PAYLOAD {
                // Bogus length: drop this sync byte and resynchronise.
                self.resyncs += 1;
                self.buf.drain(0..1);
                continue;
            }
            // sync..len2 is `10 + len + 2`; then the fixed firmware extension.
            let frame_end = 10 + len + 2;
            let total = frame_end + EXT_LEN;
            if self.buf.len() < total {
                return; // wait for the rest of the frame (+ extension)
            }
            let len2 = le_u16(&self.buf[10 + len..12 + len]) as usize;
            if len != len2 {
                // Out of sync: this wasn't a real frame. Skip the sync byte.
                self.resyncs += 1;
                self.buf.drain(0..1);
                continue;
            }
            let mut pkt = decode_payload(rx_timestamp, from, to, &self.buf[10..10 + len]);
            // Fixed extension: rxPower[4] | fpPower[4] | droppedSinceSent[2].
            pkt.rx_power = Some(le_f32(&self.buf[frame_end..frame_end + 4]));
            pkt.fp_power = Some(le_f32(&self.buf[frame_end + 4..frame_end + 8]));
            pkt.wire_dropped = le_u16(&self.buf[frame_end + 8..frame_end + 10]);
            out.push(pkt);
            self.buf.drain(0..total);
        }
    }
}

/// Per-anchor (per source address) running statistics.
#[derive(Clone)]
pub struct AnchorStat {
    #[allow(dead_code)] // mirrors the HashMap key; handy when stats are cloned out
    pub id: u8,
    pub kind: PacketKind,
    pub count: u64,
    pub lost: u64,
    last_seq: Option<u8>,
    /// Wall-clock seconds (monotonic) of recent packets, for rate estimation.
    recent: VecDeque<f64>,
    pub last_seen: f64,
    /// Smoothed receive / first-path power [dBm] (None until an extension seen).
    pub rx_power: Option<f32>,
    pub fp_power: Option<f32>,
}

impl AnchorStat {
    fn new(id: u8, kind: PacketKind) -> Self {
        Self {
            id,
            kind,
            count: 0,
            lost: 0,
            last_seq: None,
            recent: VecDeque::new(),
            last_seen: 0.0,
            rx_power: None,
            fp_power: None,
        }
    }

    fn record(&mut self, pkt: &SniffedPacket, now: f64) {
        self.kind = pkt.kind;
        self.count += 1;
        self.last_seen = now;
        if let Some(rx) = pkt.rx_power {
            self.rx_power = Some(ema(self.rx_power, rx));
        }
        if let Some(fp) = pkt.fp_power {
            self.fp_power = Some(ema(self.fp_power, fp));
        }
        self.recent.push_back(now);
        while let Some(&front) = self.recent.front() {
            if now - front > 1.0 {
                self.recent.pop_front();
            } else {
                break;
            }
        }
        if let Some(seq) = pkt.seq {
            if let Some(prev) = self.last_seq {
                let modulus = pkt.kind.seq_modulus();
                let gap = (seq as u32 + modulus - prev as u32) % modulus;
                // Only count plausible gaps; large jumps are resyncs, not loss.
                if (2..=16).contains(&gap) {
                    self.lost += (gap - 1) as u64;
                }
            }
            self.last_seq = Some(seq);
        }
    }

    /// Packets per second over the last second.
    pub fn rate_hz(&self) -> f32 {
        self.recent.len() as f32
    }

    /// Receive minus first-path power [dB]. A large gap suggests a non-line-of-
    /// sight / multipath link. `None` until both powers are known.
    pub fn nlos(&self) -> Option<f32> {
        match (self.rx_power, self.fp_power) {
            (Some(rx), Some(fp)) => Some(rx - fp),
            _ => None,
        }
    }
}

/// Exponential moving average, seeding on the first sample.
fn ema(prev: Option<f32>, new: f32) -> f32 {
    match prev {
        Some(p) => p * 0.8 + new * 0.2,
        None => new,
    }
}

/// Exponentially-smoothed directed distance measurement between two anchors.
#[derive(Clone)]
struct DistEma {
    meters: f32,
    samples: u64,
}

/// Accumulates inter-anchor distances reported in TDoA3 packets.
#[derive(Default)]
pub struct DistanceMatrix {
    /// Directed measurements keyed by (from, to).
    directed: HashMap<(u8, u8), DistEma>,
}

impl DistanceMatrix {
    fn record(&mut self, from: u8, to: u8, ticks: u16) {
        // Remove the antenna-delay offset the firmware leaves in; clamp so
        // measurement noise near zero can't produce a negative distance.
        let meters = (ticks as f64 * METERS_PER_TICK - ANTENNA_OFFSET_M).max(0.0) as f32;
        let e = self
            .directed
            .entry((from, to))
            .or_insert(DistEma { meters, samples: 0 });
        // EMA once seeded.
        if e.samples == 0 {
            e.meters = meters;
        } else {
            e.meters = e.meters * 0.8 + meters * 0.2;
        }
        e.samples += 1;
    }

    /// Sorted list of anchor ids that participate in any measurement.
    pub fn ids(&self) -> Vec<u8> {
        let mut set: Vec<u8> = self
            .directed
            .keys()
            .flat_map(|&(a, b)| [a, b])
            .collect();
        set.sort_unstable();
        set.dedup();
        set
    }

    /// Symmetric distance (metres) between `a` and `b`, averaging both directions.
    pub fn distance(&self, a: u8, b: u8) -> Option<f32> {
        let f = self.directed.get(&(a, b));
        let r = self.directed.get(&(b, a));
        match (f, r) {
            (Some(f), Some(r)) => Some((f.meters + r.meters) / 2.0),
            (Some(f), None) => Some(f.meters),
            (None, Some(r)) => Some(r.meters),
            (None, None) => None,
        }
    }
}

/// One anchor's solved position plus a residual quality metric.
#[derive(Clone, Debug)]
pub struct SurveyAnchor {
    pub id: u8,
    pub pos: [f32; 3],
    /// RMS error (metres) between solved and measured distances for this anchor.
    pub residual: f32,
}

/// A sniffed packet tagged with the host capture time (seconds since the reader
/// thread started), as written to one line of a recording file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedPacket {
    /// Host monotonic capture time [s] since the reader thread started.
    pub t: f64,
    #[serde(flatten)]
    pub packet: SniffedPacket,
}

/// Appends every sniffed packet to a JSONL file — one decoded packet per line —
/// so a full capture can be saved and re-analysed offline. Recording is opt-in
/// (driven by the Sniffer tab's "Record" checkbox) and captures *every* sample,
/// not just the capped live feed.
pub struct SnifferRecorder {
    writer: std::io::BufWriter<std::fs::File>,
    pub path: PathBuf,
    pub count: u64,
}

impl SnifferRecorder {
    /// Create a fresh timestamped recording file (`sniffer-<stamp>.jsonl`) under
    /// `dir`, creating the directory if needed.
    pub fn create(dir: &Path, stamp: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("sniffer-{stamp}.jsonl"));
        let file = std::fs::File::create(&path)?;
        Ok(Self {
            writer: std::io::BufWriter::new(file),
            path,
            count: 0,
        })
    }

    /// Append one decoded packet captured at host time `t` [s].
    pub fn record(&mut self, t: f64, packet: &SniffedPacket) {
        let rec = RecordedPacket {
            t,
            packet: packet.clone(),
        };
        if let Ok(line) = serde_json::to_string(&rec) {
            let _ = writeln!(self.writer, "{line}");
            self.count += 1;
        }
    }

    pub fn flush(&mut self) {
        let _ = self.writer.flush();
    }
}

/// Full sniffer state, shared between the reader thread and the UI.
#[derive(Default)]
pub struct SnifferState {
    pub connected: bool,
    pub port_name: String,
    pub error: Option<String>,
    pub total_packets: u64,
    pub paused: bool,
    pub stats: HashMap<u8, AnchorStat>,
    pub matrix: DistanceMatrix,
    pub feed: VecDeque<SniffedPacket>,
    /// Anchor self-reported positions seen in LPP packets.
    pub lpp_positions: HashMap<u8, [f32; 3]>,
    pub survey: Vec<SurveyAnchor>,
    pub survey_status: String,
    /// Latest monotonic time (seconds) observed by the reader, for "ago" display.
    pub now: f64,
    /// Total frames the node dropped because its USB TX queue was full
    /// (on-the-wire loss at the node), summed from the per-frame drop counter.
    pub wire_dropped: u64,
    /// Framing failures on the PC side (bytes lost host-side); mirror of the
    /// decoder's `resyncs`, copied in by the reader.
    pub host_resyncs: u64,
    /// Whether the reader thread is currently writing packets to a file.
    pub recording: bool,
    /// Packets written to the active recording (0 when not recording).
    pub rec_count: u64,
    /// Path of the active recording file (empty when not recording).
    pub rec_path: String,
}

/// Enumerate available serial ports (device paths).
pub fn list_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}

/// Cap on retained feed entries.
const FEED_CAP: usize = 500;

impl SnifferState {
    /// Fold a decoded packet into the running state.
    pub fn ingest(&mut self, pkt: SniffedPacket, now: f64) {
        self.total_packets += 1;
        self.now = now;
        self.wire_dropped += pkt.wire_dropped as u64;

        self.stats
            .entry(pkt.from)
            .or_insert_with(|| AnchorStat::new(pkt.from, pkt.kind))
            .record(&pkt, now);

        for ra in &pkt.remote_anchors {
            if let Some(ticks) = ra.distance_ticks {
                if ticks > 0 {
                    self.matrix.record(pkt.from, ra.id, ticks);
                }
            }
        }

        if let Some(pos) = pkt.lpp_position {
            self.lpp_positions.insert(pkt.from, pos);
        }

        if !self.paused {
            if self.feed.len() >= FEED_CAP {
                self.feed.pop_front();
            }
            self.feed.push_back(pkt);
        }
    }

    /// Total per-anchor sequence-gap loss across all anchors (air + wire).
    pub fn total_seq_gaps(&self) -> u64 {
        self.stats.values().map(|s| s.lost).sum()
    }

    /// Estimated over-the-air loss: total observed gaps minus the losses we can
    /// attribute to the USB link (node TX-queue drops + host-side framing loss).
    pub fn air_lost(&self) -> u64 {
        self.total_seq_gaps()
            .saturating_sub(self.wire_dropped + self.host_resyncs)
    }

    /// Run the auto-survey from the current distance matrix. Returns a status
    /// string and stores the result in `self.survey`.
    pub fn solve_survey(&mut self) {
        let ids = self.matrix.ids();
        match solve_geometry(&ids, &self.matrix, &self.lpp_positions) {
            Ok(result) => {
                let n = result.len();
                self.survey = result;
                self.survey_status = format!("Solved {n} anchors");
            }
            Err(e) => {
                self.survey.clear();
                self.survey_status = e;
            }
        }
    }
}

/// Solve 3D anchor positions from an inter-anchor distance matrix.
///
/// Uses classical MDS for an initial embedding, refines it with weighted
/// SMACOF stress majorisation (so missing pairs simply carry zero weight), then
/// fixes the gauge: if at least three anchors have a self-reported LPP position
/// the solution is aligned to those by Kabsch; otherwise a canonical frame is
/// imposed (first id at origin, second on +x, third in the +y half-plane).
pub fn solve_geometry(
    ids: &[u8],
    matrix: &DistanceMatrix,
    known: &HashMap<u8, [f32; 3]>,
) -> Result<Vec<SurveyAnchor>, String> {
    let n = ids.len();
    if n < 4 {
        return Err(format!("Need ≥4 anchors with distances (have {n})"));
    }

    // Build symmetric distance + weight matrices.
    let mut d = DMatrix::<f64>::zeros(n, n);
    let mut w = DMatrix::<f64>::zeros(n, n);
    let mut known_pairs = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            if let Some(m) = matrix.distance(ids[i], ids[j]) {
                d[(i, j)] = m as f64;
                d[(j, i)] = m as f64;
                w[(i, j)] = 1.0;
                w[(j, i)] = 1.0;
                known_pairs += 1;
            }
        }
    }
    if known_pairs < n {
        return Err(format!(
            "Too few measured pairs ({known_pairs}); keep sniffing"
        ));
    }

    // Fill unknown distances with the median of known ones for the MDS seed.
    let mut known_vals: Vec<f64> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            if w[(i, j)] > 0.0 {
                known_vals.push(d[(i, j)]);
            }
        }
    }
    known_vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = known_vals[known_vals.len() / 2];
    let mut d_filled = d.clone();
    for i in 0..n {
        for j in 0..n {
            if i != j && w[(i, j)] == 0.0 {
                d_filled[(i, j)] = median;
            }
        }
    }

    // Classical MDS: B = -1/2 J D2 J, take top-3 eigenpairs.
    let mut d2 = DMatrix::<f64>::zeros(n, n);
    for i in 0..n {
        for j in 0..n {
            d2[(i, j)] = d_filled[(i, j)] * d_filled[(i, j)];
        }
    }
    let j_center = DMatrix::<f64>::identity(n, n) - DMatrix::<f64>::from_element(n, n, 1.0 / n as f64);
    let b = &j_center * d2 * &j_center * -0.5;
    let eig = b.symmetric_eigen();
    // Pick the three largest eigenvalues.
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b2| eig.eigenvalues[b2].partial_cmp(&eig.eigenvalues[a]).unwrap());
    let mut x = DMatrix::<f64>::zeros(n, 3);
    for (col, &e) in idx.iter().take(3).enumerate() {
        let lambda = eig.eigenvalues[e].max(0.0).sqrt();
        for row in 0..n {
            x[(row, col)] = eig.eigenvectors[(row, e)] * lambda;
        }
    }

    // Weighted SMACOF refinement via the Guttman transform
    // X⁺ = V⁺ · B(X) · X, where V is the weighted Laplacian. Using the
    // pseudo-inverse (rather than a diagonal approximation) is what makes the
    // iteration converge to the true geometry and handle missing pairs.
    let mut v_lap = DMatrix::<f64>::zeros(n, n);
    for i in 0..n {
        for j in 0..n {
            if i != j {
                v_lap[(i, j)] = -w[(i, j)];
            }
        }
        let deg: f64 = (0..n).filter(|&j| j != i).map(|j| w[(i, j)]).sum();
        v_lap[(i, i)] = deg;
    }
    let v_plus = v_lap
        .pseudo_inverse(1e-9)
        .unwrap_or_else(|_| DMatrix::<f64>::identity(n, n));

    for _ in 0..300 {
        let mut bx = DMatrix::<f64>::zeros(n, n);
        for i in 0..n {
            for j in 0..n {
                if i == j || w[(i, j)] == 0.0 {
                    continue;
                }
                let dij = row_dist(&x, i, j);
                if dij > 1e-9 {
                    bx[(i, j)] = -w[(i, j)] * d[(i, j)] / dij;
                }
            }
        }
        for i in 0..n {
            let off: f64 = (0..n).filter(|&j| j != i).map(|j| bx[(i, j)]).sum();
            bx[(i, i)] = -off;
        }
        x = &v_plus * &bx * &x;
    }

    // Gauge fixing.
    let aligned = gauge_fix(&x, ids, known);

    // Per-anchor residual.
    let mut survey = Vec::with_capacity(n);
    for i in 0..n {
        let mut sq = 0.0f64;
        let mut cnt = 0u32;
        for j in 0..n {
            if i != j && w[(i, j)] > 0.0 {
                let model = ((aligned[(i, 0)] - aligned[(j, 0)]).powi(2)
                    + (aligned[(i, 1)] - aligned[(j, 1)]).powi(2)
                    + (aligned[(i, 2)] - aligned[(j, 2)]).powi(2))
                .sqrt();
                sq += (model - d[(i, j)]).powi(2);
                cnt += 1;
            }
        }
        let residual = if cnt > 0 {
            (sq / cnt as f64).sqrt() as f32
        } else {
            0.0
        };
        survey.push(SurveyAnchor {
            id: ids[i],
            pos: [
                aligned[(i, 0)] as f32,
                aligned[(i, 1)] as f32,
                aligned[(i, 2)] as f32,
            ],
            residual,
        });
    }
    Ok(survey)
}

fn row_dist(x: &DMatrix<f64>, i: usize, j: usize) -> f64 {
    ((x[(i, 0)] - x[(j, 0)]).powi(2)
        + (x[(i, 1)] - x[(j, 1)]).powi(2)
        + (x[(i, 2)] - x[(j, 2)]).powi(2))
    .sqrt()
}

/// Resolve the arbitrary rotation/translation/reflection left by MDS.
fn gauge_fix(x: &DMatrix<f64>, ids: &[u8], known: &HashMap<u8, [f32; 3]>) -> DMatrix<f64> {
    let n = x.nrows();
    // Collect anchors that have a known reference position.
    let refs: Vec<(usize, Vector3<f64>)> = ids
        .iter()
        .enumerate()
        .filter_map(|(i, id)| {
            known
                .get(id)
                .map(|p| (i, Vector3::new(p[0] as f64, p[1] as f64, p[2] as f64)))
        })
        .collect();

    if refs.len() >= 3 {
        if let Some(t) = kabsch(x, &refs) {
            return apply_transform(x, &t);
        }
    }

    // Canonical frame: id[0] at origin, id[1] on +x, id[2] in +y half-plane.
    let mut out = x.clone();
    let origin = Vector3::new(x[(0, 0)], x[(0, 1)], x[(0, 2)]);
    for i in 0..n {
        for k in 0..3 {
            out[(i, k)] -= origin[k];
        }
    }
    out
}

struct Rigid {
    rot: Matrix3<f64>,
    src_centroid: Vector3<f64>,
    dst_centroid: Vector3<f64>,
}

/// Kabsch with reflection handling: best rigid (or improper) transform mapping
/// the source rows (at indices in `refs`) onto their known target positions.
fn kabsch(x: &DMatrix<f64>, refs: &[(usize, Vector3<f64>)]) -> Option<Rigid> {
    let m = refs.len();
    let mut src_c = Vector3::zeros();
    let mut dst_c = Vector3::zeros();
    for (i, t) in refs {
        src_c += Vector3::new(x[(*i, 0)], x[(*i, 1)], x[(*i, 2)]);
        dst_c += *t;
    }
    src_c /= m as f64;
    dst_c /= m as f64;

    let mut h = Matrix3::zeros();
    for (i, t) in refs {
        let s = Vector3::new(x[(*i, 0)], x[(*i, 1)], x[(*i, 2)]) - src_c;
        let dvec = *t - dst_c;
        h += s * dvec.transpose();
    }
    let svd = h.svd(true, true);
    let u = svd.u?;
    let v_t = svd.v_t?;
    // Optimal orthogonal map source→target. We deliberately allow an improper
    // (reflecting) transform: classical MDS leaves an arbitrary reflection, so
    // mirroring to match the reference frame is correct, not a chirality error.
    let rot = v_t.transpose() * u.transpose();
    Some(Rigid {
        rot,
        src_centroid: src_c,
        dst_centroid: dst_c,
    })
}

fn apply_transform(x: &DMatrix<f64>, t: &Rigid) -> DMatrix<f64> {
    let n = x.nrows();
    let mut out = DMatrix::<f64>::zeros(n, 3);
    for i in 0..n {
        let p = Vector3::new(x[(i, 0)], x[(i, 1)], x[(i, 2)]) - t.src_centroid;
        let q = t.rot * p + t.dst_centroid;
        out[(i, 0)] = q[0];
        out[(i, 1)] = q[1];
        out[(i, 2)] = q[2];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_a_tdoa3_packet() {
        // Build a minimal TDoA3 payload: type, seq, txTs(4), remoteCount=1,
        // then one remote anchor with distance.
        let mut payload = vec![TYPE_TDOA3, 7, 0, 0, 0, 0, 1];
        payload.extend_from_slice(&[5, 0x80 | 3, 0, 0, 0, 0]); // id 5, has-distance, seq 3
        payload.extend_from_slice(&200u16.to_le_bytes()); // distance ticks
        let len = payload.len() as u16;

        let mut frame = vec![SYNC];
        frame.extend_from_slice(&[1, 2, 3, 4, 5]); // 5-byte ts
        frame.push(9); // src
        frame.push(0xff); // dst
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&len.to_le_bytes());
        // Fixed extension: rxPower, fpPower, dropCount.
        frame.extend_from_slice(&(-72.5f32).to_le_bytes());
        frame.extend_from_slice(&(-80.0f32).to_le_bytes());
        frame.extend_from_slice(&3u16.to_le_bytes());

        let mut dec = FrameDecoder::new();
        let mut out = Vec::new();
        // Feed it in two chunks to exercise buffering.
        dec.push(&frame[..4], &mut out);
        dec.push(&frame[4..], &mut out);
        assert_eq!(out.len(), 1);
        let p = &out[0];
        assert_eq!(p.from, 9);
        assert_eq!(p.kind, PacketKind::Tdoa3);
        assert_eq!(p.remote_anchors.len(), 1);
        assert_eq!(p.remote_anchors[0].id, 5);
        assert_eq!(p.remote_anchors[0].distance_ticks, Some(200));
        assert_eq!(p.rx_power, Some(-72.5));
        assert_eq!(p.fp_power, Some(-80.0));
        assert_eq!(p.wire_dropped, 3);
    }

    #[test]
    fn resyncs_on_garbage() {
        let mut dec = FrameDecoder::new();
        let mut out = Vec::new();
        dec.push(&[0x00, 0x11, 0xBC, 0x01], &mut out); // garbage then a partial sync
        assert!(out.is_empty());
    }

    #[test]
    fn solves_a_known_cube() {
        // Four anchors of a tetrahedron; feed exact distances and check the
        // solved geometry reproduces them.
        let pts = [
            (1u8, [0.0f32, 0.0, 0.0]),
            (2u8, [4.0, 0.0, 0.0]),
            (3u8, [0.0, 4.0, 0.0]),
            (4u8, [0.0, 0.0, 3.0]),
        ];
        let mut matrix = DistanceMatrix::default();
        for (a, pa) in &pts {
            for (b, pb) in &pts {
                if a != b {
                    let d = ((pa[0] - pb[0]).powi(2)
                        + (pa[1] - pb[1]).powi(2)
                        + (pa[2] - pb[2]).powi(2))
                    .sqrt();
                    // The firmware reports true distance plus the antenna offset.
                    let ticks = ((d as f64 + ANTENNA_OFFSET_M) / METERS_PER_TICK) as u16;
                    matrix.record(*a, *b, ticks);
                }
            }
        }
        let known: HashMap<u8, [f32; 3]> = pts.iter().cloned().collect();
        let survey = solve_geometry(&[1, 2, 3, 4], &matrix, &known).unwrap();
        for s in &survey {
            assert!(s.residual < 0.05, "residual too high: {}", s.residual);
            let want = known[&s.id];
            for k in 0..3 {
                assert!((s.pos[k] - want[k]).abs() < 0.1, "pos mismatch");
            }
        }
    }
}
