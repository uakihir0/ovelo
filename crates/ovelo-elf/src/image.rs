//! In-memory page-table model for loaded ELF images.
//!
//! The loader maps `PT_LOAD` segments into a sparse set of 4 KiB pages and
//! enforces the segment R/W/X flags: guest reads/writes/executes are checked
//! against [`PagePerm`]. Loader-internal writes (relocation fixups) use the
//! privileged [`Image::write_raw`] path, mirroring how a real linker applies
//! relocations before final `mprotect`.

use crate::{err, errd, PAGE};
use ovelo_error::Result;
use std::collections::HashMap;

const PAGE_SIZE: usize = PAGE as usize;

/// Page permission bits, from `p_flags` (PF_R=4, PF_W=2, PF_X=1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagePerm(u8);

impl PagePerm {
    /// Readable.
    pub const READ: Self = Self(4);
    /// Writable.
    pub const WRITE: Self = Self(2);
    /// Executable.
    pub const EXEC: Self = Self(1);

    /// No permissions.
    pub const fn empty() -> Self {
        Self(0)
    }
    /// Whether all bits of `other` are set.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for PagePerm {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PagePerm {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

struct Page {
    data: Box<[u8; PAGE_SIZE]>,
    perm: PagePerm,
}

/// Sparse guest address space.
#[derive(Default)]
pub struct Image {
    pages: HashMap<u64, Page>,
}

impl Image {
    /// Empty address space.
    pub fn new() -> Self {
        Self::default()
    }

    /// Map `bytes` at `vaddr`, zero-filling up to `memsz` (BSS).
    pub fn map_segment(
        &mut self,
        vaddr: u64,
        bytes: &[u8],
        memsz: u64,
        perm: PagePerm,
    ) -> Result<()> {
        let end = vaddr
            .checked_add(memsz)
            .ok_or_else(|| err("elf.map.bounds"))?;
        if bytes.len() as u64 > memsz {
            return Err(err("elf.map.filesz"));
        }
        let mut a = vaddr & !(PAGE - 1);
        while a < end {
            if self.pages.contains_key(&a) {
                return Err(errd("elf.map.overlap", format!("page {a:#x}")));
            }
            self.pages.insert(
                a,
                Page {
                    data: Box::new([0u8; PAGE_SIZE]),
                    perm,
                },
            );
            a += PAGE;
        }
        self.write_raw(vaddr, bytes)
    }

    /// Guest read: every touched page must allow `PagePerm::READ`.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        self.check_range(addr, buf.len(), PagePerm::READ)?;
        self.copy_out(addr, buf)
    }

    /// Unchecked multi-page copy-out (caller validated perms).
    fn copy_out(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let a = addr + done as u64;
            let base = a & !(PAGE - 1);
            let off = (a - base) as usize;
            let n = (PAGE_SIZE - off).min(buf.len() - done);
            let page = self
                .pages
                .get(&base)
                .ok_or_else(|| err("elf.mem.unmapped"))?;
            buf[done..done + n].copy_from_slice(&page.data[off..off + n]);
            done += n;
        }
        Ok(())
    }

    /// Guest write: every touched page must be writable.
    pub fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<()> {
        self.check_range(addr, bytes.len(), PagePerm::WRITE)?;
        self.write_raw(addr, bytes)
    }

    /// Guest execute check: page containing `addr` must be executable.
    pub fn check_exec(&self, addr: u64) -> Result<()> {
        self.check_range(addr, 1, PagePerm::EXEC)
    }

    /// Loader-privileged write (relocation fixups): ignores page perms.
    pub fn write_raw(&mut self, addr: u64, bytes: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < bytes.len() {
            let a = addr + done as u64;
            let base = a & !(PAGE - 1);
            let off = (a - base) as usize;
            let n = (PAGE_SIZE - off).min(bytes.len() - done);
            let page = self
                .pages
                .get_mut(&base)
                .ok_or_else(|| err("elf.mem.unmapped"))?;
            page.data[off..off + n].copy_from_slice(&bytes[done..done + n]);
            done += n;
        }
        Ok(())
    }

    /// Loader-privileged read (relocation addends, init_array).
    pub fn read_u64(&self, addr: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.copy_out(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Guest u64 read with R permission check.
    pub fn read_u64_guest(&self, addr: u64) -> Result<u64> {
        self.check_range(addr, 8, PagePerm::READ)?;
        self.read_u64(addr)
    }

    /// Guest u64 write with W permission check.
    pub fn write_u64_guest(&mut self, addr: u64, v: u64) -> Result<()> {
        self.write(addr, &v.to_le_bytes())
    }

    /// Every page covering `[addr, addr+len)` must contain `needed`.
    fn check_range(&self, addr: u64, len: usize, needed: PagePerm) -> Result<()> {
        let mut a = addr & !(PAGE - 1);
        let end = addr
            .checked_add(len as u64)
            .ok_or_else(|| err("elf.mem.bounds"))?;
        while a < end {
            let page = self.pages.get(&a).ok_or_else(|| err("elf.mem.unmapped"))?;
            if !page.perm.contains(needed) {
                return Err(errd("elf.mem.perm", format!("page {a:#x}")));
            }
            a += PAGE;
        }
        Ok(())
    }

    /// Total mapped bytes.
    pub fn mapped_bytes(&self) -> usize {
        self.pages.len() * PAGE_SIZE
    }
}
