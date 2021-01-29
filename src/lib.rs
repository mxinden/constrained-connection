use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::future::FutureExt;
use futures::ready;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::{AsyncRead, AsyncWrite};
use futures_timer::Delay;
use std::io::Result;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

pub struct Connection {
    sender: UnboundedSender<Item>,

    receiver: UnboundedReceiver<Item>,
    next_item: Option<Item>,

    shared_send: Arc<Mutex<Shared>>,
    shared_receive: Arc<Mutex<Shared>>,

    delay: Duration,
    capacity: usize,
}

impl Connection {
    /// Create a new [`Connection`] pair.
    ///
    /// `bandwidth` being the bandwidth in bits per second.
    ///
    /// `rtt` being the round trip time.
    pub fn new(bandwidth_bits_per_second: u64, rtt: Duration) -> (Connection, Connection) {
        let single_direction_capacity_bytes =
            single_direction_capacity_bytes(bandwidth_bits_per_second, rtt);
        assert!(single_direction_capacity_bytes > 0);
        let single_direction_delay = rtt / 2;

        let (a_to_b_sender, a_to_b_receiver) = unbounded();
        let (b_to_a_sender, b_to_a_receiver) = unbounded();

        let a_to_b_shared = Arc::new(Mutex::new(Default::default()));
        let b_to_a_shared = Arc::new(Mutex::new(Default::default()));

        let a = Connection {
            sender: a_to_b_sender,
            receiver: b_to_a_receiver,
            next_item: None,

            shared_send: a_to_b_shared.clone(),
            shared_receive: b_to_a_shared.clone(),

            delay: single_direction_delay,
            capacity: single_direction_capacity_bytes,
        };

        let b = Connection {
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
}

struct Item {
    data: Vec<u8>,
    delay: Delay,
}

impl Unpin for Connection {}

impl AsyncRead for Connection {
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

impl AsyncWrite for Connection {
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
            .unwrap();

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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::Spawn;
    use futures::{AsyncReadExt, AsyncWriteExt};
    use quickcheck::{Gen, QuickCheck, TestResult};
    use std::time::Instant;

    #[ignore]
    #[test]
    fn timing() {
        let bandwidth = 6 * 1024 * 1024;
        let delay = Duration::from_millis(900);
        let msg = vec![0; 10 * 1024 * 1024];
        let msg_clone = msg.clone();
        let start = Instant::now();

        let (mut a, mut b) = Connection::new(bandwidth, delay);

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
            "bandwidth {} KiB/s, delay {}s duration {}s, msg len {} KiB, percentage {}",
            bandwidth / 1024,
            delay.as_secs_f64(),
            duration.as_secs_f64(),
            msg.len() / 1024 * 8,
            (bandwidth as f64 * (duration.as_secs_f64() - delay.as_secs_f64()))
                / (msg.len() * 8) as f64
        );
    }

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

            let (mut a, mut b) = Connection::new(bandwidth as u64, rtt);

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
