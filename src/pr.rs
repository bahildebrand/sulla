use crate::health::Probe;

pub const GH: &str = "gh";

pub fn probe() -> Probe {
    Probe::new("gh", GH).also(&["auth", "status"])
}
