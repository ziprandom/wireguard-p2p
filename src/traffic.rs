use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::net::IpAddr;
use std::net::Ipv4Addr;

use tokio::prelude::*;
use tokio::prelude::stream::SplitSink;
use tokio::codec::BytesCodec;
use tokio::net::UdpFramed;
use futures::sync::mpsc::UnboundedSender;
use futures::sync::mpsc::UnboundedReceiver;
use futures::sync::mpsc;
use bytes::Bytes;
use bytes::BytesMut;

use crate::wg;

type UdpSink = SplitSink<UdpFramed<BytesCodec>>;

/// Create a new loopback socket for a new peer to forward packets between the
/// public socket and the loopback wireguard socket
fn create_internal_socket(remote_addr: SocketAddr,
    mut outbound: mpsc::UnboundedSender<(Bytes, SocketAddr)>)
    -> std::io::Result<(UdpSink, u16)>
{
    let loop_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
    let sock = tokio::net::UdpSocket::bind(&loop_addr)?;
    let port = sock.local_addr()?.port();
    
    let codec = BytesCodec::new();
    let (send, mut recv) = UdpFramed::new(sock, codec).split();

    // forward packets from the new loopback socket to the remote peer
    tokio::spawn_async(async move {
        while let Some(res) = await!(recv.next()) {
            match res {
                Err(e) => error!("{:?}", e),
                Ok((pkt, wg_addr)) => {
                    let pkt = Bytes::from(pkt);
                    debug!("LO2OUT {} bytes from {} via lo port {} to {}", pkt.len(), wg_addr, port, remote_addr);

                    await!(outbound.send_async((pkt, remote_addr))).unwrap();
                }
            }
        }
    });

    Ok((send, port))
}

pub async fn forward_inbound(
    mut new_endpoints_tx: UnboundedSender<(SocketAddr, u16)>,
    mut udp_rx: impl Stream<Item=(BytesMut, SocketAddr), Error=impl std::fmt::Debug> + std::marker::Unpin,
    mut inet2stun_tx: impl Sink<SinkItem=(BytesMut, SocketAddr), SinkError=impl std::fmt::Debug> + std::marker::Unpin,
    udp_tx: mpsc::UnboundedSender<(Bytes, SocketAddr)>,
    wg_port: u16)
{
    let mut connections = HashMap::new();
    let dst = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), wg_port);

    while let Some(res) = await!(udp_rx.next()) {
        match res {
            Err(e) => error!("UDP Receive Error: {:?}", e),
            Ok((pkt, remote_addr)) => {
                log_err!(await!(inet2stun_tx.send_async((pkt.clone(), dst))), "inet2stun_tx Send Error: {:?}");

                // TODO: cache this lookup?
                let mut is_new = false;
                let (via_sock, via_port) = connections
                    .entry(remote_addr)
                    .or_insert_with(|| {
                        is_new = true;
                        create_internal_socket(remote_addr, udp_tx.clone()).unwrap()
                        // TODO: send to broadcast (to dht)
                    });
                if is_new {
                    log_err!(await!(new_endpoints_tx.send_async((remote_addr, *via_port))),
                        "New Endpoint Send Error: {:?}");
                }

                debug!("IN2LO {} bytes from {} via lo port {} to wg port {}",
                    pkt.len(), remote_addr, via_port, dst.port());

                let pkt = Bytes::from(pkt);
                log_err!(await!(via_sock.send_async((pkt, dst))),
                    "lo2wg Send Error: {:?}");
            }
        }
    }
}

enum WgSetEndpoint {
    Endpoint((SocketAddr, u16)),
    DhtAddress(SocketAddr),
}

pub async fn set_endpoint(
    netns: Option<String>,
    wg_iface: String,
    remote_public_key: Vec<u8>,
    new_endpoints: UnboundedReceiver<(SocketAddr, u16)>,
    dht_address_rx: UnboundedReceiver<SocketAddr>,
) {
    let lo_ip: IpAddr = [127, 0, 0, 1].into();

    let mut dht_addresses = HashSet::new();
    let mut set_endpoints = HashMap::new();

    let new_endpoints = new_endpoints.map(|e| WgSetEndpoint::Endpoint(e));
    let dht_address_rx = dht_address_rx.map(|addr| WgSetEndpoint::DhtAddress(addr));

    let mut stream = new_endpoints.select(dht_address_rx);
    while let Some(Ok(item)) = await!(stream.next()) {
        // Err should never happen, because Receiver should never fail
        let addr = match item {
            WgSetEndpoint::DhtAddress(addr) => {
                dht_addresses.insert(addr);
                addr
            },
            WgSetEndpoint::Endpoint((addr, port)) => {
                set_endpoints.insert(addr, port);
                addr
            },
        };

        if dht_addresses.contains(&addr) {
            if let Some(lo_port) = set_endpoints.get(&addr) {
                debug!("Mapping {} to local port {}", addr, lo_port);
                log_err!(await!(wg::set_endpoint(netns.clone(), &wg_iface, &remote_public_key, (lo_ip, *lo_port).into())));
            }
        }
    }
}