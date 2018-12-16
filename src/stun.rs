use std::io::Error;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::time::Duration;
use std::time::Instant;

use std::sync::Arc;
use std::sync::Mutex;

use tokio::prelude::{*};
use tokio::codec::Decoder;
use tokio::codec::Encoder;
use tokio::timer::Delay;
use bytes::Bytes;
use bytes::BytesMut;

use stun3489::Stun3489;
use stun3489::StunCodec;

pub async fn run(
    stream: impl Stream<Item=(BytesMut, SocketAddr)> + std::marker::Unpin,
    sink: impl Sink<SinkItem=(Bytes, SocketAddr)> + std::marker::Unpin + Send + 'static,
    bind_addr: SocketAddr,
    stun_server: String,
    public_addr: Arc<Mutex<Option<SocketAddr>>>,
) {
    let prepare_io = || {
        let stream = stream.map_err(|_|
            Error::new(ErrorKind::InvalidInput, "TODO")
        );
        let stream = stream.timeout(Duration::from_secs(1));
        let stream = stream.map_err(|e| e.into_inner().unwrap_or_else(||
            Error::new(ErrorKind::TimedOut, ""))
        );

        let stream = stream.map(|(mut pkt, src)| {
            let data = StunCodec.decode(&mut pkt).unwrap().unwrap();
            (data, src)
        });

        let (tx, rx) = futures::sync::mpsc::unbounded();

        let inet_tx = tx.sink_map_err(|_| {
            Error::new(ErrorKind::InvalidInput, "TODO")
        });

        {
            // encode request and send it to the internet
            let inet_rx = rx.then(|res| {
                if let Ok((data, src)) = res {
                    let mut pkt = BytesMut::with_capacity(1024);
                    if StunCodec.encode(data, &mut pkt).is_err() {
                        Err(Error::new(ErrorKind::InvalidInput, "TODO"))
                    } else {
                        Ok((pkt.freeze(), src))
                    }
                } else {
                    Err(Error::new(ErrorKind::InvalidInput, "TODO"))
                }
            });

            let sink = sink.sink_map_err(|_| {
                Error::new(ErrorKind::InvalidInput, "TODO")
            });

            tokio::spawn_async(async move {
                await!(inet_rx.forward(sink)).unwrap();
            });
        }
        (stream, inet_tx)
    };

    let (stream, inet_tx) = prepare_io();
    let mut stun = Stun3489::new(inet_tx, stream);

    loop {
        let mut addrs_iter = stun_server.to_socket_addrs().unwrap();
        let server_addr = addrs_iter.next().unwrap();

        let delay = match await!(stun.check(bind_addr, server_addr)) {
            Ok(conn) => {
                println!("{:?}", conn);
                let conn: Option<SocketAddr> = conn.into();
                *public_addr.lock().unwrap() = conn;

                if conn.is_some() {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(30)
                }
            },
            Err(err) => {
                println!("{:?}", err);
                Duration::from_secs(15)
            },
        };

        await!(Delay::new(Instant::now() + delay)).unwrap();
    }
}
