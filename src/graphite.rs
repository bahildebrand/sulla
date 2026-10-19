use crate::health::Probe;

pub const GT: &str = "gt";

pub fn probe() -> Probe {
    Probe::new("gt", GT)
}
