use futures::future::FutureExt;
use futures::ready;
use futures::{AsyncRead, AsyncWrite};
use futures_timer::Delay;
use ringbuf::{Consumer, Producer, RingBuffer};
use std::collections::VecDeque;
use std::io::Result;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

pub struct Stream {
    // TODO: Consider just using a vecdequeue
    sender: Producer<u8>,
    receiver: Consumer<u8>,

    shared_send: Arc<Mutex<Shared>>,
    shared_receive: Arc<Mutex<Shared>>,

    next_timer: Option<Timer>,
    delay: Duration,
}

impl Drop for Stream {
    fn drop(&mut self) {
        println!("drop called");
        self.shared_receive.lock().unwrap().reader_dropped = true;
        self.shared_send.lock().unwrap().writer_dropped = true;

        if let Some(waker) = self.shared_send.lock().unwrap().waker_read.take() {
            waker.wake();
        }
        if let Some(waker) = self.shared_receive.lock().unwrap().waker_write.take() {
            waker.wake();
        }
    }
}

impl Unpin for Stream {}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &mut [u8],
    ) -> Poll<Result<usize>> {
        println!("called poll_read");
        let timer = match self.next_timer.as_mut() {
            Some(timer) => timer,
            None => {
                println!("no next timer");
                let next = self.shared_receive.lock().unwrap().timers.pop_front();
                match next {
                    Some(timer) => {
                        println!("timer in queue");
                        self.next_timer = Some(timer);
                        self.next_timer.as_mut().unwrap()
                    }
                    None => {
                        println!("no timer in queue");
                        if self.shared_receive.lock().unwrap().writer_dropped {
                            // TODO: Is this the right behaviour?
                            return Poll::Ready(Ok(0));
                        } else {
                            self.shared_receive.lock().unwrap().waker_read =
                                Some(cx.waker().clone());
                            return Poll::Pending;
                        }
                    }
                }
            }
        };

        ready!(timer.delay.poll_unpin(cx));

        let timer = self.next_timer.take().unwrap();

        assert!(buf.len() > timer.num_bytes);

        let bytes_read = self
            .receiver
            .write_into(&mut buf, Some(timer.num_bytes))
            .unwrap();
        assert_eq!(bytes_read, timer.num_bytes);

        if let Some(waker) = self.shared_receive.lock().unwrap().waker_write.take() {
            println!("woke writer");
            waker.wake();
        }

        Poll::Ready(Ok(bytes_read))
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        println!("called poll_write");

        if self.shared_send.lock().unwrap().reader_dropped {
            return Poll::Ready(Ok(0));
        }

        let bytes_written = self.sender.push_slice(buf);
        println!("bytes written {}", bytes_written);
        if bytes_written > 0 {
            self.shared_send.lock().unwrap().timers.push_back(Timer {
                delay: Delay::new(self.delay),
                num_bytes: bytes_written,
            });

            if let Some(waker) = self.shared_send.lock().unwrap().waker_read.take() {
                println!("woke reader");
                waker.wake();
            }

            Poll::Ready(Ok(bytes_written))
        } else {
            self.shared_send.lock().unwrap().waker_write = Some(cx.waker().clone());
            println!("saving write waker to be woken up later");
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.sender.is_empty() {
            return Poll::Ready(Ok(()));
        }

        self.shared_send.lock().unwrap().waker_write = Some(cx.waker().clone());

        Poll::Pending
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        todo!()
    }
}

#[derive(Default)]
struct Shared {
    // TODO: Could as well use a smallvec
    timers: VecDeque<Timer>,

    waker_read: Option<Waker>,
    waker_write: Option<Waker>,

    reader_dropped: bool,
    writer_dropped: bool,
}

struct Timer {
    delay: Delay,
    num_bytes: usize,
}

// TODO: Document whether delay is the delay for both directions or just a
// single?
pub fn new(bandwidth: u64, delay: Duration) -> (Stream, Stream) {
    println!("called new with {}, {}", bandwidth, delay.as_millis());

    let bandwidth_delay_product: u128 = bandwidth as u128 * delay.as_millis() / 1000u128;
    assert!(bandwidth_delay_product > 0);

    let (a_to_b_sender, a_to_b_receiver) =
        RingBuffer::<u8>::new(bandwidth_delay_product as usize).split();
    let (b_to_a_sender, b_to_a_receiver) =
        RingBuffer::<u8>::new(bandwidth_delay_product as usize).split();

    println!("created ring buffers");

    let a_to_b_shared = Arc::new(Mutex::new(Default::default()));
    let b_to_a_shared = Arc::new(Mutex::new(Default::default()));

    let a = Stream {
        sender: a_to_b_sender,
        receiver: b_to_a_receiver,

        shared_send: a_to_b_shared.clone(),
        shared_receive: b_to_a_shared.clone(),

        next_timer: None,
        delay,
    };

    let b = Stream {
        sender: b_to_a_sender,
        receiver: a_to_b_receiver,

        shared_send: b_to_a_shared.clone(),
        shared_receive: a_to_b_shared.clone(),

        next_timer: None,
        delay,
    };

    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::Spawn;
    use futures::{AsyncReadExt, AsyncWriteExt};
    use quickcheck::{Gen, QuickCheck, TestResult};

    #[test]
    fn quickcheck() {
        fn prop(msg: Vec<u8>, bandwidth: u32, delay: u16) -> TestResult {
            let bandwidth = bandwidth % 1024 * 1024 * 1024; // No more than 1 GiB.
            let delay = delay % 1_000; // No more than 1 sec.

            if bandwidth == 0 || delay == 0 || msg.is_empty() {
                return TestResult::discard();
            }

            println!(
                "msg len {}, bandwidth {}, delay {}",
                msg.len(),
                bandwidth,
                delay
            );

            let (mut a, mut b) = new(bandwidth as u64, Duration::from_millis(delay.into()));

            println!("created a and b");

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

            pool.run_until(async move {
                let mut received_msg = Vec::new();
                b.read_to_end(&mut received_msg).await.unwrap();

                assert_eq!(msg, received_msg);
            });

            TestResult::passed()
        }

        QuickCheck::new()
            .gen(Gen::new(1_000_000))
            .quickcheck(prop as fn(_, _, _) -> _)
    }
}
