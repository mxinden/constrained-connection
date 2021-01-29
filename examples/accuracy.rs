use constrained_connection::Connection;
use futures::task::Spawn;
use futures::{AsyncReadExt, AsyncWriteExt};
use std::time::Duration;
use std::time::Instant;
use futures::future::FutureExt;

fn main() {
    println!("Name\t\t\t\tBandwidth\tRTT\t\tPayload\t\tDuration\tAcurracy");

    for (name, f) in constrained_connection::samples::iter_all() {
        run_sample(name.as_str(), f);
    }
}

fn run_sample(name: &str, f: fn() -> (u64, Duration, (Connection, Connection))) {
    let (bandwidth, rtt, (mut a, mut b)) = f();

    let msg = vec![0; 1 * 1024 * 1024];
    let msg_clone = msg.clone();

    let start = Instant::now();

    let mut pool = futures::executor::LocalPool::new();

    pool.spawner()
        .spawn_obj(
            async move {
                a.write_all(&msg_clone).await.unwrap();
            }
            .boxed()
            .into(),
        )
        .unwrap();

    pool.run_until(async {
        let mut received_msg = Vec::new();
        b.read_to_end(&mut received_msg).await.unwrap();

        assert_eq!(msg, received_msg);
    });

    let duration = start.elapsed();

    println!(
        "{}\t{} KiB/s\t{:.5} s\t{} KiB\t{:.2} s\t\t{:.2} %",
        name,
        bandwidth / 1024,
        rtt.as_secs_f64(),
        msg.len() / 1024 * 8,
        duration.as_secs_f64(),
        (bandwidth as f64 * (duration.as_secs_f64() - rtt.as_secs_f64() / 2.0))
            / (msg.len() * 8) as f64
    );
}
