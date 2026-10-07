// Minimal, bounded-memory demonstration fixture for the `pair-snapshot`
// feature: creates one loopback TCP connection, prints the paired local/peer
// addresses for both ends, then shows that a bound-but-unconnected socket is
// rejected with the original OS error (no half snapshot). Run with:
//
//   cargo run --features pair-snapshot --example addr_pair_demo
use socket2::{Domain, Socket, Type};
use std::net::{Ipv4Addr, SocketAddrV4};

fn loopback() -> socket2::SockAddr {
    socket2::SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
}

fn main() {
    let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    listener.bind(&loopback()).unwrap();
    listener.listen(1).unwrap();

    let client = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    client.connect(&listener.local_addr().unwrap()).unwrap();
    let accepted = listener.accept().unwrap().0;

    for (name, s) in [("client", &client), ("server", &accepted)] {
        let pair = s.addr_pair().expect("both queries succeed");
        println!(
            "{name}: local={} peer={}",
            pair.local_addr().as_socket().unwrap(),
            pair.peer_addr().as_socket().unwrap()
        );
    }

    // Bound but never connected: pair snapshot must be rejected, keeping the
    // peer-address operation and the original OS "not connected" error.
    let unconnected = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    unconnected.bind(&loopback()).unwrap();
    match unconnected.addr_pair() {
        Ok(_) => panic!("unconnected socket must not yield a snapshot"),
        Err(e) => {
            println!(
                "unconnected: rejected by op={:?} kind={:?} raw_os_error={:?}",
                e.op(),
                e.kind(),
                e.raw_os_error()
            );
        }
    }
}
