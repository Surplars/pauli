//! Bounded two-endpoint IPv4/ARP/UDP check over the guest-private Ethernet link.
//! Ownership/Lock Ordering: one guest task; Linux owns interface and socket state.
//! Invariants: only the configured peer address/token passes, no external routes.
use super::sys::{self, Result, call, check, value};
const IOCTL: usize = 29;
const SOCKET: usize = 198;
const BIND: usize = 200;
const SENDTO: usize = 206;
const RECVFROM: usize = 207;
const AF_INET: u16 = 2;
const SOCK_DGRAM: usize = 2;
const SIOCGIFFLAGS: usize = 0x8913;
const SIOCSIFFLAGS: usize = 0x8914;
const SIOCSIFADDR: usize = 0x8916;
const SIOCSIFNETMASK: usize = 0x891c;
const IFF_UP: u16 = 1;
const MSG_DONTWAIT: usize = 0x40;
const TEST_PORT: u16 = 55432;
const ADDRESSES: [[u8; 4]; 2] = [[192, 0, 2, 1], [192, 0, 2, 2]];
#[repr(C)]
struct IfReq {
    name: [u8; 16],
    payload: [u8; 24],
}
pub fn exchange(mode: u8) -> Result<()> {
    let index = usize::from(mode == b'B');
    let local = endpoint(ADDRESSES[index], TEST_PORT);
    let remote = endpoint(ADDRESSES[index ^ 1], TEST_PORT);
    // SAFETY: IPv4 datagram socket scalar arguments.
    let fd = value(unsafe { call(SOCKET, [AF_INET as usize, SOCK_DGRAM, 0, 0, 0, 0]) })?;
    configure(fd, &local)?;
    // SAFETY: complete sockaddr_in input lives through bind.
    check(unsafe { call(BIND, [fd, local.as_ptr() as usize, local.len(), 0, 0, 0]) } == 0)?;
    let result = datagrams(fd, mode, &remote);
    sys::close(fd);
    result
}
fn configure(fd: usize, local: &[u8; 16]) -> Result<()> {
    let mut interface = IfReq {
        name: [0; 16],
        payload: [0; 24],
    };
    interface.name[..4].copy_from_slice(b"eth0");
    interface.payload[..16].copy_from_slice(local);
    interface.payload[2..4].fill(0);
    request(fd, SIOCSIFADDR, &mut interface)?;
    interface.payload[..16].copy_from_slice(&endpoint([255, 255, 255, 0], 0));
    request(fd, SIOCSIFNETMASK, &mut interface)?;
    request(fd, SIOCGIFFLAGS, &mut interface)?;
    let bits = u16::from_ne_bytes([interface.payload[0], interface.payload[1]]) | IFF_UP;
    interface.payload[..2].copy_from_slice(&bits.to_ne_bytes());
    request(fd, SIOCSIFFLAGS, &mut interface)
}
fn request(fd: usize, code: usize, interface: &mut IfReq) -> Result<()> {
    // SAFETY: all callers select documented ifreq ioctls and a full RV64 ifreq.
    check(unsafe { call(IOCTL, [fd, code, interface as *mut IfReq as usize, 0, 0, 0]) } == 0)
}
fn endpoint(ip: [u8; 4], port: u16) -> [u8; 16] {
    let mut address = [0; 16];
    address[..2].copy_from_slice(&AF_INET.to_ne_bytes());
    address[2..4].copy_from_slice(&port.to_be_bytes());
    address[4..8].copy_from_slice(&ip);
    address
}
fn datagrams(fd: usize, mode: u8, remote: &[u8; 16]) -> Result<()> {
    let token = [b'O', b'I', mode];
    let mut seen = false;
    let mut grace = 0;
    for _ in 0..300 {
        let mut data = [0u8; 8];
        let mut sender = [0u8; 16];
        let mut length = sender.len() as u32;
        // SAFETY: live packet and sockaddr buffers with exact declared capacities;
        // Linux writes sender length to one u32, and never blocks on an empty receive queue.
        let received = unsafe {
            let _ = call(
                SENDTO,
                [
                    fd,
                    token.as_ptr() as usize,
                    token.len(),
                    MSG_DONTWAIT,
                    remote.as_ptr() as usize,
                    remote.len(),
                ],
            );
            call(
                RECVFROM,
                [
                    fd,
                    data.as_mut_ptr() as usize,
                    data.len(),
                    MSG_DONTWAIT,
                    sender.as_mut_ptr() as usize,
                    &mut length as *mut u32 as usize,
                ],
            )
        };
        seen |= received == 3
            && data[..3] == [b'O', b'I', mode ^ b'A' ^ b'B']
            && &sender[..8] == &remote[..8];
        if seen {
            grace += 1;
            if grace == 25 {
                return Ok(());
            }
        }
        sys::sleep()?;
    }
    Err(())
}
