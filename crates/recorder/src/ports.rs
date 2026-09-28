//! UDP port pairs for incoming tracks: RTP on an even port, RTCP on the next one, from the configured range.

use anyhow::{anyhow, Result};
use std::collections::BTreeSet;
use std::sync::Mutex;

pub struct PortAllocator {
    min: u16,
    max: u16,
    in_use: Mutex<BTreeSet<u16>>,
}

impl PortAllocator {
    pub fn new(min: u16, max: u16) -> Self {
        Self {
            min: min + (min % 2),
            max,
            in_use: Mutex::new(BTreeSet::new()),
        }
    }

    /// Reserve an (rtp, rtcp) pair. The caller binds them; a bind failure should `release` and retry.
    pub fn allocate(&self) -> Result<(u16, u16)> {
        let mut used = self.in_use.lock().unwrap();
        let mut port = self.min;
        while port < self.max {
            if !used.contains(&port) {
                used.insert(port);
                return Ok((port, port + 1));
            }
            port += 2;
        }
        Err(anyhow!(
            "no free RTP port pair in {}..={}",
            self.min,
            self.max
        ))
    }

    pub fn release(&self, rtp_port: u16) {
        self.in_use.lock().unwrap().remove(&rtp_port);
    }

    pub fn in_use(&self) -> usize {
        self.in_use.lock().unwrap().len()
    }
}
