use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredOutput {
    pub name: String,
    pub id: String,
    pub model: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub supports_airplay2: bool,
}

pub fn select_output<'a>(
    outputs: &'a [DiscoveredOutput],
    selector: &str,
) -> Option<&'a DiscoveredOutput> {
    let normalized = selector.trim().to_ascii_lowercase();

    outputs
        .iter()
        .find(|output| output.name.to_ascii_lowercase() == normalized)
        .or_else(|| {
            outputs
                .iter()
                .find(|output| output.id.to_ascii_lowercase() == normalized)
        })
        .or_else(|| {
            outputs.iter().find(|output| {
                output
                    .addresses
                    .iter()
                    .any(|address| address.to_string().to_ascii_lowercase() == normalized)
            })
        })
        .or_else(|| {
            outputs
                .iter()
                .find(|output| output.name.to_ascii_lowercase().contains(&normalized))
        })
}
