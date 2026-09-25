//! Read-only access to Android filesystem images (ext4, later EROFS).

use std::io::{self, Write};

pub mod ext4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

/// Metadata of one inode, as needed for fs_config and file_contexts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    /// Permission bits including setuid/setgid/sticky (`0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub size: u64,
    /// `security.selinux` label, e.g. `u:object_r:system_file:s0`.
    pub selinux: Option<String>,
    /// Permitted capabilities from `security.capability`.
    pub capabilities: Option<u64>,
}

/// A read-only filesystem image.
pub trait Filesystem {
    /// Inode number (or equivalent) of a node.
    type Node: Copy + Eq + std::hash::Hash;

    fn fs_type(&self) -> &'static str;
    fn root(&self) -> Self::Node;
    fn meta(&mut self, node: Self::Node) -> io::Result<Meta>;
    /// Directory entries without "." and "..", as (raw name, node).
    fn read_dir(&mut self, node: Self::Node) -> io::Result<Vec<(Vec<u8>, Self::Node)>>;
    /// Writes the content of a regular file. Returns the number of bytes.
    fn read_file(&mut self, node: Self::Node, out: &mut dyn Write) -> io::Result<u64>;
    fn read_link(&mut self, node: Self::Node) -> io::Result<Vec<u8>>;
    /// Volume label, e.g. "/" for system-as-root or "vendor".
    fn volume_name(&self) -> String;
    /// Filesystem parameters worth keeping for a rebuild, as key=value.
    fn info(&self) -> Vec<(String, String)>;
}

/// Decodes a `security.capability` xattr (`struct vfs_cap_data`) into the
/// permitted capability mask.
pub fn parse_capability(value: &[u8]) -> Option<u64> {
    if value.len() < 12 {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes(value[i..i + 4].try_into().unwrap()) as u64;
    let low = u32_at(4);
    // revision 2 and 3 carry a second 32-bit word
    let high = if value.len() >= 20 { u32_at(12) } else { 0 };
    Some(low | high << 32)
}

pub(crate) fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_revisions() {
        // rev 1: magic + one (permitted, inheritable) pair
        let v1 = [0, 0, 0, 1, 0x00, 0x10, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_capability(&v1), Some(0x1000));
        // rev 2: second pair holds the high 32 bits
        let mut v2 = vec![0, 0, 0, 2, 0x00, 0x14, 0, 0, 0, 0, 0, 0];
        v2.extend_from_slice(&[0x10, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(parse_capability(&v2), Some(0x10_0000_1400));
        assert_eq!(parse_capability(&[1, 2, 3]), None);
    }
}
