use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    time::Instant,
};

use dns_over_tcp::QueryResult;
use dns_types::{Query, RecordType, ResponseBuilder, ResponseCode};

#[test]
fn smoke() {
    let _guard = firezone_logging::test(
        "netlink_proto=off,wire::dns::res=trace,dns_over_tcp=trace,smoltcp=trace,debug",
    );

    let ipv4 = Ipv4Addr::from([100, 90, 215, 97]);
    let ipv6 = Ipv6Addr::from([0xfd00, 0x2021, 0x1111, 0x0, 0x0, 0x0, 0x0016, 0x588f]);

    let resolver_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(100, 100, 111, 1), 53));

    let mut dns_client = dns_over_tcp::Client::new(Instant::now(), [0u8; 32]);
    dns_client.set_source_interface(ipv4, ipv6);

    let mut dns_server = dns_over_tcp::Server::new(Instant::now());
    dns_server.set_listen_addresses::<1>(BTreeSet::from([resolver_addr]));

    for id in 0..5 {
        dns_client
            .send_query(
                resolver_addr,
                Query::new("example.com".parse().unwrap(), RecordType::A).with_id(id),
            )
            .unwrap();
    }

    let results = std::iter::from_fn(|| progress(&mut dns_client, &mut dns_server))
        .take(5)
        .collect::<Vec<_>>();

    for query_result in results {
        let result = query_result.result.unwrap();

        println!("{result:?}")
    }
}

fn progress(
    dns_client: &mut dns_over_tcp::Client,
    dns_server: &mut dns_over_tcp::Server,
) -> Option<QueryResult> {
    loop {
        if let Some(packet) = dns_client.poll_outbound() {
            dns_server.handle_inbound(packet);
            continue;
        }

        if let Some(packet) = dns_server.poll_outbound() {
            dns_client.handle_inbound(packet);
            continue;
        }

        if let Some(query) = dns_server.poll_queries() {
            dns_server
                .send_message(
                    query.local,
                    query.remote,
                    ResponseBuilder::for_query(&query.message, ResponseCode::NXDOMAIN).build(),
                )
                .unwrap();
            continue;
        }

        if let Some(query) = dns_client.poll_query_result() {
            return Some(query);
        }

        dns_client.handle_timeout(Instant::now());
        dns_server.handle_timeout(Instant::now());
    }
}
