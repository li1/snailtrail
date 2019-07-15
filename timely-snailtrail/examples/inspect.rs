#[macro_use]
extern crate log;

use timely_snailtrail::{pag, Config};
use timely_snailtrail::pag::PagEdge;

use timely::dataflow::ProbeHandle;
use timely::dataflow::operators::probe::Probe;
use timely::dataflow::operators::capture::EventReader;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::generic::OutputHandle;
use timely::dataflow::operators::generic::operator::Operator;

use std::time::Duration;
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use std::time::Instant;

use logformat::pair::Pair;

use tdiag_connect::receive as connect;
use tdiag_connect::receive::ReplaySource;

fn main() {
    env_logger::init();

    let worker_peers = std::env::args().nth(1).unwrap().parse::<usize>().unwrap();
    let source_peers = std::env::args().nth(2).unwrap().parse::<usize>().unwrap();
    let throttle = std::env::args().nth(3).unwrap().parse::<u64>().unwrap();
    let from_file = if let Some(_) = std::env::args().nth(4) {
        true
    } else {
        false
    };
    let config = Config {
        timely_args: vec!["-w".to_string(), worker_peers.to_string()],
        worker_peers,
        source_peers,
        from_file,
        throttle,
    };

    inspector(config);
}

fn inspector(config: Config) {
    // creates one socket per worker in the computation we're examining
    let replay_source = if config.from_file {
        let files = (0 .. config.source_peers)
            .map(|idx| format!("{}.dump", idx))
            .map(|path| Some(PathBuf::from(path)))
            .collect::<Vec<_>>();

        ReplaySource::Files(Arc::new(Mutex::new(files)))
    } else {
        let sockets = connect::open_sockets("127.0.0.1".parse().expect("couldn't parse IP"), 8000, config.source_peers).expect("couldn't open sockets");
        ReplaySource::Tcp(Arc::new(Mutex::new(sockets)))
    };

    timely::execute_from_args(config.timely_args.clone().into_iter(), move |worker| {
        let index = worker.index();
        if index == 0 {info!("{:?}", &config);}

        // read replayers from file (offline) or TCP stream (online)
        let readers: Vec<EventReader<_, timely_adapter::connect::CompEvent, _>> =
            connect::make_readers(replay_source.clone(), worker.index(), worker.peers()).expect("couldn't create readers");

        let probe: ProbeHandle<Pair<u64, Duration>> = worker.dataflow(|scope| {
            pag::create_pag(scope, readers, index, config.throttle)
                .unary_frontier(Pipeline, "TheVoid", move |_cap, _info| {
                    let mut t0 = Instant::now();
                    let mut buffer = Vec::new();

                    move |input, output: &mut OutputHandle<_, (PagEdge, Pair<u64, Duration>, isize), _>| {
                        let mut count = 0;
                        let mut received_input = false;

                        input.for_each(|cap, data| {
                            data.swap(&mut buffer);
                            count += buffer.len();
                            received_input = !buffer.is_empty();
                            output.session(&cap).give_vec(&mut buffer);
                        });

                        if received_input {
                            println!("{}|{}|{}|{}", index, input.frontier.frontier()[0].first, t0.elapsed().as_micros(), count);
                            t0 = Instant::now();
                        }
                    }
                })
                .probe()
        });

        // let mut timer = std::time::Instant::now();
        // while probe.less_equal(&Pair::new(2, std::time::Duration::from_secs(0))) {
        while !probe.done() {
            // probe.with_frontier(|f| {
            //     let f = f.to_vec();
            //         println!("w{} frontier: {:?} | took {:?}ms", index, f, timer.elapsed().as_millis());
            //         timer = std::time::Instant::now();
            // });
            worker.step();
        };

        info!("w{} done", index);
        // stall application
        // use std::io::stdin;
        // stdin().read_line(&mut String::new()).unwrap();
    })
    .unwrap();
}


        //         // .filter(|x| x.source.epoch > x.destination.epoch)
        //         // .inspect(move |(x, t, diff)| println!("w{} | {} -> {}: -> w{}\t {:?}", x.source.worker_id, x.source.seq_no, x.destination.seq_no, x.destination.worker_id, x.edge_type))
