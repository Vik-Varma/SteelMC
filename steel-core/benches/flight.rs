//! Real-time flight benchmark.
//!
//! Starts a real server on a RAM-only overworld, joins one player through an in-memory Java
//! connection, waits for its spawn view to load, then teleports it along a straight line at a
//! fixed speed every server tick. A fake client acknowledges chunk batches and keep-alives and
//! timestamps every chunk packet. The report says how late chunks arrive after they enter the
//! player's view and how often chunks near the player are still missing.
//!
//! `cargo bench -p steel-core --bench flight -- [options]`, see [`Options::parse`].
#![expect(clippy::print_stdout, reason = "the benchmark prints its report")]

use std::env;
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::DVec3;
use rustc_hash::FxHashMap;
use steel_core::bootstrap::init_globals;
use steel_core::config::{RuntimeConfig, WorldsConfig};
use steel_core::entity::{Entity as _, next_entity_id};
use steel_core::permission::{PermissionGroupManager, PermissionGroupsConfig};
use steel_core::player::connection::{
    JavaConnection, JavaNetworkReader, JavaNetworkWriter, JavaTransportRead, OutboundPacket,
};
use steel_core::player::{ClientInformation, Player, PlayerConnection, PlayerSession};
use steel_core::server::Server;
use steel_core::server::jobs::{JobPoll, ServerJob, ServerJobContext};
use steel_protocol::packet_reader::TCPNetworkDecoder;
use steel_registry::game_profile::GameProfile;
use steel_registry::packets::play::{
    C_CHUNK_BATCH_FINISHED, C_FORGET_LEVEL_CHUNK, C_KEEP_ALIVE, C_LEVEL_CHUNK_WITH_LIGHT,
    C_PLAYER_POSITION, S_ACCEPT_TELEPORTATION, S_CHUNK_BATCH_RECEIVED, S_KEEP_ALIVE,
};
use steel_utils::locks::{AsyncMutex, SyncMutex};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, duplex};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TICKS_PER_SECOND: f64 = 20.0;

struct Options {
    seed: String,
    speed: f64,
    heading_degrees: f64,
    seconds: f64,
    view_distance: u8,
    near_radius: f64,
    desired_chunks_per_tick: f32,
    warmup_timeout: f64,
    fly_y: f64,
}

impl Options {
    /// `--seed S --speed B/s --heading DEG --seconds N --view-distance N --near R
    /// --client-rate CHUNKS_PER_TICK --y Y`
    fn parse() -> Self {
        let mut options = Self {
            seed: "12345".to_owned(),
            speed: 30.0,
            heading_degrees: 0.0,
            seconds: 60.0,
            view_distance: 10,
            near_radius: 4.0,
            desired_chunks_per_tick: 500.0,
            warmup_timeout: 120.0,
            fly_y: 160.0,
        };
        let args: Vec<String> = env::args().skip(1).filter(|arg| arg != "--bench").collect();
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            let value = iter
                .next()
                .unwrap_or_else(|| panic!("{flag} needs a value"));
            let number = || {
                value
                    .parse::<f64>()
                    .unwrap_or_else(|_| panic!("bad {flag}"))
            };
            match flag.as_str() {
                "--seed" => options.seed.clone_from(value),
                "--speed" => options.speed = number(),
                "--heading" => options.heading_degrees = number(),
                "--seconds" => options.seconds = number(),
                "--view-distance" => options.view_distance = number() as u8,
                "--near" => options.near_radius = number(),
                "--client-rate" => options.desired_chunks_per_tick = number() as f32,
                "--warmup-timeout" => options.warmup_timeout = number(),
                "--y" => options.fly_y = number(),
                _ => panic!("unknown option {flag}"),
            }
        }
        options
    }
}

/// What the fake client saw.
#[derive(Default)]
struct ClientLog {
    /// First arrival of each chunk. Forgetting a chunk does not clear it, so the report can
    /// still see chunks the player has already flown past.
    arrivals: FxHashMap<(i32, i32), Instant>,
    chunk_packets: u64,
    forgotten: u64,
    batches: u64,
}

/// Where the player was on each flight tick.
#[derive(Default)]
struct FlightLog {
    start: Option<(u64, DVec3)>,
    samples: Vec<(Instant, DVec3)>,
    done: bool,
}

struct FlightJob {
    player: Arc<Player>,
    log: Arc<SyncMutex<FlightLog>>,
    direction: DVec3,
    speed: f64,
    seconds: f64,
    yaw: f32,
}

impl ServerJob for FlightJob {
    fn poll(&mut self, context: &mut ServerJobContext) -> JobPoll {
        let mut log = self.log.lock();
        let Some((start_tick, start_pos)) = log.start else {
            return JobPoll::Pending;
        };
        let elapsed = (context.tick_count.saturating_sub(start_tick)) as f64 / TICKS_PER_SECOND;
        let pos = start_pos + self.direction * (self.speed * elapsed);
        if self.player.teleport(pos, self.yaw, 0.0).is_err() {
            log::warn!("flight teleport rejected at {pos}");
        }
        log.samples.push((Instant::now(), pos));
        if elapsed >= self.seconds {
            log.done = true;
            return JobPoll::Finished;
        }
        JobPoll::Pending
    }
}

fn read_varint(data: &[u8], pos: &mut usize) -> i32 {
    let mut value = 0i32;
    for shift in (0..35).step_by(7) {
        let byte = data[*pos];
        *pos += 1;
        value |= i32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
    }
    value
}

fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    loop {
        if value & !0x7f == 0 {
            out.push(value as u8);
            return;
        }
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
}

/// Uncompressed play frame: length, packet id, body.
fn frame(id: i32, body: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(body.len() + 5);
    write_varint(&mut payload, id);
    payload.extend_from_slice(body);
    let mut framed = Vec::with_capacity(payload.len() + 5);
    write_varint(&mut framed, payload.len() as i32);
    framed.extend_from_slice(&payload);
    framed
}

async fn run_client(
    mut outgoing: mpsc::UnboundedReceiver<OutboundPacket>,
    mut to_server: DuplexStream,
    log: Arc<SyncMutex<ClientLog>>,
    desired_chunks_per_tick: f32,
) {
    while let Some(packet) = outgoing.recv().await {
        let (OutboundPacket::Packet(packet) | OutboundPacket::Disconnect(packet)) = packet;
        let data = packet.encoded_data.as_slice();
        let mut pos = 0;
        let _length = read_varint(data, &mut pos);
        let id = read_varint(data, &mut pos);
        let body = &data[pos..];
        let reply = match id {
            C_LEVEL_CHUNK_WITH_LIGHT => {
                let x = i32::from_be_bytes(body[0..4].try_into().expect("chunk x"));
                let z = i32::from_be_bytes(body[4..8].try_into().expect("chunk z"));
                let mut log = log.lock();
                log.chunk_packets += 1;
                log.arrivals.entry((x, z)).or_insert_with(Instant::now);
                None
            }
            C_FORGET_LEVEL_CHUNK => {
                log.lock().forgotten += 1;
                None
            }
            C_CHUNK_BATCH_FINISHED => {
                log.lock().batches += 1;
                Some(frame(
                    S_CHUNK_BATCH_RECEIVED,
                    &desired_chunks_per_tick.to_be_bytes(),
                ))
            }
            C_KEEP_ALIVE => Some(frame(S_KEEP_ALIVE, &body[0..8])),
            C_PLAYER_POSITION => {
                let mut body_pos = 0;
                let teleport_id = read_varint(body, &mut body_pos);
                let mut reply = Vec::new();
                write_varint(&mut reply, teleport_id);
                Some(frame(S_ACCEPT_TELEPORTATION, &reply))
            }
            _ => None,
        };
        if let Some(reply) = reply
            && to_server.write_all(&reply).await.is_err()
        {
            break;
        }
    }
}

fn worlds_config(seed: &str) -> WorldsConfig {
    let text = format!(
        r#"
save_path = "flight-bench-saves"
seed = "{seed}"
default_gamemode = "spectator"

[storage]
type = "steel:ram"

[player_storage]
type = "steel:ram"

[domains.minecraft]
default = true

[[domains.minecraft.worlds]]
name = "overworld"
generator = "minecraft:overworld"
default = true
"#
    );
    toml::from_str(&text).expect("flight bench worlds config")
}

fn runtimes() -> (Runtime, Arc<Runtime>) {
    let main = Builder::new_multi_thread()
        .enable_all()
        .thread_name("flight-main")
        .build()
        .expect("main runtime");
    let chunk = Builder::new_multi_thread()
        .enable_all()
        .thread_name("flight-chunk")
        .build()
        .expect("chunk runtime");
    (main, Arc::new(chunk))
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index]
}

fn chunk_of(pos: DVec3) -> (i32, i32) {
    ((pos.x / 16.0).floor() as i32, (pos.z / 16.0).floor() as i32)
}

fn distance(a: (i32, i32), b: (i32, i32)) -> f64 {
    f64::from(a.0 - b.0).hypot(f64::from(a.1 - b.1))
}

/// Steel's `PlayerChunkView::contains`: the chunks the server sends for this view.
fn in_view(center: (i32, i32), view_distance: i32, chunk: (i32, i32)) -> bool {
    let dx = i64::from(((chunk.0 - center.0).abs() - 2).max(0));
    let dz = i64::from(((chunk.1 - center.1).abs() - 2).max(0));
    dx * dx + dz * dz < i64::from(view_distance) * i64::from(view_distance)
}

fn report(options: &Options, flight: &FlightLog, client: &ClientLog, flight_end: Instant) {
    let view = i32::from(options.view_distance);
    // When each chunk first came into the server's view of the player during the flight.
    let mut entered: FxHashMap<(i32, i32), Instant> = FxHashMap::default();
    let mut hole_ticks = 0usize;
    let mut hole_fraction_sum = 0.0;
    let mut worst_holes = 0usize;
    let near = options.near_radius.ceil() as i32;
    for &(time, pos) in &flight.samples {
        let center = chunk_of(pos);
        let reach = view + 2;
        for dx in -reach..=reach {
            for dz in -reach..=reach {
                let chunk = (center.0 + dx, center.1 + dz);
                if in_view(center, view, chunk) {
                    entered.entry(chunk).or_insert(time);
                }
            }
        }
        let (mut total, mut missing) = (0usize, 0usize);
        for dx in -near..=near {
            for dz in -near..=near {
                let chunk = (center.0 + dx, center.1 + dz);
                if distance(chunk, center) > options.near_radius {
                    continue;
                }
                total += 1;
                if client
                    .arrivals
                    .get(&chunk)
                    .is_none_or(|&arrived| arrived > time)
                {
                    missing += 1;
                }
            }
        }
        if missing > 0 {
            hole_ticks += 1;
        }
        worst_holes = worst_holes.max(missing);
        hole_fraction_sum += missing as f64 / total as f64;
    }

    let mut late = Vec::new();
    let mut never = 0usize;
    for (chunk, &entered_at) in &entered {
        match client.arrivals.get(chunk) {
            Some(&arrived) if arrived >= entered_at => {
                late.push((arrived - entered_at).as_secs_f64() * 1000.0);
            }
            Some(_) => late.push(0.0),
            None => {
                never += 1;
                late.push((flight_end - entered_at).as_secs_f64() * 1000.0);
            }
        }
    }
    late.sort_by(f64::total_cmp);
    let ticks = flight.samples.len().max(1);
    println!(
        "flight: seed {} speed {} b/s heading {} deg for {} s, view {} near {}, client rate {}",
        options.seed,
        options.speed,
        options.heading_degrees,
        options.seconds,
        options.view_distance,
        options.near_radius,
        options.desired_chunks_per_tick
    );
    println!(
        "chunks entering view: {}  never arrived: {}  chunk packets: {}  batches: {}  forgotten: {}",
        entered.len(),
        never,
        client.chunk_packets,
        client.batches,
        client.forgotten
    );
    println!(
        "lateness after entering view (ms): p50 {:.0}  p90 {:.0}  p95 {:.0}  p99 {:.0}  max {:.0}",
        percentile(&late, 0.50),
        percentile(&late, 0.90),
        percentile(&late, 0.95),
        percentile(&late, 0.99),
        late.last().copied().unwrap_or(f64::NAN)
    );
    println!(
        "holes within {} chunks: in {:.1}% of ticks, mean {:.2}% missing, worst {} chunks",
        options.near_radius,
        100.0 * hole_ticks as f64 / ticks as f64,
        100.0 * hole_fraction_sum / ticks as f64,
        worst_holes
    );
    println!(
        "RESULT p50_ms={:.0} p95_ms={:.0} p99_ms={:.0} max_ms={:.0} hole_tick_pct={:.1} hole_mean_pct={:.2} never={}",
        percentile(&late, 0.50),
        percentile(&late, 0.95),
        percentile(&late, 0.99),
        late.last().copied().unwrap_or(f64::NAN),
        100.0 * hole_ticks as f64 / ticks as f64,
        100.0 * hole_fraction_sum / ticks as f64,
        never
    );
}

fn main() {
    let options = Options::parse();
    init_globals();
    let (main_runtime, chunk_runtime) = runtimes();
    main_runtime.block_on(run(options, chunk_runtime));
    // Server tasks keep running until the process ends.
    process::exit(0);
}

async fn run(options: Options, chunk_runtime: Arc<Runtime>) {
    let cancel = CancellationToken::new();
    let config = RuntimeConfig {
        view_distance: options.view_distance,
        simulation_distance: options.view_distance,
        online_mode: false,
        encryption: false,
        compression: None,
        ..RuntimeConfig::default()
    };
    let permissions = PermissionGroupManager::transient(PermissionGroupsConfig::default())
        .expect("default permission groups");
    let setup_started = Instant::now();
    let server = Arc::new(
        Server::new(
            chunk_runtime,
            cancel.clone(),
            config,
            worlds_config(&options.seed),
            permissions,
        )
        .await
        .expect("server"),
    );
    println!(
        "server ready in {:.1} s",
        setup_started.elapsed().as_secs_f64()
    );
    tokio::spawn(Arc::clone(&server).run(cancel.clone()));

    let (player, client_log) = join_player(&server, &cancel, &options);

    // Warm up: wait until the spawn view has arrived (or the timeout passes).
    let join_started = Instant::now();
    let view = i32::from(options.view_distance);
    let spawn_chunk = loop {
        sleep(Duration::from_millis(100)).await;
        let center = chunk_of(player.position());
        let reach = view + 2;
        let (mut wanted, mut have) = (0usize, 0usize);
        {
            let log = client_log.lock();
            for dx in -reach..=reach {
                for dz in -reach..=reach {
                    let chunk = (center.0 + dx, center.1 + dz);
                    if in_view(center, view, chunk) {
                        wanted += 1;
                        have += usize::from(log.arrivals.contains_key(&chunk));
                    }
                }
            }
        }
        if (have == wanted && have > 0)
            || join_started.elapsed().as_secs_f64() > options.warmup_timeout
        {
            println!(
                "spawn view loaded: {have}/{wanted} chunks in {:.1} s",
                join_started.elapsed().as_secs_f64()
            );
            break center;
        }
    };

    let heading = options.heading_degrees.to_radians();
    let direction = DVec3::new(-heading.sin(), 0.0, heading.cos());
    let start = DVec3::new(
        f64::from(spawn_chunk.0) * 16.0 + 8.0,
        options.fly_y,
        f64::from(spawn_chunk.1) * 16.0 + 8.0,
    );
    let flight_log = Arc::new(SyncMutex::new(FlightLog::default()));
    server.jobs.spawn(FlightJob {
        player: Arc::clone(&player),
        log: Arc::clone(&flight_log),
        direction,
        speed: options.speed,
        seconds: options.seconds,
        yaw: options.heading_degrees as f32,
    });
    flight_log.lock().start = Some((server.current_tick() + 1, start));

    loop {
        sleep(Duration::from_millis(100)).await;
        if flight_log.lock().done {
            break;
        }
    }
    let flight_end = Instant::now();
    let flight = flight_log.lock();
    let client = client_log.lock();
    report(&options, &flight, &client, flight_end);
    cancel.cancel();
}

/// Joins one player through an in-memory Java connection and starts its fake client.
fn join_player(
    server: &Arc<Server>,
    cancel: &CancellationToken,
    options: &Options,
) -> (Arc<Player>, Arc<SyncMutex<ClientLog>>) {
    // In-memory Java connection: the client writes frames into `to_server`, the server's
    // outgoing packets arrive on `outgoing` without a socket writer.
    let (to_server, server_read) = duplex(1 << 20);
    let (outgoing_sender, outgoing) = mpsc::unbounded_channel();
    let network_writer: JavaNetworkWriter = Arc::new(AsyncMutex::new(None));
    let session = Arc::new(PlayerSession::new(10, 10));
    let connection = Arc::new(PlayerConnection::Java(JavaConnection::new(
        outgoing_sender,
        cancel.clone(),
        None,
        network_writer,
        1,
        Arc::clone(&session),
    )));
    let profile = GameProfile::new(Uuid::new_v4(), "FlightBench".to_owned(), Vec::new())
        .expect("game profile");
    let client_information = ClientInformation {
        view_distance: options.view_distance,
        ..ClientInformation::default()
    };
    let player = Arc::new(Player::new(
        profile,
        Arc::clone(&connection),
        Arc::clone(&session),
        Arc::clone(server.overworld()),
        Arc::downgrade(server),
        Arc::clone(&server.config),
        next_entity_id(),
        client_information,
    ));
    assert!(session.bind_initial_player(&player), "fresh session");
    player.set_client_loaded(true);

    let client_log = Arc::new(SyncMutex::new(ClientLog::default()));
    tokio::spawn(run_client(
        outgoing,
        to_server,
        Arc::clone(&client_log),
        options.desired_chunks_per_tick,
    ));
    let reader: JavaNetworkReader =
        TCPNetworkDecoder::new(BufReader::new(Box::new(server_read) as JavaTransportRead));
    tokio::spawn({
        let connection = Arc::clone(&connection);
        let server = Arc::clone(server);
        async move {
            if let PlayerConnection::Java(java) = &*connection {
                java.listener(reader, server).await;
            }
        }
    });

    server.queue_player_join(Arc::clone(&player));
    (player, client_log)
}
