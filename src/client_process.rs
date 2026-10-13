//! Linux process lookup for loopback OpenAI clients.

use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

/// A process identity that remains safe across PID reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientProcess {
    /// Process ID found through the client's TCP socket.
    pub pid: u32,
    /// Start time from field 22 of `/proc/<pid>/stat`.
    pub start_time: u64,
}

/// Find the same-UID process that owns the TCP connection from `peer` to `server`.
pub fn find_client_process(
    proc_root: &Path,
    peer: SocketAddr,
    server: SocketAddr,
) -> Option<ClientProcess> {
    #[cfg(target_os = "linux")]
    {
        find_linux_client_process(proc_root, peer, server)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (proc_root, peer, server);
        None
    }
}

/// Check that the process still has the same start time and is not a zombie.
pub fn is_alive(proc_root: &Path, process: &ClientProcess) -> bool {
    #[cfg(target_os = "linux")]
    {
        read_process_stat(proc_root, process.pid)
            .is_some_and(|stat| stat.start_time == process.start_time && stat.state != b'Z')
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (proc_root, process);
        false
    }
}

#[cfg(target_os = "linux")]
fn find_linux_client_process(
    proc_root: &Path,
    peer: SocketAddr,
    server: SocketAddr,
) -> Option<ClientProcess> {
    use std::os::unix::fs::MetadataExt;

    let table = if peer.is_ipv4() && server.is_ipv4() {
        "net/tcp"
    } else if peer.is_ipv6() && server.is_ipv6() {
        "net/tcp6"
    } else {
        return None;
    };
    let inode = find_socket_inode(
        &std::fs::read_to_string(proc_root.join(table)).ok()?,
        peer,
        server,
    )?;
    let own_pid = std::process::id();
    let own_uid = std::fs::metadata(proc_root.join("self")).ok()?.uid();

    for entry in std::fs::read_dir(proc_root).ok()?.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own_pid
            || entry
                .metadata()
                .ok()
                .is_none_or(|metadata| metadata.uid() != own_uid)
        {
            continue;
        }
        let fd_dir = entry.path().join("fd");
        let Ok(fd_entries) = std::fs::read_dir(fd_dir) else {
            continue;
        };
        let owns_socket = fd_entries.filter_map(Result::ok).any(|fd| {
            std::fs::read_link(fd.path())
                .ok()
                .is_some_and(|target| target == format!("socket:[{inode}]"))
        });
        if owns_socket {
            let stat = read_process_stat(proc_root, pid)?;
            return Some(ClientProcess {
                pid,
                start_time: stat.start_time,
            });
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn find_socket_inode(table: &str, peer: SocketAddr, server: SocketAddr) -> Option<u64> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10
            || parse_endpoint(fields[1])? != peer
            || parse_endpoint(fields[2])? != server
        {
            return None;
        }
        fields[9].parse().ok()
    })
}

#[cfg(target_os = "linux")]
fn parse_endpoint(value: &str) -> Option<SocketAddr> {
    let (address, port) = value.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = match address.len() {
        8 => {
            let mut octets = [0; 4];
            for (index, byte) in octets.iter_mut().enumerate() {
                let start = (3 - index) * 2;
                *byte = u8::from_str_radix(&address[start..start + 2], 16).ok()?;
            }
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        32 => {
            let mut octets = [0; 16];
            for (word, chunk) in address.as_bytes().chunks_exact(8).enumerate() {
                for (byte_index, pair) in chunk.chunks_exact(2).enumerate() {
                    let start = word * 4 + (3 - byte_index);
                    octets[start] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
                }
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(target_os = "linux")]
struct ProcessStat {
    state: u8,
    start_time: u64,
}

#[cfg(target_os = "linux")]
fn read_process_stat(proc_root: &Path, pid: u32) -> Option<ProcessStat> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(ProcessStat {
        state: *fields.first()?.as_bytes().first()?,
        start_time: fields.get(19)?.parse().ok()?,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct FakeProc {
        root: std::path::PathBuf,
    }

    impl FakeProc {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-openai-server-proc-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("net")).unwrap();
            let own = root.join(std::process::id().to_string());
            std::fs::create_dir_all(&own).unwrap();
            symlink(std::process::id().to_string(), root.join("self")).unwrap();
            Self { root }
        }

        fn add_process(&self, pid: u32, comm: &str, state: char, start_time: u64, inode: u64) {
            let fd = self.root.join(pid.to_string()).join("fd");
            std::fs::create_dir_all(&fd).unwrap();
            std::fs::write(
                self.root.join(pid.to_string()).join("stat"),
                stat_line(pid, comm, state, start_time),
            )
            .unwrap();
            symlink(format!("socket:[{inode}]"), fd.join("3")).unwrap();
        }

        fn table(&self, family: &str, peer: SocketAddr, server: SocketAddr, inode: u64) {
            std::fs::write(
                self.root.join("net").join(family),
                format!("sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n0: {} {} 01 00000000:00000000 00:00000000 00000000 1000 0 {inode}\n", proc_endpoint(peer), proc_endpoint(server)),
            )
            .unwrap();
        }
    }

    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn stat_line(pid: u32, comm: &str, state: char, start_time: u64) -> String {
        let mut fields = vec![state.to_string()];
        fields.extend((0..18).map(|_| "1".to_string()));
        fields.push(start_time.to_string());
        format!("{pid} ({comm}) {}\n", fields.join(" "))
    }

    fn proc_endpoint(address: SocketAddr) -> String {
        let ip: String = match address.ip() {
            IpAddr::V4(ip) => ip
                .octets()
                .into_iter()
                .rev()
                .map(|b| format!("{b:02X}"))
                .collect(),
            IpAddr::V6(ip) => ip
                .octets()
                .chunks_exact(4)
                .flat_map(|word| word.iter().rev().copied())
                .map(|b| format!("{b:02X}"))
                .collect(),
        };
        format!("{ip}:{:04X}", address.port())
    }

    #[test]
    fn finds_client_from_tcp_and_parses_comm_with_spaces_and_parentheses() {
        let proc = FakeProc::new();
        let peer = "127.0.0.1:51000".parse().unwrap();
        let server = "127.0.0.1:8787".parse().unwrap();
        proc.table("tcp", peer, server, 4321);
        proc.add_process(1234, "opencode serve (stdio)", 'S', 9876, 4321);
        assert_eq!(
            find_client_process(&proc.root, peer, server),
            Some(ClientProcess {
                pid: 1234,
                start_time: 9876,
            })
        );
    }

    #[test]
    fn finds_client_from_tcp6() {
        let proc = FakeProc::new();
        let peer = "[::1]:51001".parse().unwrap();
        let server = "[::1]:8787".parse().unwrap();
        proc.table("tcp6", peer, server, 9876);
        proc.add_process(1235, "opencode", 'S', 12, 9876);
        assert_eq!(
            find_client_process(&proc.root, peer, server).unwrap().pid,
            1235
        );
    }

    #[test]
    fn process_liveness_checks_start_time_state_and_presence() {
        let proc = FakeProc::new();
        proc.add_process(1234, "client", 'S', 77, 1);
        proc.add_process(1235, "zombie", 'Z', 88, 2);
        assert!(is_alive(
            &proc.root,
            &ClientProcess {
                pid: 1234,
                start_time: 77,
            }
        ));
        assert!(!is_alive(
            &proc.root,
            &ClientProcess {
                pid: 1234,
                start_time: 78,
            }
        ));
        assert!(!is_alive(
            &proc.root,
            &ClientProcess {
                pid: 1235,
                start_time: 88,
            }
        ));
        assert!(!is_alive(
            &proc.root,
            &ClientProcess {
                pid: 9999,
                start_time: 0,
            }
        ));
    }

    #[test]
    fn missing_socket_or_proc_files_are_not_tracked() {
        let proc = FakeProc::new();
        let peer = "127.0.0.1:51000".parse().unwrap();
        let server = "127.0.0.1:8787".parse().unwrap();
        proc.table("tcp", peer, server, 4321);
        assert_eq!(find_client_process(&proc.root, peer, server), None);
        std::fs::remove_file(proc.root.join("net/tcp")).unwrap();
        assert_eq!(find_client_process(&proc.root, peer, server), None);
    }
}
