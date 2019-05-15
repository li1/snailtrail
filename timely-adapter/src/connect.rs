//! Helpers to obtain traces suitable for PAG construction from timely / differential.
//!
//! To log a computation, use `register_logger` at its beginning. If
//! `SNAILTRAIL_ADDR=<IP>:<Port>` is set as env variable, the computation will be logged
//! online via TCP. Regardless of offline/online logging, `register_logger`'s contract has
//! to be upheld. See `log_pag`'s docstring for more information.
//!
//! To obtain the logged computation's events, use `make_replayers` and `replay_into` them
//! into a dataflow of your choice. If you pass `Some(sockets)` created with `open_sockets`
//! to `make_replayer`, an online connection will be used as the event source.

use std::{
    error::Error,
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use timely::{
    communication::allocator::Generic,
    dataflow::operators::capture::{event::EventPusher, Event, EventReader, EventWriter},
    logging::{TimelyEvent, WorkerIdentifier},
    worker::Worker,
};

use logformat::pair::Pair;

use TimelyEvent::{Channels, Messages, Operates, Progress, Schedule, Text};

/// A replayer that reads data to be streamed into timely
pub type Replayer<R> = EventReader<Pair<u64, Duration>, (Duration, WorkerIdentifier, TimelyEvent), R>;

/// Types of replayer to be created from `make_replayers`
pub enum ReplayerType {
    /// a TCP-backed online replayer
    Tcp(TcpStream),
    /// a file-backed offline replayer
    File(File),
}

impl Read for ReplayerType {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ReplayerType::Tcp(x) => x.read(buf),
            ReplayerType::File(x) => x.read(buf),
        }
    }
}

/// Listens on 127.0.0.1:8000 and opens `source_peers` sockets from the
/// computations we're examining (one socket for every worker on the
/// examined computation).
/// Adapted from TimelyDataflow examples / https://github.com/utaal/timely-viz
pub fn open_sockets(source_peers: usize) -> Arc<Mutex<Vec<Option<TcpStream>>>> {
    let listener = TcpListener::bind("127.0.0.1:8000").unwrap();
    Arc::new(Mutex::new(
        (0..source_peers)
            .map(|_| Some(listener.incoming().next().unwrap().unwrap()))
            .collect::<Vec<_>>(),
    ))
}

// @TODO Currently, the computation runs best with worker_peers == source_peers.
// It might be worth investigating how replaying could benefit from worker_peers > source_peers.
// @TODO TCP stream optimization might be necessary (e.g. smarter consumption of batches)
/// Construct replayers that read data from sockets or file and can stream it into
/// timely dataflow. If `Some(sockets)` is passed, the replayers assume an online setting.
/// Adapted from TimelyDataflow examples / https://github.com/utaal/timely-viz
pub fn make_replayers(
    worker_index: usize,
    worker_peers: usize,
    source_peers: usize,
    sockets: Option<Arc<Mutex<Vec<Option<TcpStream>>>>>,
) -> Vec<Replayer<ReplayerType>> {
    assert!(source_peers == worker_peers, "fix exchange after logrecord creation for this to work");

    info!(
        "Creating replayers\tworker index: {}, worker peers: {}, source peers: {}, online: {}",
        worker_index,
        worker_peers,
        source_peers,
        sockets.is_some()
    );

    if let Some(sockets) = sockets {
        // online
        sockets
            .lock()
            .unwrap()
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| *i % worker_peers == worker_index)
            .map(move |(_, s)| s.take().unwrap())
            .map(|r| EventReader::new(ReplayerType::Tcp(r)))
            .collect::<Vec<_>>()
    } else {
        // from file
        (0..source_peers)
            .filter(|i| i % worker_peers == worker_index)
            .map(|i| {
                let name = format!("{:?}.dump", i);
                let path = Path::new(&name);

                match File::open(&path) {
                    Err(why) => panic!("couldn't open. {}", why.description()),
                    Ok(file) => file,
                }
            })
            .map(|f| EventReader::new(ReplayerType::File(f)))
            .collect::<Vec<_>>()
    }
}

/// Logging of events to TCP or file.
/// For live analysis, provide `SNAILTRAIL_ADDR` as env variable.
/// Else, the computation will log to file for later replay.
pub fn register_logger(worker: &mut Worker<Generic>) {
    if let Ok(addr) = ::std::env::var("SNAILTRAIL_ADDR") {
        if let Ok(stream) = TcpStream::connect(&addr) {
            // SnailTrail should be able to keep up with an online computation.
            // If batch sizes are too large, they should be buffered. Blocking the
            // TCP connection is not an option as it slows down the main computation.
            stream
                .set_nonblocking(true)
                .expect("set_nonblocking call failed");

            let writer = EventWriter::new(stream);
            unsafe { log_pag(worker, writer); }
        } else {
            panic!("Could not connect logging stream to: {:?}", addr);
        }
    } else {
        let name = format!("{:?}.dump", worker.index());
        let path = Path::new(&name);
        let file = match File::create(&path) {
            Err(why) => panic!("couldn't create {}: {}", path.display(), why.description()),
            Ok(file) => file,
        };
        let writer = EventWriter::new(file);
        unsafe { log_pag(worker, writer); }
    }
}

// @TODO: further describe contract between log_pag and SnailTrail; mark as unsafe
// @TODO: for triangles query with round size == 1, the computation is slowed down by TCP.
//        A reason for this might be the overhead in creating TCP packets, so it might be
//        worthwhile investigating the reintroduction of batching for very small computation
//        rounds.
/// Registers a `TimelyEvent` logger which outputs relevant log events for PAG construction.
/// 1. Only relevant events are written to `writer`.
/// 2. Using `Text` events as markers, logged events are written out at one time per epoch.
/// 3. Dataflow setup is collapsed into t=(0, 0ns) so that peel_operators is more efficient.
/// 4. If the computation is bounded, capabilities will be dropped correctly at the end of
///    computation.
///
/// This function is marked `unsafe` as it relies on an implicit contract with the
/// logged computation:
/// 1. After `advance_to(0)`, the beginning of computation should be logged:
///    `"[st] begin computation at epoch: {:?}"`
/// 2. After each epoch's `worker.step()`, the epoch progress should be logged:
///    `"[st] closed times before: {:?}"`
/// 3. (optional) After the last round, the end of the computation should be logged:
///    `"[st] computation done"`
/// Failing to do so might have unexpected effects on the PAG creation.
unsafe fn log_pag<W: 'static + Write>(
    worker: &mut Worker<Generic>,
    mut writer: EventWriter<Pair<u64, Duration>, (Duration, usize, TimelyEvent), W>,
) {
    // first real frontier, used for setting up the computation
    // (`Operates` et al.)
    let mut new_frontier = Pair::new(0, Default::default());

    // System time is only allowed to progress once until a
    // progress message is sent.
    let mut allow_frontier_update = true;

    // initialized to 0, which we'll drop as soon as the
    // computation makes progress
    let mut curr_frontier = Pair::new(0, Default::default());

    // buffer of relevant events for a batch. As a batch only ever belongs
    // to a single epoch (epoch markers only appear at the beginning of a batch),
    // we don't have to keep track of times for batch elements.
    let mut buffer = Vec::new();

    // 1st: marker that computation has ended
    // 2nd: capabilities have been dropped; no further messages
    //      should get sent.
    let mut wrap_up = (false, false);

    // debug logs
    let index = worker.index();
    let mut total = 0;

    // If a computation's epoch size exceeds `MAX_FUEL`, events are batched
    // into multiple processing times within that epoch.
    const MAX_FUEL: usize = 512;
    let mut fuel = MAX_FUEL;

    worker
        .log_register()
        .insert::<TimelyEvent, _>("timely", move |time, data| {
            for tuple in data.drain(..) {
                // println!("time & event, {:?} \t {:?}", time, tuple);
                match &tuple.2 {
                    Progress(_) | Messages(_) | Schedule(_) => {
                        if !wrap_up.1 {
                            fuel -= 1;
                        }

                        // Advance system time if it hasn't yet been advanced
                        // for the current progress batch.
                        if allow_frontier_update {
                            new_frontier.second = tuple.0;
                            allow_frontier_update = false;
                        }

                        buffer.push(tuple);
                    }
                    Operates(_) | Channels(_) => {
                        if !wrap_up.1 {
                            fuel -= 1;
                        }

                        // all operates events should happen in the initialization epoch,
                        // i.e., before any Text event epoch markers have been observed
                        assert!(new_frontier == Pair::new(0, Default::default()));
                        assert!(new_frontier == curr_frontier);

                        // the tuple is provided to the computation at a 0ns data timestamp,
                        // and (0, 0ns) time (see below).
                        buffer.push((curr_frontier.second, tuple.1, tuple.2));
                    }
                    // Text events mark epochs in the computation. They are always the first
                    // in their batch, so a single batch is never split into multiple epochs.
                    Text(e) => {
                        trace!("text event: {}", e);
                        if e.starts_with("[st] computation done") {
                            wrap_up = (true, false);
                        } else {
                            // advance epoch time
                            new_frontier.first += 1;
                        }
                    }
                    _ => {}
                }

                if !wrap_up.1 {
                    // Advance frontier if (a) epoch has advanced or (b) fuel has run out.
                    // Timestamps strictly increase as event and processing time are related.
                    if (new_frontier.first > curr_frontier.first) || fuel == 0 {
                        info!(
                            "w{} progress {:?} -> {:?} | fuel: {}",
                            index, curr_frontier, new_frontier, fuel
                        );

                        fuel = MAX_FUEL;

                        writer.push(Event::Progress(vec![
                            (new_frontier.clone(), 1),
                            (curr_frontier.clone(), -1),
                        ]));
                        curr_frontier = new_frontier.clone();
                        allow_frontier_update = true;

                        // flush out remaining elements
                        if buffer.len() > 0 {
                            trace!("w{} flush@{:?}: count: {} | total: {}", index, curr_frontier, buffer.len(), total);
                            total += buffer.len();
                            writer.push(Event::Messages(curr_frontier.clone(), std::mem::replace(&mut buffer, Vec::new())));
                        }
                    }
                }
            }

            // @TODO: do I want to write out even if fuel hasn't been depleted?
            // if buffer.len() > 0 && !wrap_up.1 {
            //     // trace!(" w{} send @epoch{:?}: count: {} | total: {}", index, curr_frontier, buffer.len(), total); trace!("first buffer element: {:?}", buffer[0]);
            //     total += buffer.len();
            //     writer.push(Event::Messages(curr_frontier.clone(), std::mem::replace(&mut buffer, Vec::new())));
            // }

            if wrap_up.0 && !wrap_up.1 {
                info!(
                    "w{}@ep{:?}: timely logging wrapping up | total: {}",
                    index, curr_frontier, total
                );

                // write out remaining messages
                if buffer.len() > 0 {
                    writer.push(Event::Messages(curr_frontier.clone(), buffer.clone()));
                }

                // free capabilities
                writer.push(Event::Progress(vec![(curr_frontier.clone(), -1)]));

                // 1st to false so that marker isn't processed multiple times.
                // 2nd to true so that no further `Event::Messages` will be sent.
                wrap_up = (false, true);
            }
        });
}
