# constrained-connection

Simulate constrained network connections.

Can be used to benchmark networking logic build on top of a stream oriented
connection (e.g. TCP). Create a connection by specifying its bandwidth as
well as its round trip time (delay). The connection will delay each bytes
chunk by the configured delay and allow at most [bandwidth-delay
product](https://en.wikipedia.org/wiki/Bandwidth-delay_product) number of
bytes on the *wire* enforcing backpressure.

```rust
let msg = vec![0; 10 * 1024 * 1024];
let msg_clone = msg.clone();
let start = Instant::now();
let mut pool = futures::executor::LocalPool::new();

let bandwidth = 1_000_000_000;
let rtt = Duration::from_micros(100);
let (mut a, mut b) = Connection::new_constrained(bandwidth, rtt);

pool.spawner().spawn_obj(async move {
    a.write_all(&msg_clone).await.unwrap();
}.boxed().into()).unwrap();

pool.run_until(async {
    let mut received_msg = Vec::new();
    b.read_to_end(&mut received_msg).await.unwrap();

    assert_eq!(msg, received_msg);
});

let duration = start.elapsed();

println!(
    "Bandwidth {} KiB/s, RTT {:.5} s, Payload length {} KiB, duration {:.5} s",
    bandwidth / 1024, rtt.as_secs_f64(), msg.len() / 1024, duration.as_secs_f64(),
);
```

For now, as the library is not properly optimized, you can not simulate high
speed networks. Execute the `examples/accuracy.rs` binary for details.

```bash
$ cargo run --example accuracy --release

Name                            Bandwidth       RTT             Payload         Duration        Acurracy
Satellite Network               500 KiB/s       0.90000 s       10240 KiB       164.49 s        1.00 %
Residential DSL                 1953 KiB/s      0.05000 s       10240 KiB       42.97 s         1.02 %
Mobile HSDPA                    5859 KiB/s      0.10000 s       10240 KiB       14.19 s         1.01 %
Residential ADSL2+              19531 KiB/s     0.05000 s       10240 KiB       4.33 s          1.03 %
Residential Cable Internet      195312 KiB/s    0.02000 s       10240 KiB       0.46 s          1.07 %
GBit LAN                        976562 KiB/s    0.00010 s       10240 KiB       0.26 s          3.16 %
High Speed Terrestiral Net      976562 KiB/s    0.00100 s       10240 KiB       0.13 s          1.56 %
Ultra High Speed LAN            97656250 KiB/s  0.00003 s       10240 KiB       0.01 s          16.08 %
```

License: Apache-2.0 OR MIT
