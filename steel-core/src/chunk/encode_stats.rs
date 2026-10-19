//! Console statistics for chunk network encoding, for comparing encoder builds side by side.
//!
//! Every chunk sent to a player records how long its sections took to encode and how long its
//! whole packet took. A background thread prints a report every few seconds while chunks are
//! being sent, plus running totals since the server started.

use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use steel_utils::locks::SyncMutex;

/// Which build this is, shown in the startup banner and every report.
pub const BUILD_LABEL: &str = "FASTER SECTION ENCODING - faster-section-encoding 5615ba7";

const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Interval {
    /// Time to encode all sections of one chunk, per chunk.
    section_ns: Vec<u64>,
    /// Time to build one whole chunk packet (sections, heightmaps, block entities, light,
    /// compression), per chunk.
    packet_ns: Vec<u64>,
    sections: u64,
    section_bytes: u64,
}

#[derive(Default)]
struct Totals {
    chunks: u64,
    sections: u64,
    section_bytes: u64,
    section_ns: u64,
    packet_chunks: u64,
    packet_ns: u64,
}

#[derive(Default)]
struct State {
    interval: Interval,
    totals: Totals,
}

static STATE: OnceLock<SyncMutex<State>> = OnceLock::new();

fn state() -> &'static SyncMutex<State> {
    STATE.get_or_init(|| {
        let spawned = thread::Builder::new()
            .name("encode-stats".to_owned())
            .spawn(report_loop);
        if let Err(error) = spawned {
            tracing::warn!("chunk encoding statistics disabled: {error}");
        }
        SyncMutex::new(State::default())
    })
}

/// Prints which build is running and what the reports contain.
pub fn log_build_banner() {
    tracing::info!("==================================================================");
    tracing::info!("  Build: {BUILD_LABEL}");
    tracing::info!("  Chunk encoding statistics are printed every 5 s while chunks are");
    tracing::info!("  sent to players. Join on localhost and move around to load chunks.");
    tracing::info!("==================================================================");
}

/// Records the encoding of one chunk's sections.
pub fn record_sections(sections: usize, bytes: usize, elapsed: Duration) {
    let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let mut state = state().lock();
    state.interval.section_ns.push(ns);
    state.interval.sections += sections as u64;
    state.interval.section_bytes += bytes as u64;
    state.totals.chunks += 1;
    state.totals.sections += sections as u64;
    state.totals.section_bytes += bytes as u64;
    state.totals.section_ns += ns;
}

/// Records building one whole chunk packet.
pub fn record_packet(elapsed: Duration) {
    let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
    let mut state = state().lock();
    state.interval.packet_ns.push(ns);
    state.totals.packet_chunks += 1;
    state.totals.packet_ns += ns;
}

fn report_loop() {
    let mut last = Instant::now();
    loop {
        thread::sleep(REPORT_INTERVAL);
        let elapsed = last.elapsed();
        last = Instant::now();
        let (mut interval, totals) = {
            let mut state = state().lock();
            let interval = std::mem::take(&mut state.interval);
            let t = &state.totals;
            let totals = Totals {
                chunks: t.chunks,
                sections: t.sections,
                section_bytes: t.section_bytes,
                section_ns: t.section_ns,
                packet_chunks: t.packet_chunks,
                packet_ns: t.packet_ns,
            };
            (interval, totals)
        };
        if interval.section_ns.is_empty() {
            continue;
        }
        report(&mut interval, &totals, elapsed);
    }
}

fn report(interval: &mut Interval, totals: &Totals, elapsed: Duration) {
    interval.section_ns.sort_unstable();
    interval.packet_ns.sort_unstable();
    let chunks = interval.section_ns.len() as u64;
    let section_total: u64 = interval.section_ns.iter().sum();
    let packet_total: u64 = interval.packet_ns.iter().sum();
    let seconds = elapsed.as_secs_f64();

    tracing::info!(
        "[chunk encoding | {BUILD_LABEL}] last {seconds:.1} s: {chunks} chunks ({:.1}/s), {} sections, {} of section data ({} per chunk)",
        chunks as f64 / seconds,
        interval.sections,
        bytes(interval.section_bytes),
        bytes(interval.section_bytes / chunks),
    );
    tracing::info!(
        "    section encoding: {} total | per chunk: mean {}, p50 {}, p95 {}, p99 {}, max {} | per section: {}",
        time(section_total),
        time(section_total / chunks),
        time(percentile(&interval.section_ns, 50)),
        time(percentile(&interval.section_ns, 95)),
        time(percentile(&interval.section_ns, 99)),
        time(interval.section_ns.last().copied().unwrap_or(0)),
        time(section_total / interval.sections.max(1)),
    );
    if !interval.packet_ns.is_empty() {
        let packets = interval.packet_ns.len() as u64;
        tracing::info!(
            "    whole chunk packet (sections + heightmaps + block entities + light + compression): per chunk: mean {}, p50 {}, p99 {} | sections are {:.0}% of it",
            time(packet_total / packets),
            time(percentile(&interval.packet_ns, 50)),
            time(percentile(&interval.packet_ns, 99)),
            100.0 * section_total as f64 / packet_total.max(1) as f64,
        );
    }
    tracing::info!(
        "    since start: {} chunks, {} sections, {} of section data | section encoding {} total, mean {} per chunk | whole packet mean {} per chunk",
        totals.chunks,
        totals.sections,
        bytes(totals.section_bytes),
        time(totals.section_ns),
        time(totals.section_ns / totals.chunks.max(1)),
        time(totals.packet_ns / totals.packet_chunks.max(1)),
    );
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) * percent).div_ceil(100)]
}

fn time(ns: u64) -> String {
    match ns {
        0..1_000 => format!("{ns} ns"),
        1_000..1_000_000 => format!("{:.1} us", ns as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.2} ms", ns as f64 / 1e6),
        _ => format!("{:.2} s", ns as f64 / 1e9),
    }
}

fn bytes(count: u64) -> String {
    match count {
        0..1_024 => format!("{count} B"),
        1_024..1_048_576 => format!("{:.1} KiB", count as f64 / 1024.0),
        _ => format!("{:.2} MiB", count as f64 / 1_048_576.0),
    }
}
