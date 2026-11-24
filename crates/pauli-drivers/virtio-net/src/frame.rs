//! Single-buffer modern VirtIO-net framing, retaining native non-merge behavior.
//! Ownership/Lock Ordering: borrowed CPU-owned packet; no state or locks.
//! Invariants: ignore unused num_buffers; lengths/offload flags define accepted frames.

pub const HEADER: usize = 12;
pub const FRAME: usize = 1514;
pub fn rx_payload(packet: &[u8]) -> Option<&[u8]> {
    if !(HEADER + 14..=HEADER + FRAME).contains(&packet.len()) || packet[0] != 0 || packet[1] != 0 {
        return None;
    }
    // Native non-merge RX may write only ten header bytes. The trailing
    // num_buffers field is not negotiated and cannot determine payload bounds.
    Some(&packet[HEADER..])
}

#[cfg(test)]
mod tests {
    //! Frame boundary tests; local arrays, no locks or hardware access.
    //! Invariants: stale unused fields cannot turn accepted packets into DMA errors.
    use super::*;
    #[test]
    fn modern_nonmerge_and_malformed_frames() {
        let mut packet = [0u8; HEADER + FRAME];
        packet[10..12].copy_from_slice(&[0xff, 0xff]);
        assert_eq!(rx_payload(&packet).unwrap().len(), FRAME);
        assert!(rx_payload(&packet[..HEADER + 13]).is_none());
        packet[0] = 1;
        assert!(rx_payload(&packet).is_none());
        packet[0] = 0;
        packet[1] = 1;
        assert!(rx_payload(&packet).is_none());
    }
}
