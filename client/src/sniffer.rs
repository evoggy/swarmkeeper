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

/// Readings retained per directed anchor pair. The survey takes the median over
/// this window; captures show only a few centimetres of spread within a pair, so
/// 256 is ample and still bounds memory at a few hundred kB for a full arena.
const PAIR_WINDOW: usize = 256;

/// How far below the antenna offset a reading may sit before it is discarded.
/// The DW1000 cannot measure a flight time shorter than the delay the firmware
/// bakes in, so anything meaningfully below it is a corrupt reading rather than
/// a short link.
const MAX_UNDERSHOOT_M: f64 = 0.5;

/// Pooled readings a pair needs (both directions together) before the survey
/// will trust it. Thinly-sampled pairs carry by far the largest residual errors
/// in real captures, and dropping them costs nothing once a link is established.
pub const MIN_PAIR_SAMPLES: u64 = 20;

/// Links an anchor needs before it can be positioned in 3D. Four is the
/// algebraic minimum; six leaves enough redundancy that one bad link cannot drag
/// the anchor off. Sparser anchors are reported as unsolved rather than placed
/// somewhere arbitrary — and, critically, are kept out of the gauge fit, where a
/// single badly-placed anchor would otherwise rotate the whole frame.
pub const MIN_ANCHOR_LINKS: usize = 6;

/// Recent tick readings for one directed anchor pair.
#[derive(Clone, Default)]
struct PairSamples {
    /// Ring of the most recent raw readings, oldest first.
    ticks: VecDeque<u16>,
    /// Every reading ever accepted, including those aged out of `ticks`.
    total: u64,
}

/// Accumulates inter-anchor distances reported in TDoA3 packets.
#[derive(Default)]
pub struct DistanceMatrix {
    /// Directed measurements keyed by (from, to).
    directed: HashMap<(u8, u8), PairSamples>,
}

impl DistanceMatrix {
    fn record(&mut self, from: u8, to: u8, ticks: u16) {
        // Reject the physically impossible instead of clamping it: the old
        // clamp-to-zero turned a corrupt short reading into a fake 0 m link and
        // fed it straight to the survey.
        if ticks as f64 * METERS_PER_TICK < ANTENNA_OFFSET_M - MAX_UNDERSHOOT_M {
            return;
        }
        let e = self.directed.entry((from, to)).or_default();
        if e.ticks.len() >= PAIR_WINDOW {
            e.ticks.pop_front();
        }
        e.ticks.push_back(ticks);
        e.total += 1;
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

    /// Symmetric distance (metres) between `a` and `b`: the median of the
    /// readings pooled from both directions.
    ///
    /// A median rather than the mean of two exponential averages. Inter-anchor
    /// readings are tight — a few centimetres of spread — but do contain
    /// occasional gross outliers, and unlike an EMA a median does not depend on
    /// arrival order, so replaying a recording solves identically every time.
    pub fn distance(&self, a: u8, b: u8) -> Option<f32> {
        let mut pooled: Vec<u16> = Vec::new();
        for key in [(a, b), (b, a)] {
            if let Some(s) = self.directed.get(&key) {
                pooled.extend(s.ticks.iter().copied());
            }
        }
        if pooled.is_empty() {
            return None;
        }
        pooled.sort_unstable();
        let median = pooled[pooled.len() / 2] as f64;
        Some((median * METERS_PER_TICK - ANTENNA_OFFSET_M).max(0.0) as f32)
    }

    /// Readings behind [`Self::distance`] for this pair, both directions summed.
    pub fn sample_count(&self, a: u8, b: u8) -> u64 {
        [(a, b), (b, a)]
            .iter()
            .filter_map(|k| self.directed.get(k))
            .map(|s| s.total)
            .sum()
    }

    /// Whether this pair is sampled well enough to enter the survey.
    fn is_usable(&self, a: u8, b: u8) -> bool {
        self.sample_count(a, b) >= MIN_PAIR_SAMPLES && self.distance(a, b).is_some()
    }
}

/// One anchor's solved position plus a residual quality metric.
#[derive(Clone, Debug)]
pub struct SurveyAnchor {
    pub id: u8,
    pub pos: [f32; 3],
    /// RMS error (metres) between solved and measured distances for this anchor,
    /// over the links that survived NLOS rejection.
    pub residual: f32,
    /// Links to this anchor the robust fit kept.
    pub links_used: usize,
    /// Links to this anchor that entered the fit, before rejection.
    pub links_total: usize,
    /// The position this anchor broadcast about itself, when it broadcast one.
    /// The survey is compared against this.
    pub reference: Option<[f32; 3]>,
}

impl SurveyAnchor {
    /// Offset from the broadcast position to the solved one [m], per axis.
    ///
    /// Meaningful only because the survey is gauge-fixed onto the very same
    /// broadcast positions: the fit recovers a shape, and it is the alignment
    /// that puts that shape in the anchors' declared frame. Comparing against a
    /// *different* set of positions than the solve aligned to would measure the
    /// frame mismatch, not the anchor placement.
    pub fn delta(&self) -> Option<[f32; 3]> {
        let r = self.reference?;
        Some([
            self.pos[0] - r[0],
            self.pos[1] - r[1],
            self.pos[2] - r[2],
        ])
    }

    /// Straight-line distance between the solved and reference positions [m].
    pub fn delta_norm(&self) -> Option<f32> {
        let d = self.delta()?;
        Some((d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt())
    }
}

/// Everything one auto-survey run produced, including what it threw away.
#[derive(Clone, Debug, Default)]
pub struct SurveySolution {
    /// Solved anchors, in ascending id order.
    pub anchors: Vec<SurveyAnchor>,
    /// Anchors left unsolved for want of links, with the link count they had.
    pub excluded: Vec<(u8, usize)>,
    /// Pairs that entered the robust fit.
    pub pairs_total: usize,
    /// Pairs the robust fit rejected as non-line-of-sight.
    pub pairs_rejected: usize,
    /// RMS residual [m] over the pairs the fit kept.
    pub rms: f32,
    /// Anchors that broadcast a position to compare against.
    pub ref_count: usize,
    /// Mean distance [m] between solved and broadcast positions.
    pub ref_mean: f32,
    /// Worst distance [m] between solved and broadcast positions.
    pub ref_max: f32,
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
    /// Positions the anchors report about themselves in the LPP block appended
    /// to their TDoA3 packets — i.e. the geometry each node actually has stored.
    /// This is the survey's reference frame: it needs no second device, since
    /// the sniffer overhears it directly from every anchor.
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

/// Replay a recorded capture (JSONL, as written by [`SnifferRecorder`]) into a
/// fresh [`SnifferState`], ingesting every packet exactly as the live reader
/// would have. Lets a capture be re-solved offline, so a real arena can serve as
/// a survey regression fixture. Malformed lines are skipped rather than fatal —
/// a capture truncated by a crash is still worth replaying.
#[allow(dead_code)] // used by the survey regression tests, not by the running app
pub fn replay_recording(path: &Path) -> std::io::Result<SnifferState> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut state = SnifferState::default();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<RecordedPacket>(&line) {
            state.ingest(rec.packet, rec.t);
        }
    }
    Ok(state)
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
        // Anchor the solution to the positions the nodes broadcast about
        // themselves — the configured geometry straight from the source, since
        // every TDoA3 packet carries the sender's own stored position. With none
        // seen, the fit falls back to a canonical frame and there is nothing to
        // compare against.
        let refs = self.lpp_positions.clone();
        let solved = solve_geometry(&ids, &self.matrix, &refs);
        match solved {
            Ok(sol) => {
                let mut status = format!(
                    "Solved {} anchors · {}/{} pairs rejected as NLOS · rms {:.0} mm",
                    sol.anchors.len(),
                    sol.pairs_rejected,
                    sol.pairs_total,
                    sol.rms * 1000.0
                );
                if sol.ref_count > 0 {
                    status.push_str(&format!(
                        " · vs broadcast positions: mean Δ {:.0} mm, max {:.0} mm over {} anchors",
                        sol.ref_mean * 1000.0,
                        sol.ref_max * 1000.0,
                        sol.ref_count
                    ));
                }
                if !sol.excluded.is_empty() {
                    let list: Vec<String> = sol
                        .excluded
                        .iter()
                        .map(|(id, links)| format!("A{id} ({links} links)"))
                        .collect();
                    status.push_str(&format!(" · too few links: {}", list.join(", ")));
                }
                self.survey = sol.anchors;
                self.survey_status = status;
            }
            Err(e) => {
                self.survey.clear();
                self.survey_status = e;
            }
        }
    }

    /// One-line description of the reference the survey compares against, for
    /// the UI. Reported continuously: the nodes broadcast their stored positions
    /// unprompted, so the operator should see the reference is already in hand
    /// without having to do anything to fetch it.
    pub fn reference_status(&self) -> String {
        if self.lpp_positions.is_empty() {
            "No positions broadcast yet — anchors may have no geometry set".to_string()
        } else {
            format!(
                "{} anchors broadcasting their configured position",
                self.lpp_positions.len()
            )
        }
    }
}

/// Weight below which a link counts as rejected.
const REJECTED_WEIGHT: f64 = 0.05;

/// Robust-fit cutoffs, in units of the residual scale, applied in order. The
/// schedule is deliberately graduated: starting tight would let a contaminated
/// first fit decide which links are outliers and lock the solution into the
/// wrong basin, so the first passes only shave the wildest links and each
/// subsequent pass re-fits before tightening.
const IRLS_CUTOFFS: [f64; 10] = [6.0, 4.0, 3.0, 2.5, 2.0, 1.8, 1.6, 1.5, 1.5, 1.5];

/// Floor on the estimated residual scale [m], so a near-perfect fit can't drive
/// the cutoff to zero and start rejecting healthy links.
const SCALE_FLOOR_M: f64 = 0.10;

/// Solve 3D anchor positions from an inter-anchor distance matrix.
///
/// Uses classical MDS for an initial embedding, then refines it with weighted
/// SMACOF stress majorisation (so missing pairs simply carry zero weight) under
/// iteratively reweighted least squares, and finally fixes the gauge: if at
/// least three anchors have a self-reported LPP position the solution is aligned
/// to those by Kabsch; otherwise a canonical frame is imposed.
///
/// The reweighting is what makes the result usable in a real arena. Inter-anchor
/// readings are individually precise — a few centimetres of spread over minutes —
/// but a handful of *pairs* carry a large static bias, because a blocked direct
/// path means the first path the radio detects is a reflection. Those errors
/// don't average out no matter how long you sniff, and plain least squares
/// smears them across every anchor in the fit. They are also one-sided: a
/// reflection can only ever look *longer* than the truth, never shorter, so an
/// over-long residual is treated as more suspicious than an equally large
/// short one.
pub fn solve_geometry(
    ids: &[u8],
    matrix: &DistanceMatrix,
    known: &HashMap<u8, [f32; 3]>,
) -> Result<SurveySolution, String> {
    // Keep only anchors with enough well-sampled neighbours. Dropping one anchor
    // lowers its neighbours' counts, so repeat to a fixed point, always removing
    // the sparsest first. The requirement relaxes for small installations, where
    // there simply aren't six other anchors to see.
    let mut kept: Vec<u8> = ids.to_vec();
    let mut excluded: Vec<(u8, usize)> = Vec::new();
    loop {
        if kept.len() <= 4 {
            break;
        }
        let required = MIN_ANCHOR_LINKS.min(kept.len() - 1).max(4);
        let degrees: Vec<usize> = kept
            .iter()
            .map(|&id| {
                kept.iter()
                    .filter(|&&o| o != id && matrix.is_usable(id, o))
                    .count()
            })
            .collect();
        let Some((worst, &deg)) = degrees
            .iter()
            .enumerate()
            .min_by_key(|&(_, d)| *d)
            .map(|(i, d)| (i, d))
        else {
            break;
        };
        if deg >= required {
            break;
        }
        excluded.push((kept[worst], deg));
        kept.remove(worst);
    }
    excluded.sort_unstable();

    let n = kept.len();
    if n < 4 {
        return Err(format!(
            "Need ≥4 well-connected anchors (have {n} of {})",
            ids.len()
        ));
    }

    // Build symmetric distance + weight matrices over the surviving anchors.
    let mut d = DMatrix::<f64>::zeros(n, n);
    let mut w0 = DMatrix::<f64>::zeros(n, n);
    let mut pairs_total = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            if !matrix.is_usable(kept[i], kept[j]) {
                continue;
            }
            let Some(m) = matrix.distance(kept[i], kept[j]) else {
                continue;
            };
            d[(i, j)] = m as f64;
            d[(j, i)] = m as f64;
            w0[(i, j)] = 1.0;
            w0[(j, i)] = 1.0;
            pairs_total += 1;
        }
    }
    if pairs_total < n {
        return Err(format!(
            "Too few measured pairs ({pairs_total}); keep sniffing"
        ));
    }

    // Seed the MDS with graph shortest paths for the pairs that were never
    // measured. Walking the measured links to an out-of-earshot anchor lands far
    // closer to the truth than a constant does, and on a sparse matrix that
    // difference decides which basin the whole fit falls into.
    let mut d_filled = DMatrix::<f64>::from_element(n, n, f64::INFINITY);
    for i in 0..n {
        d_filled[(i, i)] = 0.0;
        for j in 0..n {
            if i != j && w0[(i, j)] > 0.0 {
                d_filled[(i, j)] = d[(i, j)];
            }
        }
    }
    for k in 0..n {
        for i in 0..n {
            for j in 0..n {
                let via = d_filled[(i, k)] + d_filled[(k, j)];
                if via < d_filled[(i, j)] {
                    d_filled[(i, j)] = via;
                }
            }
        }
    }
    if let Some((i, j)) = (0..n)
        .flat_map(|i| ((i + 1)..n).map(move |j| (i, j)))
        .find(|&(i, j)| !d_filled[(i, j)].is_finite())
    {
        // No chain of measured links joins these two, so their relative
        // placement is unconstrained — no amount of fitting can recover it.
        return Err(format!(
            "Anchors split into disconnected groups (no path A{} → A{})",
            kept[i], kept[j]
        ));
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

    // Robust fit: alternate SMACOF with a Tukey biweight reweighting of the
    // links, tightening the cutoff on each pass.
    let mut w = w0.clone();
    x = smacof(&d, &w, x);
    for &cutoff in &IRLS_CUTOFFS {
        let mut residuals: Vec<f64> = Vec::with_capacity(pairs_total);
        let mut r = DMatrix::<f64>::zeros(n, n);
        for i in 0..n {
            for j in 0..n {
                if i == j || w0[(i, j)] == 0.0 {
                    continue;
                }
                // Positive residual: measured longer than modelled, i.e. the
                // signature of a reflected (non-line-of-sight) path.
                r[(i, j)] = d[(i, j)] - row_dist(&x, i, j);
                if j > i {
                    residuals.push(r[(i, j)]);
                }
            }
        }
        let scale = mad_scale(&residuals).max(SCALE_FLOOR_M);

        for i in 0..n {
            for j in 0..n {
                if i == j || w0[(i, j)] == 0.0 {
                    continue;
                }
                // One-sided: over-long links are cut at `cutoff` scales, short
                // ones only at 1.5× that, since only reflections inflate a range.
                let u = if r[(i, j)] > 0.0 {
                    r[(i, j)] / (cutoff * scale)
                } else {
                    -r[(i, j)] / (cutoff * scale * 1.5)
                };
                w[(i, j)] = if u < 1.0 {
                    let t = 1.0 - u * u;
                    t * t
                } else {
                    0.0
                };
            }
        }
        // Never orphan an anchor: if every one of its links was rejected the
        // solve has no opinion on where it goes at all, so re-attach them all at
        // a low weight and let the next pass decide.
        for i in 0..n {
            let deg: f64 = (0..n).filter(|&j| j != i).map(|j| w[(i, j)]).sum();
            if deg <= REJECTED_WEIGHT {
                for j in 0..n {
                    if i != j && w0[(i, j)] > 0.0 {
                        w[(i, j)] = 0.1;
                        w[(j, i)] = 0.1;
                    }
                }
            }
        }
        x = smacof(&d, &w, x);
    }

    // Gauge fixing against the anchors that reported their own position. Only
    // the anchors that survived the link gate are candidates, so one sparsely
    // connected anchor can't drag the whole frame round.
    let aligned = gauge_fix(&x, &kept, known);

    // Per-anchor residual and link accounting, over the links the fit kept.
    let mut anchors = Vec::with_capacity(n);
    let mut all_sq = 0.0f64;
    let mut all_cnt = 0u32;
    let mut pairs_rejected = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            if w0[(i, j)] > 0.0 && w[(i, j)] <= REJECTED_WEIGHT {
                pairs_rejected += 1;
            }
        }
    }
    for i in 0..n {
        let mut sq = 0.0f64;
        let mut used = 0usize;
        let mut total = 0usize;
        for j in 0..n {
            if i == j || w0[(i, j)] == 0.0 {
                continue;
            }
            total += 1;
            if w[(i, j)] <= REJECTED_WEIGHT {
                continue;
            }
            used += 1;
            let model = row_dist(&aligned, i, j);
            sq += (model - d[(i, j)]).powi(2);
            if j > i {
                all_sq += (model - d[(i, j)]).powi(2);
                all_cnt += 1;
            }
        }
        let residual = if used > 0 {
            (sq / used as f64).sqrt() as f32
        } else {
            f32::NAN
        };
        anchors.push(SurveyAnchor {
            id: kept[i],
            pos: [
                aligned[(i, 0)] as f32,
                aligned[(i, 1)] as f32,
                aligned[(i, 2)] as f32,
            ],
            residual,
            links_used: used,
            links_total: total,
            reference: known.get(&kept[i]).copied(),
        });
    }
    anchors.sort_by_key(|a| a.id);

    // How far the solved geometry sits from the broadcast positions it was
    // aligned to. Unlike the residual, this catches a survey that is perfectly
    // self-consistent but doesn't match the geometry the nodes have stored.
    let deltas: Vec<f32> = anchors.iter().filter_map(|a| a.delta_norm()).collect();
    let ref_count = deltas.len();
    let ref_mean = if ref_count > 0 {
        deltas.iter().sum::<f32>() / ref_count as f32
    } else {
        0.0
    };
    let ref_max = deltas.iter().copied().fold(0.0f32, f32::max);

    Ok(SurveySolution {
        anchors,
        excluded,
        pairs_total,
        pairs_rejected,
        rms: if all_cnt > 0 {
            (all_sq / all_cnt as f64).sqrt() as f32
        } else {
            0.0
        },
        ref_count,
        ref_mean,
        ref_max,
    })
}

/// Median absolute deviation about the median, scaled to a normal-consistent
/// standard deviation. Robust to the outliers we're trying to find, unlike the
/// plain standard deviation, which they would dominate.
fn mad_scale(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = v[v.len() / 2];
    let mut dev: Vec<f64> = v.iter().map(|x| (x - med).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap());
    1.4826 * dev[dev.len() / 2]
}

/// Maximum Guttman transforms per SMACOF run, if it hasn't converged first.
const MAX_SMACOF_ITERS: usize = 300;
/// Positional change [m] below which a SMACOF run is considered converged.
const SMACOF_TOLERANCE: f64 = 1e-9;

/// Weighted SMACOF refinement via the Guttman transform
/// X⁺ = V⁺ · B(X) · X, where V is the weighted Laplacian. Using the
/// pseudo-inverse (rather than a diagonal approximation) is what makes the
/// iteration converge to the true geometry and handle missing pairs.
///
/// Iterates to convergence rather than a fixed count — the robust fit calls this
/// once per reweighting pass, and after the first pass each run starts from an
/// almost-converged embedding and finishes in a handful of steps.
fn smacof(d: &DMatrix<f64>, w: &DMatrix<f64>, mut x: DMatrix<f64>) -> DMatrix<f64> {
    let n = d.nrows();
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

    for _ in 0..MAX_SMACOF_ITERS {
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
        let next = &v_plus * &bx * &x;
        let delta = (&next - &x).iter().fold(0.0f64, |m, v| m.max(v.abs()));
        x = next;
        if delta < SMACOF_TOLERANCE {
            break;
        }
    }
    x
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
        let matrix = matrix_from(&pts, &[]);
        let known: HashMap<u8, [f32; 3]> = pts.iter().cloned().collect();
        let sol = solve_geometry(&[1, 2, 3, 4], &matrix, &known).unwrap();
        assert_eq!(sol.anchors.len(), 4);
        assert_eq!(sol.pairs_rejected, 0);
        for s in &sol.anchors {
            assert!(s.residual < 0.05, "residual too high: {}", s.residual);
            let want = known[&s.id];
            for k in 0..3 {
                assert!((s.pos[k] - want[k]).abs() < 0.1, "pos mismatch");
            }
        }
    }

    /// Build a matrix from exact geometry, applying `bias` metres to the named
    /// pairs — the way a reflected (non-line-of-sight) path lengthens a reading.
    fn matrix_from(pts: &[(u8, [f32; 3])], bias: &[(u8, u8, f64)]) -> DistanceMatrix {
        let mut matrix = DistanceMatrix::default();
        for (a, pa) in pts {
            for (b, pb) in pts {
                if a == b {
                    continue;
                }
                let d = ((pa[0] - pb[0]).powi(2) + (pa[1] - pb[1]).powi(2) + (pa[2] - pb[2]).powi(2))
                    .sqrt() as f64;
                let extra = bias
                    .iter()
                    .find(|(x, y, _)| (x, y) == (a, b) || (x, y) == (b, a))
                    .map_or(0.0, |&(_, _, m)| m);
                // The firmware reports true distance plus the antenna offset.
                let ticks = ((d + extra + ANTENNA_OFFSET_M) / METERS_PER_TICK) as u16;
                for _ in 0..MIN_PAIR_SAMPLES {
                    matrix.record(*a, *b, ticks);
                }
            }
        }
        matrix
    }

    /// The eight corners of a box — enough redundancy that one bad link can be
    /// identified and dropped.
    fn box_anchors() -> Vec<(u8, [f32; 3])> {
        let mut pts = Vec::new();
        for (i, (x, y, z)) in [
            (0.0f32, 0.0f32, 0.0f32),
            (8.0, 0.0, 0.0),
            (8.0, 6.0, 0.0),
            (0.0, 6.0, 0.0),
            (0.0, 0.0, 3.0),
            (8.0, 0.0, 3.0),
            (8.0, 6.0, 3.0),
            (0.0, 6.0, 3.0),
        ]
        .into_iter()
        .enumerate()
        {
            pts.push((i as u8 + 1, [x, y, z]));
        }
        pts
    }

    #[test]
    fn rejects_a_reflected_link() {
        let pts = box_anchors();
        let ids: Vec<u8> = pts.iter().map(|(id, _)| *id).collect();
        let known: HashMap<u8, [f32; 3]> = pts.iter().cloned().collect();

        // Anchors 1 and 7 are diagonally opposite; pretend the direct path is
        // blocked and the radio locks onto a reflection 3 m longer.
        let matrix = matrix_from(&pts, &[(1, 7, 3.0)]);
        let sol = solve_geometry(&ids, &matrix, &known).unwrap();

        assert_eq!(sol.pairs_rejected, 1, "expected exactly the bad link dropped");
        assert!(sol.rms < 0.05, "rms should be clean once dropped: {}", sol.rms);
        for a in &sol.anchors {
            let want = known[&a.id];
            let err = ((a.pos[0] - want[0]).powi(2)
                + (a.pos[1] - want[1]).powi(2)
                + (a.pos[2] - want[2]).powi(2))
            .sqrt();
            assert!(err < 0.1, "A{} off by {err:.3} m", a.id);
        }
        // The two ends of the rejected link lost one usable neighbour each.
        for a in sol.anchors.iter().filter(|a| a.id == 1 || a.id == 7) {
            assert_eq!(a.links_used + 1, a.links_total);
        }
    }

    #[test]
    fn excludes_an_underconnected_anchor() {
        let mut pts = box_anchors();
        // A ninth anchor that only ever hears three of the others.
        pts.push((9u8, [4.0, 3.0, 1.5]));
        let ids: Vec<u8> = pts.iter().map(|(id, _)| *id).collect();
        let known: HashMap<u8, [f32; 3]> = pts.iter().cloned().collect();

        let mut matrix = matrix_from(&pts, &[]);
        // Erase A9's links except to 1, 2 and 3 by rebuilding without them.
        matrix.directed.retain(|&(a, b), _| {
            (a != 9 && b != 9) || matches!(a.min(b), 1..=3) && a.max(b) == 9
        });

        let sol = solve_geometry(&ids, &matrix, &known).unwrap();
        assert_eq!(sol.excluded, vec![(9u8, 3usize)]);
        assert!(sol.anchors.iter().all(|a| a.id != 9));
        assert_eq!(sol.anchors.len(), 8);
    }

    #[test]
    fn reports_offset_from_the_broadcast_position() {
        let pts = box_anchors();
        let ids: Vec<u8> = pts.iter().map(|(id, _)| *id).collect();
        let matrix = matrix_from(&pts, &[]);

        // A6 broadcasts a position a metre above where the radios place it — an
        // anchor that was physically moved without its stored geometry being
        // updated, which is the failure this comparison exists to catch.
        let mut broadcast: HashMap<u8, [f32; 3]> = pts.iter().cloned().collect();
        broadcast.get_mut(&6).unwrap()[2] += 1.0;

        let sol = solve_geometry(&ids, &matrix, &broadcast).unwrap();
        assert_eq!(sol.ref_count, 8);

        let a6 = sol.anchors.iter().find(|a| a.id == 6).unwrap();
        let d6 = a6.delta_norm().unwrap();
        // Kabsch spreads part of the discrepancy over the whole set, so A6 keeps
        // most of the metre rather than all of it — but it must stand out.
        assert!(d6 > 0.6, "A6 offset should dominate, got {d6:.3} m");
        assert!((sol.ref_max - d6).abs() < 1e-5, "A6 should be the worst");
        for a in sol.anchors.iter().filter(|a| a.id != 6) {
            let d = a.delta_norm().unwrap();
            assert!(d < 0.35, "A{} should stay put, moved {d:.3} m", a.id);
        }
        // The fit itself is still perfectly self-consistent: the disagreement is
        // with the configuration, not within the measurements.
        assert!(sol.rms < 0.01, "rms {} should stay clean", sol.rms);
    }

    #[test]
    fn compares_against_the_nodes_own_broadcast_by_default() {
        let pts = box_anchors();
        let mut state = SnifferState::default();
        state.matrix = matrix_from(&pts, &[]);
        // Every TDoA3 packet carries the sending node's stored position, so the
        // sniffer alone already has the configured geometry to compare against —
        // no other device is involved.
        for (id, p) in &pts {
            state.lpp_positions.insert(*id, *p);
        }
        assert!(
            state.reference_status().contains("8 anchors broadcasting"),
            "{}",
            state.reference_status()
        );

        state.solve_survey();
        assert_eq!(state.survey.len(), 8);
        for a in &state.survey {
            let want = pts.iter().find(|(id, _)| *id == a.id).unwrap().1;
            assert_eq!(a.reference, Some(want));
            let d = a.delta_norm().unwrap();
            assert!(d < 0.05, "A{} off by {d:.3} m from its own broadcast", a.id);
        }

        // With no broadcast at all, say so rather than leaving the field blank.
        let silent = SnifferState::default();
        assert!(
            silent.reference_status().contains("No positions broadcast"),
            "{}",
            silent.reference_status()
        );
    }

    #[test]
    fn median_ignores_a_gross_outlier() {
        let mut matrix = DistanceMatrix::default();
        let good = ((5.0 + ANTENNA_OFFSET_M) / METERS_PER_TICK) as u16;
        for _ in 0..40 {
            matrix.record(1, 2, good);
        }
        // One reading 10 m long, one physically impossible short one.
        matrix.record(1, 2, good + (10.0 / METERS_PER_TICK) as u16);
        matrix.record(1, 2, 1000);
        let d = matrix.distance(1, 2).unwrap();
        assert!((d - 5.0).abs() < 0.01, "median dragged to {d}");
        // The impossible reading was dropped outright, the long one retained.
        assert_eq!(matrix.sample_count(1, 2), 41);
    }

    /// Replay every capture under `client/recordings/sniffer/` and check the
    /// survey against the anchors' self-reported (LPP) positions. Recordings
    /// aren't committed, so this reports and passes when there are none.
    #[test]
    fn replays_recorded_captures() {
        let dir = Path::new("recordings/sniffer");
        let Ok(entries) = std::fs::read_dir(dir) else {
            eprintln!("no {} — skipping replay regression", dir.display());
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort();
        let mut checked = 0;
        for file in &files {
            let state = replay_recording(file).expect("read recording");
            // Only captures where the anchors broadcast their own positions can
            // be scored; the rest just have to solve without erroring.
            let sol = match solve_geometry(
                &state.matrix.ids(),
                &state.matrix,
                &state.lpp_positions,
            ) {
                Ok(sol) => sol,
                Err(e) => panic!("{}: solve failed: {e}", file.display()),
            };
            let scored: Vec<f32> = sol
                .anchors
                .iter()
                .filter_map(|a| {
                    let p = state.lpp_positions.get(&a.id)?;
                    Some(
                        ((a.pos[0] - p[0]).powi(2)
                            + (a.pos[1] - p[1]).powi(2)
                            + (a.pos[2] - p[2]).powi(2))
                        .sqrt(),
                    )
                })
                .collect();
            if scored.len() < 4 {
                eprintln!("{}: no reference positions, skipped", file.display());
                continue;
            }
            let mean = scored.iter().sum::<f32>() / scored.len() as f32;
            let max = scored.iter().cloned().fold(0.0f32, f32::max);
            eprintln!(
                "{}: {} anchors, {}/{} pairs rejected, ref err mean {:.2} m max {:.2} m",
                file.file_name().unwrap().to_string_lossy(),
                sol.anchors.len(),
                sol.pairs_rejected,
                sol.pairs_total,
                mean,
                max
            );
            assert!(
                mean < 0.6,
                "{}: mean error vs reference regressed to {mean:.2} m",
                file.display()
            );
            checked += 1;
        }
        eprintln!("replayed {} capture(s), scored {checked}", files.len());
    }
}
