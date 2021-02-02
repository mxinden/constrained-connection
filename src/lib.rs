//! Simulate constrained network connections.
//!
//! Can be used to benchmark networking logic build on top of a stream oriented
//! connection (e.g. TCP). Create a connection by specifying its bandwidth as
//! well as its round trip time (delay). The connection will delay each bytes
//! chunk by the configured delay and allow at most [bandwidth-delay
//! product](https://en.wikipedia.org/wiki/Bandwidth-delay_product) number of
//! bytes on the *wire* enforcing backpressure.
//!
//! ```
//! # use constrained_connection::{Endpoint, new_constrained_connection};
//! # use futures::task::Spawn;
//! # use futures::{AsyncReadExt, AsyncWriteExt};
//! # use std::time::Duration;
//! # use std::time::Instant;
//! # use futures::future::FutureExt;
//! let msg = vec![0; 10 * 1024 * 1024];
//! let msg_clone = msg.clone();
//! let start = Instant::now();
//! let mut pool = futures::executor::LocalPool::new();
//!
//! let bandwidth = 1_000_000_000;
//! let rtt = Duration::from_micros(100);
//! let (mut a, mut b) = new_constrained_connection(bandwidth, rtt);
//!
//! pool.spawner().spawn_obj(async move {
//!     a.write_all(&msg_clone).await.unwrap();
//! }.boxed().into()).unwrap();
//!
//! pool.run_until(async {
//!     let mut received_msg = Vec::new();
//!     b.read_to_end(&mut received_msg).await.unwrap();
//!
//!     assert_eq!(msg, received_msg);
//! });
//!
//! let duration = start.elapsed();
//!
//! println!(
//!     "Bandwidth {} KiB/s, RTT {:.5} s, Payload length {} KiB, duration {:.5} s",
//!     bandwidth / 1024, rtt.as_secs_f64(), msg.len() / 1024, duration.as_secs_f64(),
//! );
//! ```
//!
//! For now, as the library is not properly optimized, you can not simulate high
//! speed networks. Execute the `examples/accuracy.rs` binary for details.
//!
//! ```bash
//! $ cargo run --example accuracy --release
//!
//! Name                            Bandwidth       RTT             Payload         Duration        Acurracy
//! Satellite Network               500 KiB/s       0.90000 s       10240 KiB       164.46 s        1.00 %
//! Residential DSL                 1953 KiB/s      0.05000 s       10240 KiB       42.78 s         1.02 %
//! Mobile HSDPA                    5859 KiB/s      0.10000 s       10240 KiB       14.17 s         1.01 %
//! Residential ADSL2+              19531 KiB/s     0.05000 s       10240 KiB       4.29 s          1.02 %
//! Residential Cable Internet      195312 KiB/s    0.02000 s       10240 KiB       0.46 s          1.08 %
//! GBit LAN                        976562 KiB/s    0.00010 s       10240 KiB       0.28 s          3.34 %
//! High Speed Terrestiral Net      976562 KiB/s    0.00100 s       10240 KiB       0.14 s          1.63 %
//! Ultra High Speed LAN            97656250 KiB/s  0.00003 s       10240 KiB       0.02 s          18.87 %
//! Unconstrained                   18014398509481983 KiB/s 0.00000 s       10240 KiB       0.03 s          6378832541.51 %
//! ```

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::future::FutureExt;
use futures::ready;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::{AsyncRead, AsyncWrite};
use futures_timer::Delay;
use std::io::{Error, ErrorKind, Result};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

pub struct Endpoint {
    sender: UnboundedSender<Item>,

    receiver: UnboundedReceiver<Item>,
    next_item: Option<Item>,

    shared_send: Arc<Mutex<Shared>>,
    shared_receive: Arc<Mutex<Shared>>,

    delay: Duration,
    capacity: usize,
}

/// Create a new [`Endpoint`] pair.
///
/// `bandwidth` being the bandwidth in bits per second.
///
/// `rtt` being the round trip time.
pub fn new_constrained_connection(
    bandwidth_bits_per_second: u64,
    rtt: Duration,
) -> (Endpoint, Endpoint) {
    let single_direction_capacity_bytes =
        single_direction_capacity_bytes(bandwidth_bits_per_second, rtt);
    assert!(single_direction_capacity_bytes > 0);
    let single_direction_delay = rtt / 2;

    let (a_to_b_sender, a_to_b_receiver) = unbounded();
    let (b_to_a_sender, b_to_a_receiver) = unbounded();

    let a_to_b_shared = Arc::new(Mutex::new(Default::default()));
    let b_to_a_shared = Arc::new(Mutex::new(Default::default()));

    let a = Endpoint {
        sender: a_to_b_sender,
        receiver: b_to_a_receiver,
        next_item: None,

        shared_send: a_to_b_shared.clone(),
        shared_receive: b_to_a_shared.clone(),

        delay: single_direction_delay,
        capacity: single_direction_capacity_bytes,
    };

    let b = Endpoint {
        sender: b_to_a_sender,
        receiver: a_to_b_receiver,
        next_item: None,

        shared_send: b_to_a_shared,
        shared_receive: a_to_b_shared,

        delay: single_direction_delay,
        capacity: single_direction_capacity_bytes,
    };

    (a, b)
}

pub fn new_unconstrained_connection() -> (Endpoint, Endpoint) {
    let (a_to_b_sender, a_to_b_receiver) = unbounded();
    let (b_to_a_sender, b_to_a_receiver) = unbounded();

    let a_to_b_shared = Arc::new(Mutex::new(Default::default()));
    let b_to_a_shared = Arc::new(Mutex::new(Default::default()));

    let a = Endpoint {
        sender: a_to_b_sender,
        receiver: b_to_a_receiver,
        next_item: None,

        shared_send: a_to_b_shared.clone(),
        shared_receive: b_to_a_shared.clone(),

        delay: Duration::from_secs(0),
        capacity: std::usize::MAX,
    };

    let b = Endpoint {
        sender: b_to_a_sender,
        receiver: a_to_b_receiver,
        next_item: None,

        shared_send: b_to_a_shared,
        shared_receive: a_to_b_shared,

        delay: Duration::from_secs(0),
        capacity: std::usize::MAX,
    };

    (a, b)
}

struct Item {
    data: Vec<u8>,
    delay: Delay,
}

impl Unpin for Endpoint {}

impl AsyncRead for Endpoint {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize>> {
        let item = match self.next_item.as_mut() {
            Some(item) => item,
            None => match ready!(self.receiver.poll_next_unpin(cx)) {
                Some(item) => {
                    self.next_item = Some(item);
                    self.next_item.as_mut().unwrap()
                }
                None => {
                    return Poll::Ready(Ok(0));
                }
            },
        };

        ready!(item.delay.poll_unpin(cx));

        let n = std::cmp::min(buf.len(), item.data.len());

        buf[0..n].copy_from_slice(&item.data[0..n]);

        if n < item.data.len() {
            item.data = item.data.split_off(n);
        } else {
            self.next_item.take().unwrap();
        }

        let mut shared = self.shared_receive.lock().unwrap();
        if let Some(waker) = shared.waker_write.take() {
            waker.wake();
        }

        debug_assert!(shared.size >= n);
        shared.size -= n;

        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for Endpoint {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        let mut shared = self.shared_send.lock().unwrap();
        let n = std::cmp::min(self.capacity - shared.size, buf.len());
        if n == 0 {
            shared.waker_write = Some(cx.waker().clone());
            return Poll::Pending;
        }

        self.sender
            .unbounded_send(Item {
                data: buf[0..n].to_vec(),
                delay: Delay::new(self.delay),
            })
            .map_err(|e| Error::new(ErrorKind::ConnectionAborted, e))?;

        shared.size += n;

        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.sender.poll_flush_unpin(cx)).unwrap();
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.sender.poll_close_unpin(cx)).unwrap();
        Poll::Ready(Ok(()))
    }
}

#[derive(Default)]
struct Shared {
    waker_write: Option<Waker>,
    size: usize,
}

fn single_direction_capacity_bytes(bandwidth_bits_per_second: u64, rtt: Duration) -> usize {
    let bandwidth_delay_product: u128 =
        bandwidth_bits_per_second as u128 * rtt.as_micros() / 1_000_000u128 / 8;
    (bandwidth_delay_product / 2) as usize
}

/// Samples based on numbers from
/// https://en.wikipedia.org/wiki/Bandwidth-delay_product#examples
pub mod samples {
    use super::{new_constrained_connection, new_unconstrained_connection, Endpoint};
    use std::time::Duration;

    pub fn satellite_network() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 512_000;
        let rtt = Duration::from_millis(900);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn residential_dsl() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 2_000_000;
        let rtt = Duration::from_millis(50);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn mobile_hsdpa() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 6_000_000;
        let rtt = Duration::from_millis(100);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn residential_adsl2() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 20_000_000;
        let rtt = Duration::from_millis(50);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn residential_cable_internet() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 200_000_000;
        let rtt = Duration::from_millis(20);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn gbit_lan() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 1_000_000_000;
        let rtt = Duration::from_micros(100);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn high_speed_terrestiral_network() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 1_000_000_000;
        let rtt = Duration::from_millis(1);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn ultra_high_speed_lan() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = 100_000_000_000;
        let rtt = Duration::from_micros(30);
        let connections = new_constrained_connection(bandwidth, rtt);

        (bandwidth, rtt, connections)
    }

    pub fn unconstrained() -> (u64, Duration, (Endpoint, Endpoint)) {
        let bandwidth = std::u64::MAX;
        let rtt = Duration::from_micros(0);
        let connections = new_unconstrained_connection();

        (bandwidth, rtt, connections)
    }

    pub fn iter_all(
    ) -> impl Iterator<Item = (String, fn() -> (u64, Duration, (Endpoint, Endpoint)))> {
        vec![
            (
                "Satellite Network         ".to_string(),
                satellite_network as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "Residential DSL           ".to_string(),
                residential_dsl as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "Mobile HSDPA              ".to_string(),
                mobile_hsdpa as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "Residential ADSL2+        ".to_string(),
                residential_adsl2 as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "Residential Cable Internet".to_string(),
                residential_cable_internet as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "GBit LAN                 ".to_string(),
                gbit_lan as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "High Speed Terrestiral Net".to_string(),
                high_speed_terrestiral_network as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "Ultra High Speed LAN     ".to_string(),
                ultra_high_speed_lan as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
            (
                "Unconstrained            ".to_string(),
                unconstrained as fn() -> (u64, Duration, (Endpoint, Endpoint)),
            ),
        ]
        .into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::Spawn;
    use futures::{AsyncReadExt, AsyncWriteExt};
    use quickcheck::{Gen, QuickCheck, TestResult};
    use std::time::Instant;

    #[test]
    fn quickcheck() {
        fn prop(msg: Vec<u8>, bandwidth: u32, rtt: u64) -> TestResult {
            let start = Instant::now();

            let bandwidth = bandwidth % 1024 * 1024 * 1024; // No more than 1 GiB.
            let rtt = Duration::from_micros(rtt % Duration::from_secs(1).as_millis() as u64); // No more than 1 second.

            if bandwidth == 0
                || rtt == Duration::from_micros(1)
                || msg.is_empty()
                || single_direction_capacity_bytes(bandwidth as u64, rtt) < 1
            {
                return TestResult::discard();
            }

            let (mut a, mut b) = new_constrained_connection(bandwidth as u64, rtt);

            let mut pool = futures::executor::LocalPool::new();

            let msg_clone = msg.clone();
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
                "bandwidth {} KiB/s, rtt {}s duration {}s, msg len {} KiB, percentage {}",
                bandwidth / 1024,
                rtt.as_secs_f64(),
                duration.as_secs_f64(),
                msg.len() / 1024 * 8,
                (bandwidth as f64 * (duration.as_secs_f64() - rtt.as_secs_f64() / 2.0))
                    / (msg.len() * 8) as f64
            );

            TestResult::passed()
        }

        QuickCheck::new()
            .gen(Gen::new(1_000_000))
            .quickcheck(prop as fn(_, _, _) -> _)
    }
}
