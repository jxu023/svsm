use core::ops::{Index, IndexMut};
use core::alloc::{GlobalAlloc, Layout};
use super::allocator::ALLOCATOR;
use super::util::*;

pub const PAGE_SIZE		: usize = 4096;
//const PAGE_SIZE_2M	: usize = 1024 * PAGE_SIZE;
const ENTRY_COUNT	: usize = 512;

pub type PhysAddr	= usize;
pub type VirtAddr	= usize;

pub fn flush_tlb() {
	write_cr3(read_cr3());
}

pub fn flush_tlb_global() {
	let cr4 = read_cr4();

	if !cr4.contains(CR4Flags::PGE) {
		return;
	}

	let mut cr4_nopge = cr4;
	cr4_nopge.remove(CR4Flags::PGE);

	write_cr4(cr4_nopge);
	write_cr4(cr4);
}

fn encrypt_mask() -> usize {
	1 << 51
	//0
}

fn strip_c_bit(paddr : PhysAddr) -> PhysAddr {
	paddr & !encrypt_mask()
}

fn set_c_bit(paddr : PhysAddr) -> PhysAddr {
	paddr | encrypt_mask()
}

bitflags! {
	pub struct PTEntryFlags: u64 {
		const PRESENT		= 1 << 0;
		const WRITABLE		= 1 << 1;
		const USER		= 1 << 2;
		const ACCESSED		= 1 << 5;
		const DIRTY		= 1 << 6;
		const HUGE		= 1 << 7;
		const GLOBAL		= 1 << 8;
		const NX		= 1 << 63;
	}
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct PTEntry(pub u64);

impl PTEntry {
	pub fn is_clear(&self) -> bool {
		self.0 == 0
	}

	pub fn clear(&mut self) {
		self.0 = 0;
	}

	pub fn flags(&self) -> PTEntryFlags {
		PTEntryFlags::from_bits_truncate(self.0)
	}

	pub fn set(&mut self, addr: PhysAddr, flags: PTEntryFlags) {
		assert!(addr & !0x000f_ffff_ffff_f000 == 0);
		self.0 = (addr as u64) | flags.bits();
	}

	pub fn address(&self) -> PhysAddr {
		strip_c_bit((self.0 & 0x000f_ffff_ffff_f000) as PhysAddr)
	}
}

#[repr(C)]
pub struct PTPage {
	entries: [PTEntry; ENTRY_COUNT],
}

impl Index<usize> for PTPage {
	type Output = PTEntry;

	fn index(&self, index : usize) -> &PTEntry {
		&self.entries[index]
	}
}

impl IndexMut<usize> for PTPage {
	fn index_mut(&mut self, index: usize) -> &mut PTEntry {
		&mut self.entries[index]
	}
}

pub enum Mapping<'a> {
	Level3(&'a mut PTEntry),
	Level2(&'a mut PTEntry),
	Level1(&'a mut PTEntry),
	Level0(&'a mut PTEntry),
}

#[repr(C)]
pub struct PageTable {
	root: PTPage,
}

impl PageTable {

	fn allocate_page_table() -> *mut PTPage {
		let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();

		unsafe {
			let ptr = ALLOCATOR.alloc(layout);
			ptr as *mut PTPage
		}
	}

	fn index<const L : usize>(vaddr : VirtAddr) -> usize {
		vaddr >> (12 + L * 9) & 0x1ff
	}

	fn entry_to_pagetable(entry : PTEntry) -> Option<&'static mut PTPage> {
		let flags = entry.flags();
		if !flags.contains(PTEntryFlags::PRESENT) ||
		    flags.contains(PTEntryFlags::HUGE) {
			    return None;
		    }

		let address = entry.address();
		Some(unsafe { &mut *(address as *mut PTPage) })

	}

	fn walk_addr_lvl0<'a>(page: &'a mut PTPage, vaddr : VirtAddr) -> Mapping<'a> {
		let idx = PageTable::index::<0>(vaddr);

		Mapping::Level0(&mut page[idx])
	}
	
	fn walk_addr_lvl1<'a>(page: &'a mut PTPage, vaddr : VirtAddr) -> Mapping<'a> {
		let idx = PageTable::index::<1>(vaddr);
		let entry = page[idx];
		let ret = PageTable::entry_to_pagetable(entry);

		match ret {
			Some(page) => return PageTable::walk_addr_lvl0(page, vaddr),
			None => return Mapping::Level1(&mut page[idx]),
		}
	}

	fn walk_addr_lvl2<'a>(page: &'a mut PTPage, vaddr : VirtAddr) -> Mapping<'a> {
		let idx = PageTable::index::<2>(vaddr);
		let entry = page[idx];
		let ret = PageTable::entry_to_pagetable(entry);

		match ret {
			Some(page) => return PageTable::walk_addr_lvl1(page, vaddr),
			None => return Mapping::Level2(&mut page[idx]),
		}
	}

	fn walk_addr_lvl3<'a>(page: &'a mut PTPage, vaddr : VirtAddr) -> Mapping<'a> {
		let idx = PageTable::index::<3>(vaddr);
		let entry = page[idx];
		let ret = PageTable::entry_to_pagetable(entry);

		match ret {
			Some(page) => return PageTable::walk_addr_lvl2(page, vaddr),
			None => return Mapping::Level3(&mut page[idx]),
		}
	}

	pub fn walk_addr(&mut self, vaddr : VirtAddr) -> Mapping {
		PageTable::walk_addr_lvl3(&mut self.root, vaddr)
	}

	fn do_split_4k(entry : &mut PTEntry) -> Result<(), ()> {
		let page = PageTable::allocate_page_table();
		let mut flags = entry.flags();

		assert!(flags.contains(PTEntryFlags::HUGE));

		if page.is_null() {
			return Err(());
		}

		let addr_2m = entry.address() & 0x000f_ffff_fff0_0000;

		flags.remove(PTEntryFlags::HUGE);

		// Prepare PTE leaf page
		for i in 0..512 {
			let addr_4k = addr_2m + (i * PAGE_SIZE);
			unsafe {
				(*page).entries[i].clear();
				(*page).entries[i].set(set_c_bit(addr_4k), flags);
			}
		}

		let addr_2m = page as PhysAddr;
		entry.set(set_c_bit(addr_2m), flags);

		flush_tlb();

		Ok(())
	}

	pub fn split_4k(mapping : Mapping) -> Result<(),()> {
		match mapping {
			Mapping::Level0(_entry) => Ok(()),
			Mapping::Level1( entry) => PageTable::do_split_4k(entry),
			Mapping::Level2(_entry) => Err(()),
			Mapping::Level3(_entry) => Err(()),
		}
	}

	fn clear_c_bit(entry : &mut PTEntry) {
		let flags = entry.flags();
		let addr  = entry.address();

		// entry.address() returned with c-bit clear already
		entry.set(addr, flags);
	}
	
	fn set_c_bit(entry : &mut PTEntry) {
		let flags = entry.flags();
		let addr  = entry.address();

		// entry.address() returned with c-bit clear already
		entry.set(set_c_bit(addr), flags);
	}

	pub fn set_shared_4k(&mut self, vaddr : VirtAddr) -> Result<(), ()> {
		let mapping = self.walk_addr(vaddr);

		if let Err(_e) = PageTable::split_4k(mapping) {
			return Err(());
		}

		if let Mapping::Level0(entry) = self.walk_addr(vaddr) {
			PageTable::clear_c_bit(entry);
			Ok(())
		} else {
			Err(())
		}
	}
	
	pub fn set_encrypted_4k(&mut self, vaddr : VirtAddr) -> Result<(), ()> {
		let mapping = self.walk_addr(vaddr);

		if let Err(_e) = PageTable::split_4k(mapping) {
			return Err(());
		}

		if let Mapping::Level0(entry) = self.walk_addr(vaddr) {
			PageTable::set_c_bit(entry);
			Ok(())
		} else {
			Err(())
		}
	}
}
