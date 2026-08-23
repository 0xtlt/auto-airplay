use std::net::{IpAddr, Ipv4Addr};

use auto_airplay::discovery::{DiscoveredOutput, select_output};

fn output(name: &str, id: &str, last_octet: u8) -> DiscoveredOutput {
    DiscoveredOutput {
        name: name.into(),
        id: id.into(),
        model: "AudioAccessory1,1".into(),
        addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, last_octet))],
        port: 7000,
        supports_airplay2: true,
    }
}

#[test]
fn exact_name_is_preferred_over_partial_matches() {
    let outputs = vec![
        output("Living Room Group", "GROUP", 21),
        output("Living Room", "HOME", 16),
    ];

    let selected = select_output(&outputs, "Living Room").unwrap();

    assert_eq!(selected.id, "HOME");
}

#[test]
fn output_can_be_selected_by_id_or_ip() {
    let mut receiver = output("Living Room", "AABBCCDDEEFF", 16);
    receiver.addresses = vec!["192.0.2.10".parse().unwrap()];
    let outputs = vec![receiver];

    assert_eq!(
        select_output(&outputs, "AABBCCDDEEFF").unwrap().name,
        "Living Room"
    );
    assert_eq!(
        select_output(&outputs, "192.0.2.10").unwrap().name,
        "Living Room"
    );
}
