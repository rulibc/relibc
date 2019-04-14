use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::{mem, ptr, slice};
use goblin::elf::{Elf, program_header, reloc, sym};
use goblin::error::{Error, Result};

use c_str::CString;
use fs::File;
use header::{fcntl, sys_mman, unistd};
use io::Read;
use platform::types::c_void;

const PAGE_SIZE: usize = 4096;

#[cfg(target_os = "redox")]
const PATH_SEP: char = ';';

#[cfg(target_os = "linux")]
const PATH_SEP: char = ':';

// On Linux, a new TCB is required
#[cfg(target_os = "linux")]
unsafe fn allocate_tls(size: usize) -> Result<&'static mut [u8]> {
    let ptr = sys_mman::mmap(
        ptr::null_mut(),
        size + PAGE_SIZE /* TLS and TCB */,
        sys_mman::PROT_READ | sys_mman::PROT_WRITE,
        sys_mman::MAP_ANONYMOUS | sys_mman::MAP_PRIVATE,
        -1,
        0
    );
    if ptr as usize == !0 /* MAP_FAILED */ {
        return Err(Error::Malformed(
            format!("failed to map tls")
        ));
    }

    let mut tls = slice::from_raw_parts_mut(ptr as *mut u8, size);
    let mut tcb = slice::from_raw_parts_mut((ptr as *mut u8).add(size), PAGE_SIZE);
    *(tcb.as_mut_ptr() as *mut *mut u8) = tls.as_mut_ptr().add(size);

    #[cfg(target_arch = "x86_64")]
    {
        const ARCH_SET_FS: usize = 0x1002;
        syscall!(ARCH_PRCTL, ARCH_SET_FS, tcb.as_mut_ptr());
    }

    Ok(tls)
}

// On Redox, reuse the current TCB
// TODO: Consider adopting Linux behavior
#[cfg(target_os = "redox")]
unsafe fn allocate_tls(size: usize) -> Result<&'static mut [u8]> {
    let ptr = sys_mman::mmap(
        ptr::null_mut(),
        size,
        sys_mman::PROT_READ | sys_mman::PROT_WRITE,
        sys_mman::MAP_ANONYMOUS | sys_mman::MAP_PRIVATE,
        -1,
        0
    );
    if ptr as usize == !0 /* MAP_FAILED */ {
        return Err(Error::Malformed(
            format!("failed to map tls")
        ));
    }

    let mut tls = slice::from_raw_parts_mut(ptr as *mut u8, size);
    let mut tcb = slice::from_raw_parts_mut(0xB000_0000 as *mut u8, PAGE_SIZE);
    *(tcb.as_mut_ptr() as *mut *mut u8) = tls.as_mut_ptr().add(size);

    Ok(tls)
}

pub struct Linker {
    library_path: String,
    objects: BTreeMap<String, Box<[u8]>>
}

impl Linker {
    pub fn new(library_path: &str) -> Self {
        Self {
            library_path: library_path.to_string(),
            objects: BTreeMap::new(),
        }
    }

    pub fn load(&mut self, name: &str, path: &str) -> Result<()> {
        println!("load {}: {}", name, path);

        let mut data = Vec::new();

        let path_c = CString::new(path).map_err(|err| Error::Malformed(
            format!("invalid path '{}': {}", path, err)
        ))?;

        {
            let flags = fcntl::O_RDONLY | fcntl::O_CLOEXEC;
            let mut file = File::open(&path_c, flags).map_err(|err| Error::Malformed(
                format!("failed to open '{}': {}", path, err)
            ))?;

            file.read_to_end(&mut data).map_err(|err| Error::Malformed(
                format!("failed to read '{}': {}", path, err)
            ))?;
        }

        self.load_data(name, data.into_boxed_slice())
    }

    pub fn load_data(&mut self, name: &str, data: Box<[u8]>) -> Result<()> {
        //TODO: Prevent failures due to recursion
        {
            let elf = Elf::parse(&data)?;
            //println!("{:#?}", elf);

            for library in elf.libraries.iter() {
                if ! self.objects.contains_key(&library.to_string()) {
                    self.load_library(library)?;
                }
            }
        }

        self.objects.insert(name.to_string(), data);

        Ok(())
    }

    pub fn load_library(&mut self, name: &str) -> Result<()> {
        if name.contains('/') {
            self.load(name, name)
        } else {
            let library_path = self.library_path.clone();
            for part in library_path.split(PATH_SEP) {
                let path = if part.is_empty() {
                    format!("./{}", name)
                } else {
                    format!("{}/{}", part, name)
                };

                println!("check {}", path);

                let access = unsafe {
                    let path_c = CString::new(path.as_bytes()).map_err(|err| Error::Malformed(
                        format!("invalid path '{}': {}", path, err)
                    ))?;

                    // TODO: Use R_OK | X_OK
                    unistd::access(path_c.as_ptr(), unistd::F_OK) == 0
                };

                if access {
                    self.load(name, &path)?;
                    return Ok(());
                }
            }

            Err(Error::Malformed(
                format!("failed to locate '{}'", name)
            ))
        }
    }

    pub fn link(&mut self, primary: &str) -> Result<usize> {
        let elfs = {
            let mut elfs = BTreeMap::new();
            for (name, data) in self.objects.iter() {
                elfs.insert(name.as_str(), Elf::parse(&data)?);
            }
            elfs
        };

        // Load all ELF files into memory and find all globals
        let mut tls_primary = 0;
        let mut tls_size = 0;
        let mut mmaps = BTreeMap::new();
        let mut globals = BTreeMap::new();
        for (elf_name, elf) in elfs.iter() {
            println!("map {}", elf_name);

            let object = match self.objects.get(*elf_name) {
                Some(some) => some,
                None => continue,
            };

            // Calculate virtual memory bounds
            let bounds = {
                let mut bounds_opt: Option<(usize, usize)> = None;
                for ph in elf.program_headers.iter() {
                    let voff = ph.p_vaddr as usize % PAGE_SIZE;
                    let vaddr = ph.p_vaddr as usize - voff;
                    let vsize = ((ph.p_memsz as usize + voff + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;

                    match ph.p_type {
                        program_header::PT_LOAD => {
                            println!("  load {:#x}, {:#x}: {:x?}", vaddr, vsize, ph);

                            if let Some(ref mut bounds) = bounds_opt {
                                if vaddr < bounds.0 {
                                    bounds.0 = vaddr;
                                }
                                if vaddr + vsize > bounds.1 {
                                    bounds.1 = vaddr + vsize;
                                }
                            } else {
                                bounds_opt = Some((vaddr, vaddr + vsize));
                            }
                        },
                        program_header::PT_TLS => {
                            println!("  load tls {:#x}: {:x?}", vsize, ph);
                            tls_size += vsize;
                            if *elf_name == primary {
                                tls_primary += vsize;
                            }
                        },
                        _ => ()
                    }
                }
                match bounds_opt {
                    Some(some) => some,
                    None => continue,
                }
            };
            println!("  bounds {:#x}, {:#x}", bounds.0, bounds.1);

            // Allocate memory
            let mmap = unsafe {
                let size = bounds.1 /* - bounds.0 */;
                let ptr = sys_mman::mmap(
                    ptr::null_mut(),
                    size,
                    //TODO: Make it possible to not specify PROT_EXEC on Redox
                    sys_mman::PROT_READ | sys_mman::PROT_WRITE,
                    sys_mman::MAP_ANONYMOUS | sys_mman::MAP_PRIVATE,
                    -1,
                    0
                );
                if ptr as usize == !0 /* MAP_FAILED */ {
                    return Err(Error::Malformed(
                        format!("failed to map {}", elf_name)
                    ));
                }
                slice::from_raw_parts_mut(ptr as *mut u8, size)
            };
            println!("  mmap {:p}, {:#x}", mmap.as_mut_ptr(), mmap.len());

            // Locate all globals
            for sym in elf.dynsyms.iter() {
                if sym.st_bind() == sym::STB_GLOBAL && sym.st_value != 0 {
                    if let Some(name_res) = elf.dynstrtab.get(sym.st_name) {
                        let name = name_res?;
                        let value = mmap.as_ptr() as usize + sym.st_value as usize;
                        //println!("  global {}: {:x?} = {:#x}", name, sym, value);
                        globals.insert(name, value);
                    }
                }
            }

            mmaps.insert(elf_name, mmap);
        }

        // Allocate TLS
        let mut tls = unsafe { allocate_tls(tls_size)? };
        println!("tls {:p}, {:#x}", tls.as_mut_ptr(), tls.len());

        // Copy data
        let mut tls_offset = tls_primary;
        let mut tls_index = 0;
        let mut tls_ranges = BTreeMap::new();
        for (elf_name, elf) in elfs.iter() {
            let object = match self.objects.get(*elf_name) {
                Some(some) => some,
                None => continue,
            };

            let mmap = match mmaps.get_mut(elf_name) {
                Some(some) => some,
                None => continue
            };

            println!("load {}", elf_name);

            // Copy data
            for ph in elf.program_headers.iter() {
                let voff = ph.p_vaddr as usize % PAGE_SIZE;
                let vaddr = ph.p_vaddr as usize - voff;
                let vsize = ((ph.p_memsz as usize + voff + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;

                match ph.p_type {
                    program_header::PT_LOAD => {
                        let obj_data = {
                            let range = ph.file_range();
                            match object.get(range.clone()) {
                                Some(some) => some,
                                None => return Err(Error::Malformed(
                                    format!("failed to read {:?}", range)
                                )),
                            }
                        };

                        let mmap_data = {
                            let range = ph.p_vaddr as usize..ph.p_vaddr as usize + obj_data.len();
                            match mmap.get_mut(range.clone()) {
                                Some(some) => some,
                                None => return Err(Error::Malformed(
                                    format!("failed to write {:?}", range)
                                )),
                            }
                        };

                        println!("  copy {:#x}, {:#x}: {:#x}, {:#x}", vaddr, vsize, voff, obj_data.len());

                        mmap_data.copy_from_slice(obj_data);
                    },
                    program_header::PT_TLS => {
                        let valign = if ph.p_align > 0 {
                            ((ph.p_memsz + (ph.p_align - 1))/ph.p_align) * ph.p_align
                        } else {
                            ph.p_memsz
                        } as usize;

                        let obj_data = {
                            let range = ph.file_range();
                            match object.get(range.clone()) {
                                Some(some) => some,
                                None => return Err(Error::Malformed(
                                    format!("failed to read {:?}", range)
                                )),
                            }
                        };

                        let tls_data = {
                            let (index, start) = if *elf_name == primary {
                                (0, tls.len() - valign)
                            } else {
                                let start = tls.len() - (tls_offset + valign);
                                tls_offset += vsize;
                                tls_index += 1;
                                (tls_index, start)
                            };
                            let range = start..start + obj_data.len();
                            match tls.get_mut(range.clone()) {
                                Some(some) => {
                                    tls_ranges.insert(elf_name, (index, range));
                                    some
                                },
                                None => return Err(Error::Malformed(
                                    format!("failed to write tls {:?}", range)
                                )),
                            }
                        };

                        println!("  copy tls {:#x}, {:#x}: {:#x}, {:#x}", vaddr, vsize, voff, obj_data.len());

                        tls_data.copy_from_slice(obj_data);
                    },
                    _ => ()
                }
            }
        }

        // Perform relocations, and protect pages
        for (elf_name, elf) in elfs.iter() {
            let mmap = match mmaps.get_mut(elf_name) {
                Some(some) => some,
                None => continue
            };

            println!("link {}", elf_name);

            // Relocate
            for rel in elf.dynrelas.iter().chain(elf.dynrels.iter()).chain(elf.pltrelocs.iter()) {
                // println!("  rel {}: {:x?}",
                //     reloc::r_to_str(rel.r_type, elf.header.e_machine),
                //     rel
                // );

                let a = rel.r_addend.unwrap_or(0) as usize;

                let b = mmap.as_mut_ptr() as usize;

                let s = if rel.r_sym > 0 {
                    let sym = elf.dynsyms.get(rel.r_sym).ok_or(Error::Malformed(
                        format!("missing symbol for relocation {:?}", rel)
                    ))?;

                    let name = elf.dynstrtab.get(sym.st_name).ok_or(Error::Malformed(
                        format!("missing name for symbol {:?}", sym)
                    ))??;

                    if let Some(value) = globals.get(name) {
                        // println!("    sym {}: {:x?} = {:#x}", name, sym, value);
                        *value
                    } else {
                        // println!("    sym {}: {:x?} = undefined", name, sym);
                        0
                    }
                } else {
                    0
                };

                let t = if let Some((tls_index, tls_range)) = tls_ranges.get(elf_name) {
                    tls_range.start
                } else {
                    0
                };

                let ptr = unsafe {
                    mmap.as_mut_ptr().add(rel.r_offset as usize)
                };

                let set_u64 = |value| {
                    //println!("    set_u64 {:#x}", value);
                    unsafe { *(ptr as *mut u64) = value; }
                };

                match rel.r_type {
                    reloc::R_X86_64_64 => {
                        set_u64((s + a) as u64);
                    },
                    reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT => {
                        set_u64(s as u64);
                    },
                    reloc::R_X86_64_RELATIVE => {
                        set_u64((b + a) as u64);
                    },
                    reloc::R_X86_64_TPOFF64 => {
                        set_u64((s + a).wrapping_sub(t) as u64);
                    },
                    reloc::R_X86_64_IRELATIVE => (), // Handled below
                    _ => {
                        println!("    {} unsupported", reloc::r_to_str(rel.r_type, elf.header.e_machine));
                    }
                }
            }

            // Protect pages
            for ph in elf.program_headers.iter() {
                match ph.p_type {
                    program_header::PT_LOAD => {
                        let voff = ph.p_vaddr as usize % PAGE_SIZE;
                        let vaddr = ph.p_vaddr as usize - voff;
                        let vsize = ((ph.p_memsz as usize + voff + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;

                        let mut prot = 0;

                        if ph.p_flags & program_header::PF_R == program_header::PF_R {
                            prot |= sys_mman::PROT_READ;
                        }

                        // W ^ X. If it is executable, do not allow it to be writable, even if requested
                        if ph.p_flags & program_header::PF_X == program_header::PF_X {
                            prot |= sys_mman::PROT_EXEC;
                        } else if ph.p_flags & program_header::PF_W == program_header::PF_W {
                            prot |= sys_mman::PROT_WRITE;
                        }

                        let res = unsafe {
                            let ptr = mmap.as_mut_ptr().add(vaddr);
                            println!("  prot {:#x}, {:#x}: {:p}, {:#x}", vaddr, vsize, ptr, prot);

                            sys_mman::mprotect(
                                ptr as *mut c_void,
                                vsize,
                                prot
                            )
                        };

                        if res < 0 {
                            return Err(Error::Malformed(
                                format!("failed to mprotect {}", elf_name)
                            ));
                        }
                    },
                    _ => ()
                }
            }
        }

        // Perform indirect relocations (necessary evil), gather entry point
        let mut entry_opt = None;
        for (elf_name, elf) in elfs.iter() {
            let mmap = match mmaps.get_mut(elf_name) {
                Some(some) => some,
                None => continue
            };

            println!("entry {}", elf_name);

            let is_primary = *elf_name == primary;
            if is_primary {
                entry_opt = Some(mmap.as_mut_ptr() as usize + elf.header.e_entry as usize);
            }

            // Relocate
            for rel in elf.dynrelas.iter().chain(elf.dynrels.iter()).chain(elf.pltrelocs.iter()) {
                // println!("  rel {}: {:x?}",
                //     reloc::r_to_str(rel.r_type, elf.header.e_machine),
                //     rel
                // );

                let a = rel.r_addend.unwrap_or(0) as usize;

                let b = mmap.as_mut_ptr() as usize;

                let ptr = unsafe {
                    mmap.as_mut_ptr().add(rel.r_offset as usize)
                };

                let set_u64 = |value| {
                    // println!("    set_u64 {:#x}", value);
                    unsafe { *(ptr as *mut u64) = value; }
                };

                match rel.r_type {
                    reloc::R_X86_64_IRELATIVE => unsafe {
                        let f: unsafe extern "C" fn () -> u64 = mem::transmute(b + a);
                        set_u64(f());
                    },
                    _ => ()
                }
            }

            // Protect pages
            for ph in elf.program_headers.iter() {
                match ph.p_type {
                    program_header::PT_LOAD => {
                        let voff = ph.p_vaddr as usize % PAGE_SIZE;
                        let vaddr = ph.p_vaddr as usize - voff;
                        let vsize = ((ph.p_memsz as usize + voff + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;

                        let mut prot = 0;

                        if ph.p_flags & program_header::PF_R == program_header::PF_R {
                            prot |= sys_mman::PROT_READ;
                        }

                        // W ^ X. If it is executable, do not allow it to be writable, even if requested
                        if ph.p_flags & program_header::PF_X == program_header::PF_X {
                            prot |= sys_mman::PROT_EXEC;
                        } else if ph.p_flags & program_header::PF_W == program_header::PF_W {
                            prot |= sys_mman::PROT_WRITE;
                        }

                        let res = unsafe {
                            let ptr = mmap.as_mut_ptr().add(vaddr);
                            println!("  prot {:#x}, {:#x}: {:p}, {:#x}", vaddr, vsize, ptr, prot);

                            sys_mman::mprotect(
                                ptr as *mut c_void,
                                vsize,
                                prot
                            )
                        };

                        if res < 0 {
                            return Err(Error::Malformed(
                                format!("failed to mprotect {}", elf_name)
                            ));
                        }
                    },
                    _ => ()
                }
            }
        }

        entry_opt.ok_or(Error::Malformed(
            format!("missing entry for {}", primary)
        ))
    }
}