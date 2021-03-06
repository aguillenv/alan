use std::array::IntoIter;
use std::str;
use std::sync::Arc;

use byteorder::{ByteOrder, NativeEndian};

use crate::vm::{program::Program, protos, VMError, VMResult};

// -2^63
pub const CLOSURE_ARG_MEM_START: i64 = -9223372036854775808;
// The closure arg memory end has been extended to handle disambiguating nested closure arguments
// being used deep in the scope hierarchy. The quickest solution was to just increase that memory
// space to a large constant range, but the proper solution is to make this no longer a constant
// and determine the range based on the side of the global memory.
pub const CLOSURE_ARG_MEM_END: i64 = CLOSURE_ARG_MEM_START + 9001; // TODO: IT'S OVER 9000!
                                                                   // Flags for the registers_ish vector. The normal address flag indicates that the data is stored
                                                                   // normally in either the memory or fractal memory structures. The fixed pointer address flag
                                                                   // indicates that the value in the memory structure is actually a pointer to an i64 value. The
                                                                   // handlermemory pointer address flag indicates that the value in the memory structure is actually
                                                                   // a pointer to a HandlerMemory object.
const NORMAL_ADDR: i8 = 0;
const GMEM_ADDR: i8 = 1;
const ARGS_ADDR: i8 = 2;

fn addr_type(addr: i64) -> i8 {
  return if addr >= 0 {
    NORMAL_ADDR
  } else if addr <= CLOSURE_ARG_MEM_END {
    ARGS_ADDR
  } else {
    GMEM_ADDR
  };
}

/// Memory representation of a fractal memory block within HandlerMemory
#[derive(Clone, Debug)]
pub struct FractalMemory {
  /// address in HandlerMemory which is not present for actual data or deeply nested fractals
  hm_addr: Option<i64>,
  /// a memory block from HandlerMemory.mems
  block: Vec<(usize, i64)>,
  /// id for HandlerMemory that contains it, casted as usize from raw ptr
  hm_id: usize,
}

impl FractalMemory {
  pub fn new(block: Vec<(usize, i64)>) -> FractalMemory {
    return FractalMemory {
      hm_addr: None,
      hm_id: 0 as *const HandlerMemory as usize, // null ptr, mostly for fractal strings
      block,
    };
  }

  /// Determines this Fractal was read from the provided HandlerMemory
  pub fn belongs(self: &FractalMemory, hm: &Arc<HandlerMemory>) -> bool {
    return self.hm_id == 0 || self.hm_id == Arc::as_ptr(hm) as usize;
  }

  /// Length of memory block
  pub fn len(self: &FractalMemory) -> usize {
    return self.block.len();
  }

  /// Compare the blocks at a given index between two FractalMemory
  pub fn compare_at(self: &FractalMemory, idx: usize, other: &FractalMemory) -> bool {
    return self.block[idx] == other.block[idx];
  }

  /// Reads fixed data from a given address.
  pub fn read_fixed(self: &FractalMemory, idx: usize) -> VMResult<i64> {
    if self.block[idx].0 != usize::MAX {
      Err(VMError::IllegalAccess)
    } else {
      Ok(self.block[idx].1)
    }
  }
}

impl PartialEq for FractalMemory {
  fn eq(&self, other: &Self) -> bool {
    // ignore hm_addr
    self.block == other.block
  }
}

/// Memory representation of a handler call
#[derive(Clone, Debug)]
pub struct HandlerMemory {
  /// Optional parent pointer present in forked HandlerMemories
  parent: Option<Arc<HandlerMemory>>,
  /// The set of memory blocks. Each representing a fractal. The first (zeroth) block hosts global memory and all blocks
  /// afterwards host memory created by the handler. Each block consists of tuples of two values,
  /// representing either a virtual pointer or raw data, with three classes of values:
  /// 1. `(usize::MAX, any value)` - The first value indicates that the second value is actual raw
  ///    data
  /// 2. `(< usize::MAX, usize::MAX)` - The first value indicates that this is a virtual pointer to
  ///    nested memory. The second value indicates that the pointer is to an entire block of
  ///    memory, not an explicit value.
  /// 3. `(< usize::MAX, < usize::MAX)` - The first value indicates that this is a virtual pointer
  ///    to nested memory. The second value indicates that the pointer is to an explicit value
  ///    within that block of memory.
  /// Virtual pointers are simply the indexes into the `mems` field.
  mems: Vec<Vec<(usize, i64)>>,
  /// The virtual address spaces to an (optional) index pointing to mems that the handler can mutate.
  /// If the index is not defined in the current Handler Memory, it is in the parent Handler Memory.
  /// The first is the "normal" memory space, and the second is the args memory space.
  /// Global addresses are fixed for the application and do not need a mutable vector to parse.
  addr: (Vec<Option<(usize, usize)>>, Vec<Option<(usize, usize)>>),
  /// Specifies which memory block to push values into.
  mem_addr: usize,
}

impl HandlerMemory {
  /// Constructs a new HandlerMemory. If given another HandlerMemory it simply adjusts it to the
  /// expected memory needs, otherwise constructs a new one with said memory requirements.
  pub fn new(
    payload_mem: Option<Arc<HandlerMemory>>,
    mem_req: i64,
  ) -> VMResult<Arc<HandlerMemory>> {
    let mut hm = match payload_mem {
      Some(payload) => payload,
      None => Arc::new(HandlerMemory {
        mems: [Program::global().gmem.clone(), Vec::new()].to_vec(),
        addr: (Vec::new(), Vec::new()),
        mem_addr: 1,
        parent: None,
      }),
    };
    let handlermemory = Arc::get_mut(&mut hm).ok_or_else(|| VMError::HandMemDanglingPtr)?;
    handlermemory.mems[1].reserve(mem_req as usize);
    return Ok(hm);
  }

  /// Grabs the relevant data for the event and constructs a new HandlerMemory with that value in
  /// address 0, or returns no HandlerMemory if it is a void event.
  pub fn alloc_payload(
    event_id: i64,
    curr_addr: i64,
    curr_hand_mem: &Arc<HandlerMemory>,
  ) -> VMResult<Option<Arc<HandlerMemory>>> {
    let pls = Program::global()
      .event_pls
      .get(&event_id)
      .ok_or(VMError::EventNotDefined(event_id))?
      .clone();
    return if pls == 0 {
      // no payload, void event
      Ok(None)
    } else {
      let mut hm = HandlerMemory::new(None, 1)?;
      HandlerMemory::transfer(curr_hand_mem, curr_addr, &mut hm, 0)?;
      Ok(Some(hm))
    };
  }

  /// Drop the immutable, thread-safe reference to the parent provided by `fork`.
  /// Consuming forked children is a mutable operation on the parent
  /// that cannot be performed on an object within an Arc. As a result,
  /// all the forked children drop their reference to the parent, we take
  /// the parent out of the Arc after parallel work is done and then join on its children.
  pub fn drop_parent(mut self: Arc<HandlerMemory>) -> VMResult<Arc<HandlerMemory>> {
    let hm = Arc::get_mut(&mut self).ok_or(VMError::HandMemDanglingPtr)?;
    hm.parent.take();
    Ok(self)
  }

  /// Returns true if the idxs are valid in self
  fn is_idx_defined(self: &HandlerMemory, a: usize, b: usize) -> bool {
    let is_raw = a == std::usize::MAX;
    let safe_mem_space = self.mem_addr == 1 || a >= self.mem_addr || self.mems[a].len() > 0;
    let is_fixed = safe_mem_space && self.mems.len() > a && self.mems[a].len() > b;
    let is_fractal = safe_mem_space && self.mems.len() > a && b == std::usize::MAX;
    return is_raw || is_fixed || is_fractal;
  }

  /// Recursively finds the parent and returns None if the idxs
  /// belong to self or otherwise a reference to the parent
  fn hm_for_idxs(self: &Arc<HandlerMemory>, a: usize, b: usize) -> Option<Arc<HandlerMemory>> {
    if self.is_idx_defined(a, b) {
      return None;
    }
    match self.parent.as_ref() {
      Some(parent) => parent.hm_for_idxs(a, b).or(Some(parent.clone())),
      None => None,
    }
  }

  /// Takes a given address and looks up the fractal location and
  /// `mems` indexes relevant to it if available
  pub fn addr_to_idxs_opt(self: &HandlerMemory, addr: i64) -> Option<(usize, usize)> {
    return if addr >= 0 {
      *self.addr.0.get(addr as usize).unwrap_or(&None)
    } else if addr <= CLOSURE_ARG_MEM_END {
      *self
        .addr
        .1
        .get((addr - CLOSURE_ARG_MEM_START) as usize)
        .unwrap_or(&None)
    } else {
      Some((0, ((-1 * addr - 1) / 8) as usize))
    };
  }

  /// Recursively takes a given address and looks up the fractal location and
  /// `mems` indexes relevant to it. It also returns an Option that is
  /// None if the address is in self or a ptr to the ancestor if `self` is forked
  // TODO: determine if we have to do the following:
  // if addr < self.mem_addr { copy_from_parent };
  fn addr_to_idxs(
    self: &Arc<HandlerMemory>,
    addr: i64,
  ) -> VMResult<((usize, usize), Option<Arc<HandlerMemory>>)> {
    return match self.addr_to_idxs_opt(addr) {
      Some(res) => Ok((res, self.hm_for_idxs(res.0, res.1))),
      None => {
        let (idxs, hm_opt) = self
          .parent
          .as_ref()
          // fail if no parent
          .ok_or(VMError::OrphanMemory)?
          .addr_to_idxs(addr)?;
        let hm = match hm_opt {
          Some(hm) => Some(hm),
          None => self.parent.clone(),
        };
        Ok((idxs, hm))
      }
    };
  }

  /// Reads fixed data from a given address.
  pub fn read_fixed(self: &Arc<HandlerMemory>, addr: i64) -> VMResult<i64> {
    let ((a, b), hm_opt) = self.addr_to_idxs(addr)?;
    return if a == std::usize::MAX {
      Ok(b as i64)
    } else {
      let hm = hm_opt.as_ref().unwrap_or(self).as_ref();
      Ok(hm.mems[a][b].1)
    };
  }

  /// Reads an array of data from the given address.
  pub fn read_fractal(self: &Arc<HandlerMemory>, addr: i64) -> VMResult<FractalMemory> {
    let ((a, b), hm_opt) = self.addr_to_idxs(addr)?;
    // eprintln!("addr: {}, self?: {}, (a,b): ({},{})", addr, hm_opt.is_none(), a, b);
    let hm = hm_opt.as_ref().unwrap_or(self);
    // Special behavior to read strings out of global memory
    let start = if addr_type(addr) == GMEM_ADDR { b } else { 0 };
    return Ok(FractalMemory {
      hm_addr: Some(addr),
      block: hm.mems[a][start..].to_vec(),
      hm_id: Arc::as_ptr(hm) as usize,
    });
  }

  /// Provides a mutable array of data from the given address.
  pub fn read_mut_fractal<'mem>(
    self: &'mem mut Arc<HandlerMemory>,
    addr: i64,
  ) -> VMResult<&'mem mut Vec<(usize, i64)>> {
    let ((a, _), hm_opt) = self.addr_to_idxs(addr)?;
    if let Some(hm) = hm_opt {
      // copy necessary data from ancestor
      Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?.mems[a] = hm.mems[a].clone();
    }
    Ok(&mut Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?.mems[a])
  }

  /// You better know what you're doing if you use this
  pub fn read_mut_fractal_by_idx<'mem>(
    self: &'mem mut Arc<HandlerMemory>,
    a: usize,
  ) -> VMResult<&'mem mut Vec<(usize, i64)>> {
    if let Some(hm) = self.hm_for_idxs(a, std::usize::MAX) {
      Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?.mems[a] = hm.mems[a].clone();
    }
    Ok(&mut Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?.mems[a])
  }

  /// For a given address, determines if the data is a single value or an array of values, and
  /// returns that value either as a vector or the singular value wrapped in a vector, and a
  /// boolean indicating if it was a fractal value or not.
  pub fn read_either(self: &Arc<HandlerMemory>, addr: i64) -> VMResult<(FractalMemory, bool)> {
    let ((a, b), hm_opt) = self.addr_to_idxs(addr)?;
    let hm = match hm_opt.as_ref() {
      Some(hm) => hm,
      None => self,
    };
    let (block, is_fractal) = if b < std::usize::MAX {
      (vec![hm.mems[a][b].clone()], false)
    } else {
      (hm.mems[a].clone(), true)
    };
    return Ok((
      FractalMemory {
        hm_addr: Some(addr),
        block,
        hm_id: Arc::as_ptr(hm) as usize,
      },
      is_fractal,
    ));
  }

  /// Simply sets a given address to an explicit set of `mems` indexes. Simplifies pointer creation
  /// to deeply-nested data.
  fn set_addr(self: &mut Arc<HandlerMemory>, addr: i64, a: usize, b: usize) -> VMResult<()> {
    let hm = Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?;
    if addr_type(addr) == NORMAL_ADDR {
      let addru = addr as usize;
      if hm.addr.0.len() <= addru {
        hm.addr.0.resize(addru + 1, None);
      }
      hm.addr.0[addru] = Some((a, b));
    } else {
      let addru = (addr - CLOSURE_ARG_MEM_START) as usize;
      if hm.addr.1.len() <= addru {
        hm.addr.1.resize(addru + 1, None);
      }
      hm.addr.1[addru] = Some((a, b));
    }
    Ok(())
  }

  /// For the memory block(s) starting at idx in Fractal, determines if the data is a single value or an array of
  /// values, and returns that value either as a vector or the singular value wrapped in a vector,
  /// and a boolean indicating if it is a fractal value or not.
  pub fn read_from_fractal(
    self: &Arc<HandlerMemory>,
    fractal: &FractalMemory,
    idx: usize,
  ) -> (FractalMemory, bool) {
    let (a, b) = fractal.block[idx];
    let b_usize = b as usize;
    let hm_opt = self.hm_for_idxs(a, b_usize);
    let hm = match hm_opt.as_ref() {
      Some(hm) => hm,
      None => self,
    };
    return if a == std::usize::MAX {
      // The indexes are the actual data
      (FractalMemory::new(vec![(a, b)]), false)
    } else {
      let (block, is_fractal) = if b_usize < std::usize::MAX {
        // The indexes point to fixed data
        (vec![hm.mems[a][b_usize].clone()], false)
      } else {
        // b_usize == std::usize::MAX
        // The indexes point at nested data
        (hm.mems[a].clone(), true)
      };
      (
        FractalMemory {
          hm_addr: None,
          block,
          hm_id: Arc::as_ptr(hm) as usize,
        },
        is_fractal,
      )
    };
  }

  /// Stores a nested fractal of data in a given address.
  pub fn write_fixed_in_fractal(
    self: &mut Arc<HandlerMemory>,
    fractal: &mut FractalMemory,
    idx: usize,
    val: i64,
  ) -> VMResult<()> {
    fractal.block[idx].1 = val;
    match (fractal.belongs(self), fractal.hm_addr) {
      (true, Some(addr)) => self.write_fractal(addr, fractal),
      _ => Err(VMError::MemoryNotOwned),
    }
  }

  /// Stores a fixed value in a given address. Determines where to place it based on the kind of
  /// address in question.
  pub fn write_fixed(self: &mut Arc<HandlerMemory>, addr: i64, val: i64) -> VMResult<()> {
    let a = self.mem_addr;
    let hm = Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?;
    let b = hm.mems[a].len();
    hm.mems[a].push((std::usize::MAX, val));
    self.set_addr(addr, a, b)
  }

  /// Stores a nested fractal of data in a given address.
  pub fn write_fractal(
    self: &mut Arc<HandlerMemory>,
    addr: i64,
    fractal: &FractalMemory,
  ) -> VMResult<()> {
    let a = self.mems.len();
    if !fractal.belongs(self) {
      if fractal.hm_addr.is_none() {
        return Err(VMError::MemoryNotOwned);
      }
      // copy fractal from ancestor
      let addr = fractal
        .hm_addr
        .as_ref()
        .ok_or(VMError::OrphanMemory)?
        .clone();
      let ((a, _), hm_opt) = self.addr_to_idxs(addr)?;
      let hm = hm_opt.ok_or(VMError::IllegalAccess)?;
      Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?.mems[a] = hm.mems[a].clone();
      drop(hm);
      todo!("Writing to a fractal that a child scope has access to mutably but never acquired mutably. Please report this error!")
    }
    let mut_self = Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?;
    mut_self.mems.push(fractal.block.clone());
    self.set_addr(addr, a, std::usize::MAX)
  }

  /// Stores a nested empty fractal of data in a given address.
  pub fn init_fractal(self: &mut Arc<HandlerMemory>, addr: i64) -> VMResult<()> {
    let a = self.mems.len();
    Arc::get_mut(self)
      .ok_or(VMError::HandMemDanglingPtr)?
      .mems
      .push(Vec::new());
    self.set_addr(addr, a, std::usize::MAX)
  }

  /// Pushes a fixed value into a fractal at a given address.
  pub fn push_fixed(self: &mut Arc<HandlerMemory>, addr: i64, val: i64) -> VMResult<()> {
    let mem = self.read_mut_fractal(addr)?;
    mem.push((std::usize::MAX, val));
    Ok(())
  }

  /// Pushes a nested fractal value into a fractal at a given address.
  pub fn push_fractal(
    self: &mut Arc<HandlerMemory>,
    addr: i64,
    val: FractalMemory,
  ) -> VMResult<()> {
    let a = self.mems.len();
    let mem = self.read_mut_fractal(addr)?;
    mem.push((a, std::usize::MAX as i64));
    Arc::get_mut(self)
      .ok_or(VMError::HandMemDanglingPtr)?
      .mems
      .push(val.block);
    Ok(())
  }

  /// Pops a value off of the fractal. May be fixed data or a virtual pointer.
  pub fn pop(self: &mut Arc<HandlerMemory>, addr: i64) -> VMResult<FractalMemory> {
    let mem = self.read_mut_fractal(addr)?;
    if mem.len() > 0 {
      return Ok(FractalMemory::new(vec![mem
        .pop()
        // if pop returned a None, that means something very bad has happened.
        .ok_or(VMError::IllegalAccess)?]));
    } else {
      return Err(VMError::Other(format!("cannot pop empty array")));
    }
  }

  /// Deletes a value off of the fractal at the given idx. May be fixed data or a virtual pointer.
  pub fn delete(self: &mut Arc<HandlerMemory>, addr: i64, idx: usize) -> VMResult<FractalMemory> {
    let mem = self.read_mut_fractal(addr)?;
    if mem.len() > 0 && mem.len() > idx {
      return Ok(FractalMemory::new(vec![mem.remove(idx)]));
    } else {
      return Err(VMError::Other(format!(
        "cannot remove idx {} from array with length {}",
        idx,
        mem.len()
      )));
    }
  }

  /* REGISTER MANIPULATION METHODS */

  /// Creates a pointer from `orig_addr` to `addr`
  pub fn register(
    self: &mut Arc<HandlerMemory>,
    addr: i64,
    orig_addr: i64,
    is_variable: bool,
  ) -> VMResult<()> {
    let ((a, b), _) = self.addr_to_idxs(orig_addr)?;
    if addr_type(orig_addr) == GMEM_ADDR && is_variable {
      // Special behavior to read strings out of global memory
      let string =
        HandlerMemory::fractal_to_string(FractalMemory::new(self.mems[a][b..].to_vec()))?;
      self.write_fractal(addr, &HandlerMemory::str_to_fractal(&string))
    } else {
      self.set_addr(addr, a, b)
    }
  }

  /// Pushes a pointer from `orig_addr` address into the fractal at `addr`.
  pub fn push_register(self: &mut Arc<HandlerMemory>, addr: i64, orig_addr: i64) -> VMResult<()> {
    let ((a, b), _) = self.addr_to_idxs(orig_addr)?;
    // Special path for strings in global memory which is the same for parent and self
    if a == 0 {
      let strmem = self.mems[0][b..].to_vec().clone();
      let new_a = self.mems.len();
      Arc::get_mut(self)
        .ok_or(VMError::HandMemDanglingPtr)?
        .mems
        .push(strmem);
      let mem = self.read_mut_fractal(addr)?;
      mem.push((new_a, std::usize::MAX as i64));
    } else {
      let mem = self.read_mut_fractal(addr)?;
      mem.push((a, b as i64));
    }
    Ok(())
  }

  /// Creates a pointer from `orig_addr` to index/offset `offset_addr` of fractal in `fractal_addr`
  pub fn register_in(
    self: &mut Arc<HandlerMemory>,
    orig_addr: i64,
    fractal_addr: i64,
    offset_addr: i64,
  ) -> VMResult<()> {
    let ((a, b), _) = self.addr_to_idxs(orig_addr)?;
    let mem = self.read_mut_fractal(fractal_addr)?;
    mem[offset_addr as usize] = (a, b as i64);
    Ok(())
  }

  /// Creates a pointer from index/offset `offset_addr` of fractal in `fractal_addr` to `out_addr`
  /// The inverse of `register_in`
  pub fn register_out(
    self: &mut Arc<HandlerMemory>,
    fractal_addr: i64,
    offset_addr: usize,
    out_addr: i64,
  ) -> VMResult<()> {
    let ((arr_a, _), _) = self.addr_to_idxs(fractal_addr)?;
    let fractal = self.read_fractal(fractal_addr)?;
    let (a, b) = fractal.block[offset_addr];
    if a < std::usize::MAX {
      self.set_addr(out_addr, a, b as usize)
    } else {
      self.set_addr(out_addr, arr_a, offset_addr)
    }
  }

  /// Creates a pointer from index/offset `idx` in FractalMemory to `out_addr`
  /// Used for deeply nested fractals in which case `register_out` can't be used
  pub fn register_from_fractal(
    self: &mut Arc<HandlerMemory>,
    out_addr: i64,
    fractal: &FractalMemory,
    idx: usize,
  ) -> VMResult<()> {
    let (a, b) = fractal.block[idx];
    self.set_addr(out_addr, a, b as usize)
  }

  /// Pushes a pointer from index/offset `offset_addr` of FractalMemory to fractal at `out_addr`
  pub fn push_register_out(
    self: &mut Arc<HandlerMemory>,
    out_addr: i64,
    fractal: &FractalMemory,
    offset_addr: usize,
  ) -> VMResult<()> {
    let mem = self.read_mut_fractal(out_addr)?;
    mem.push(fractal.block[offset_addr]);
    Ok(())
  }

  /* DATA TRANSFER, FORKING AND DUPLICATION METHODS */

  /// Migrates data from one HandlerMemory at a given address to another HandlerMemory at another
  /// address. Used by many things.
  pub fn transfer(
    origin: &Arc<HandlerMemory>,
    orig_addr: i64,
    dest: &mut Arc<HandlerMemory>,
    dest_addr: i64,
  ) -> VMResult<()> {
    let ((a, b), hm_opt) = origin.addr_to_idxs(orig_addr)?;
    let orig = match hm_opt.as_ref() {
      Some(orig) => orig,
      None => origin,
    };
    return HandlerMemory::transfer_idxs(orig, a, b, dest, dest_addr);
  }

  pub fn transfer_idxs(
    orig: &Arc<HandlerMemory>,
    a: usize,
    b: usize,
    dest: &mut Arc<HandlerMemory>,
    dest_addr: i64,
  ) -> VMResult<()> {
    if a == 0 {
      // Special behavior for global memory transfers since it may be a single value or a string
      let mem_slice = &orig.mems[a][b..];
      // To make this distinction we're gonna do some tests on the memory and see if it evals as a
      // string or not. There is some ridiculously small possibility that this is going to make a
      // false positive though so TODO: either make global memory unambiguous or update all uses of
      // this function to provide a type hint.
      let len = mem_slice[0].1 as usize;
      if len == 0 {
        // Assume zero is not a string
        return dest.write_fixed(dest_addr, mem_slice[0].1);
      }
      let mut s_bytes: Vec<u8> = Vec::new();
      for i in 1..mem_slice.len() {
        let mut b = mem_slice[i].1.clone().to_ne_bytes().to_vec();
        s_bytes.append(&mut b);
      }
      if len > s_bytes.len() {
        // Absolutely not correct
        return dest.write_fixed(dest_addr, mem_slice[0].1);
      }
      match str::from_utf8(&s_bytes[0..len]) {
        // Well, waddaya know!
        Ok(string) => return dest.write_fractal(dest_addr, &HandlerMemory::str_to_fractal(string)),
        // Also not a string
        Err(_) => dest.write_fixed(dest_addr, mem_slice[0].1)?,
      };
    }
    return if a == std::usize::MAX {
      // It's direct fixed data, just copy it over
      dest.write_fixed(dest_addr, b as i64)
    } else if a < std::usize::MAX && (b as usize) < std::usize::MAX {
      // All pointers are made shallow, so we know this is a pointer to a fixed value and just
      // grab it and de-reference it.
      let (_, b_nest) = orig.mems[a][b as usize];
      dest.write_fixed(dest_addr, b_nest)
    } else if a < std::usize::MAX && b as usize == std::usize::MAX {
      // It's a nested array of data. This may itself contain references to other nested arrays of
      // data and is relatively complex to transfer over. First create some lookup vectors and
      // populate them with the nested fractal, adding more and more as each fractal is checked
      // until no new ones are added
      let mut check_idx = 0;
      let mut orig_arr_addrs: Vec<usize> = vec![a];
      let mut orig_arr_copies: Vec<Vec<(usize, i64)>> = vec![orig.mems[a].clone()];
      while check_idx < orig_arr_addrs.len() {
        let arr = &orig_arr_copies[check_idx];
        let l = arr.len();
        drop(arr);
        for i in 0..l {
          let other_arr_idx = orig_arr_copies[check_idx][i].0.clone();
          if other_arr_idx < std::usize::MAX {
            if !orig_arr_addrs.contains(&other_arr_idx) {
              orig_arr_addrs.push(other_arr_idx);
              orig_arr_copies.push(orig.mems[other_arr_idx].clone());
            }
          }
        }
        check_idx = check_idx + 1;
      }
      // Next, get the current size of the destination mems vector to use as the offset to add to
      // the index of the copied arrays, updating their interior references, if any, in the process
      let dest_offset = dest.mems.len();
      for i in 0..orig_arr_copies.len() {
        let arr = &mut orig_arr_copies[i];
        for j in 0..arr.len() {
          let (a, b) = arr[j];
          if a < std::usize::MAX {
            for k in 0..orig_arr_addrs.len() {
              if orig_arr_addrs[k] == a {
                arr[j] = (dest_offset + k, b);
              }
            }
          }
        }
      }
      Arc::get_mut(dest)
        .ok_or(VMError::HandMemDanglingPtr)?
        .mems
        .append(&mut orig_arr_copies);
      // Finally, set the destination address to point at the original, main nested array
      dest.set_addr(dest_addr, dest_offset, std::usize::MAX)
    } else {
      Ok(())
    };
  }

  /// Creates a duplicate of data at one address in the HandlerMemory in a new address. Makes the
  /// `clone` function in Alan possible.
  pub fn dupe(self: &mut Arc<HandlerMemory>, orig_addr: i64, dest_addr: i64) -> VMResult<()> {
    // This *should be possible with something like this:
    // HandlerMemory::transfer(self, orig_addr, self, dest_addr);
    // But Rust's borrow checker doesn't like it, so we basically have to replicate the code here
    // despite the fact that it should work just fine...
    let ((a, b), _) = self.addr_to_idxs(orig_addr)?;
    return if a == std::usize::MAX {
      self.write_fixed(dest_addr, b as i64)
    } else if a < std::usize::MAX && (b as usize) < std::usize::MAX {
      // All pointers are made shallow, so we know this is a pointer to a fixed value and just
      // grab it and de-reference it.
      let (_, b_nest) = self.mems[a][b];
      self.write_fixed(dest_addr, b_nest)
    } else if a < std::usize::MAX && b as usize == std::usize::MAX {
      // It's a nested array of data. This may itself contain references to other nested arrays of
      // data and is relatively complex to transfer over. First create some lookup vectors and
      // populate them with the nested fractal, adding more and more as each fractal is checked
      // until no new ones are added
      let mut check_idx = 0;
      let mut orig_arr_addrs: Vec<usize> = vec![a];
      let mut orig_arr_copies: Vec<Vec<(usize, i64)>> =
        vec![self.read_fractal(orig_addr)?.block.clone()];
      while check_idx < orig_arr_addrs.len() {
        let arr = &orig_arr_copies[check_idx];
        let l = arr.len();
        drop(arr);
        for i in 0..l {
          let other_arr_idx = orig_arr_copies[check_idx][i].0.clone();
          if other_arr_idx < std::usize::MAX {
            if !orig_arr_addrs.contains(&other_arr_idx) {
              orig_arr_addrs.push(other_arr_idx);
              orig_arr_copies.push(self.mems[other_arr_idx].clone());
            }
          }
        }
        check_idx = check_idx + 1;
      }
      // Next, get the current size of the destination mems vector to use as the offset to add to
      // the index of the copied arrays, updating their interior references, if any, in the process
      let dest_offset = self.mems.len();
      for i in 0..orig_arr_copies.len() {
        let arr = &mut orig_arr_copies[i];
        for j in 0..arr.len() {
          let (a, b) = arr[j];
          if a < std::usize::MAX {
            for k in 0..orig_arr_addrs.len() {
              if orig_arr_addrs[k] == a {
                arr[j] = (dest_offset + k, b);
              }
            }
          }
        }
      }
      Arc::get_mut(self)
        .ok_or(VMError::HandMemDanglingPtr)?
        .mems
        .append(&mut orig_arr_copies);
      // Finally, set the destination address to point at the original, main nested array
      self.set_addr(dest_addr, dest_offset, std::usize::MAX)
    } else {
      Ok(())
    };
  }

  /// Returns a new HandlerMemory with a read-only reference to HandlerMemory as parent
  pub fn fork(parent: Arc<HandlerMemory>) -> VMResult<Arc<HandlerMemory>> {
    let s = parent.mems.len();
    let mut hm = HandlerMemory::new(None, 1)?;
    let handmem = Arc::get_mut(&mut hm).ok_or(VMError::HandMemDanglingPtr)?;
    handmem.parent = Some(parent);
    handmem.mems.resize(s + 1, Vec::new());
    handmem.mem_addr = s;
    return Ok(hm);
  }

  /// Joins two HandlerMemory structs back together. Assumes that the passed in handler memory was
  /// generated by a fork call. This process moves over the records created in the forked HandlerMemory
  /// into the original and then "stitches up" the virtual memory pointers for anything pointing at
  /// newly-created data. This mechanism is faster but will keep around unreachable memory for longer.
  /// Whether or not this is the right trade-off will have to be seen by real-world usage.
  ///
  /// Since the HandlerMemory has been transfered back into the original, this method assumes that
  /// the atomic reference is the *only one*, and consumes it so that it can't be used again.
  pub fn join(self: &mut Arc<HandlerMemory>, mut hm: Arc<HandlerMemory>) -> VMResult<()> {
    let s = hm.mem_addr; // The initial block that will be transferred (plus all following blocks)
    let s2 = self.mems.len(); // The new address of the initial block
    let offset = s2 - s; // Assuming it was made by `fork` this should be positive or zero
    if let Some((a, b)) = hm.addr_to_idxs_opt(CLOSURE_ARG_MEM_START) {
      // The only address that can "escape"
      let off = if a < std::usize::MAX && a >= s {
        offset
      } else {
        0
      };
      self.set_addr(CLOSURE_ARG_MEM_START, a + off, b)?;
    };

    // println!("a: {}, b: {}, s: {}, in_fork: {}, in_parent: {}", a, b, s, hm.is_idx_defined(a, b), self.is_idx_defined(a, b));
    let hm = Arc::get_mut(&mut hm).ok_or(VMError::HandMemDanglingPtr)?;
    let parent = Arc::get_mut(self).ok_or(VMError::HandMemDanglingPtr)?;
    let updated_prev_mems = hm.mems.drain(..s);
    for (i, updated_prev_mem) in updated_prev_mems.enumerate() {
      if updated_prev_mem.len() > 0 && i > 0 {
        // This was a mutated clone of parent data, switch to the mutated form
        parent.mems[i] = updated_prev_mem;
      }
    }
    // Append the relevant ones to the original HandlerMemory
    parent.mems.append(&mut hm.mems);

    // Set the return address on the original HandlerMemory to the acquired indexes, potentially
    // offset if it is a pointer at new data

    // Similarly "stitch up" every pointer in the moved data with a pass-through scan and update
    let l = parent.mems.len();
    for i in s2..l {
      let mem = &mut parent.mems[i];
      for j in 0..mem.len() {
        let (a, b) = mem[j];
        if a < std::usize::MAX && a >= s {
          mem[j] = (a + offset, b);
        }
      }
    }
    // Finally pull any addresses added by the old object into the new with a similar stitching
    if hm.addr.0.len() > parent.addr.0.len() {
      parent.addr.0.resize(hm.addr.0.len(), None);
    }
    for i in 0..hm.addr.0.len() {
      if let Some((a, b)) = hm.addr.0[i] {
        match parent.addr.0[i] {
          Some((c, d)) if (a != c || b != d) => {
            if a + offset >= s && a != std::usize::MAX {
              parent.addr.0[i] = Some((a + offset, b))
            } else {
              parent.addr.0[i] = Some((a, b))
            }
          }
          Some((c, _)) if a == c => parent.addr.0[i] = Some((a, b)),
          Some(_) => {
            // TODO: Since we now `fork`/`join` on IO opcodes, we need to figure out how to handle
            // the case where all children edit the same value. Right now, we work around this
            // in a couple ways:
            // 1. parallelized io opcode closures will refuse to compile if they edit a variable
            // 2. linear io opcodes *can* edit external variables, but since they can only be
            //    executed in their own batch, we work around this by only `fork`/`join`ing
            //    for batches with >1 IO opcode.
            // However, as we start increasing efficiency in the AVM, we may eventually come to a
            // point where we actually *do* have to merge the parent/children memories. Until then,
            // the workarounds should be fine.
            return Err(VMError::Other("Unable to merge memories".to_string()));
          }
          None => parent.addr.0[i] = Some((a + offset, b)),
        }
      }
    }
    Ok(())
  }

  /// Takes a UTF-8 string and converts it to fractal memory that can be stored inside of a
  /// HandlerMemory. Alan stores strings as Pascal strings with a 64-bit length prefix. There is no
  /// computer on the planet that has 64-bits worth of RAM, so this should work for quite a while
  /// into the future. :)
  pub fn str_to_fractal(s: &str) -> FractalMemory {
    let mut s_mem = vec![(std::usize::MAX, s.len() as i64)];
    let mut s_bytes = s.as_bytes().to_vec();
    loop {
      if s_bytes.len() % 8 != 0 {
        s_bytes.push(0);
      } else {
        break;
      }
    }
    let mut i = 0;
    loop {
      if i < s_bytes.len() {
        let s_slice = &s_bytes[i..i + 8];
        s_mem.push((std::usize::MAX, NativeEndian::read_i64(s_slice)));
        i = i + 8;
      } else {
        break;
      }
    }
    FractalMemory::new(s_mem)
  }

  /// Takes a fractal memory and treats it like a UTF-8 encoded Pascal string, and the converts it
  /// to something Rust can work with. This function *may* crash if the underlying data is not a
  /// UTF-8 encoded Pascal string.
  pub fn fractal_to_string(f: FractalMemory) -> VMResult<String> {
    let s_len = f.block[0].1 as usize;
    let s_bytes = f
      .block
      .iter()
      .skip(1)
      // TODO: `IntoIter::new` will be deprecated once `rust-lang/rust#65819` is merged
      .flat_map(|(_, chars)| IntoIter::new(chars.to_ne_bytes()))
      .collect::<Vec<u8>>();
    let s = str::from_utf8(&s_bytes[0..s_len]).map_err(|_| VMError::InvalidString)?;
    Ok(s.to_string())
  }

  /// Returns a new Protobuf HandlerMemory from an existing HandlerMemory
  pub fn to_pb(self: &Arc<HandlerMemory>) -> protos::HandlerMemory::HandlerMemory {
    let mut proto_hm = protos::HandlerMemory::HandlerMemory::new();
    set_pb_mems(self, &mut proto_hm);
    set_pb_addr(self, &mut proto_hm);
    if let Some(parent) = &self.parent {
      proto_hm.set_parent(HandlerMemory::to_pb(&parent));
    }
    proto_hm.set_mem_addr(self.mem_addr as u64);
    proto_hm
  }

  /// Returns a HandlerMemory from a new Protobuf HandlerMemory
  pub fn from_pb(proto_hm: &protos::HandlerMemory::HandlerMemory) -> VMResult<Arc<HandlerMemory>> {
    let mut hm = HandlerMemory::new(None, 1)?;
    let mut hm_mut = Arc::get_mut(&mut hm).ok_or(VMError::HandMemDanglingPtr)?;
    set_mems_from_pb(&proto_hm, hm_mut);
    set_addr_from_pb(&proto_hm, hm_mut);
    if proto_hm.has_parent() {
      let parent = proto_hm.get_parent();
      hm_mut.parent = Some(HandlerMemory::from_pb(&parent)?);
    }
    hm_mut.mem_addr = proto_hm.get_mem_addr() as usize;
    Ok(hm)
  }
}

/// Sets mems HandlerMemory attribute from Protobuf HandlerMemory struct
fn set_mems_from_pb(proto_hm: &protos::HandlerMemory::HandlerMemory, hm: &mut HandlerMemory) {
  // Set global memory first
  let mut mems = [Program::global().gmem.clone()].to_vec();
  for pb_mem in proto_hm.get_mems() {
    let mut mem = Vec::new();
    for mem_block in pb_mem.get_mem() {
      mem.push((mem_block.get_mem_type() as usize, mem_block.get_mem_val()));
    }
    mems.push(mem);
  }
  hm.mems = mems;
}

/// Sets mems Protobuf HandlerMemory attribute from HandlerMemory struct
fn set_pb_mems(hm: &Arc<HandlerMemory>, proto_hm: &mut protos::HandlerMemory::HandlerMemory) {
  let mut mem_vec: protobuf::RepeatedField<protos::HandlerMemory::HandlerMemory_Mems> =
    protobuf::RepeatedField::new();
  for (i, hm_inner_vec) in hm.mems.iter().enumerate() {
    // Ignore global memory
    if i == 0 {
      continue;
    };
    let mut inner_vec: protobuf::RepeatedField<protos::HandlerMemory::HandlerMemory_MemBlock> =
      protobuf::RepeatedField::new();
    for hm_mem_block in hm_inner_vec.iter() {
      let mut mem_block = protos::HandlerMemory::HandlerMemory_MemBlock::new();
      mem_block.set_mem_type(hm_mem_block.0 as u64);
      mem_block.set_mem_val(hm_mem_block.1 as i64);
      inner_vec.push(mem_block);
    }
    let mut mem = protos::HandlerMemory::HandlerMemory_Mems::new();
    mem.set_mem(inner_vec);
    mem_vec.push(mem);
  }
  proto_hm.set_mems(mem_vec);
}

/// Sets addr HandlerMemory attribute from Protobuf HandlerMemory struct
fn set_addr_from_pb(proto_hm: &protos::HandlerMemory::HandlerMemory, hm: &mut HandlerMemory) {
  let mut mem_space = Vec::new();
  let mut mem_space_args = Vec::new();
  complete_mem_space_from_pb(&mut mem_space, &proto_hm.get_addr().get_mem_space());
  complete_mem_space_from_pb(
    &mut mem_space_args,
    &proto_hm.get_addr().get_mem_space_args(),
  );
  hm.addr = (mem_space, mem_space_args);
}

/// Sets addr Protobuf HandlerMemory attribute from HandlerMemory struct
fn set_pb_addr(hm: &Arc<HandlerMemory>, proto_hm: &mut protos::HandlerMemory::HandlerMemory) {
  let mut addr = protos::HandlerMemory::HandlerMemory_Addr::new();
  let mut mem_space_vec: protobuf::RepeatedField<protos::HandlerMemory::HandlerMemory_MemSpace> =
    protobuf::RepeatedField::new();
  let mut mem_space_args_vec: protobuf::RepeatedField<
    protos::HandlerMemory::HandlerMemory_MemSpace,
  > = protobuf::RepeatedField::new();
  complete_pb_mem_space(&mut mem_space_vec, &(hm.addr.0));
  complete_pb_mem_space(&mut mem_space_args_vec, &(hm.addr.1));
  addr.set_mem_space(mem_space_vec);
  addr.set_mem_space_args(mem_space_args_vec);
  proto_hm.set_addr(addr);
}

/// Completes Protobuf HandlerMemory mem space vector from HandlerMemory
fn complete_pb_mem_space(
  mem_space_vec: &mut protobuf::RepeatedField<protos::HandlerMemory::HandlerMemory_MemSpace>,
  hm_addr: &Vec<Option<(usize, usize)>>,
) {
  for hm_mem_space in hm_addr.iter() {
    let mut mem_space = protos::HandlerMemory::HandlerMemory_MemSpace::new();
    if let Some(hm_mem_space) = hm_mem_space {
      let mut mem_space_struct = protos::HandlerMemory::HandlerMemory_MemSpaceStruct::new();
      mem_space_struct.set_first(hm_mem_space.0 as u64);
      mem_space_struct.set_second(hm_mem_space.1 as u64);
      mem_space.set_memspacestruct(mem_space_struct);
    } else {
      mem_space.clear_memspacestruct();
    }
    mem_space_vec.push(mem_space);
  }
}

/// Completes HandlerMemory mem space vector from Protobuf HandlerMemory
fn complete_mem_space_from_pb(
  mem_space_vec: &mut Vec<Option<(usize, usize)>>,
  proto_mem_space: &[protos::HandlerMemory::HandlerMemory_MemSpace],
) {
  for mem_space in proto_mem_space.iter() {
    if mem_space.has_memspacestruct() {
      let mem_space_struct = mem_space.get_memspacestruct();
      mem_space_vec.push(Some((
        mem_space_struct.get_first() as usize,
        mem_space_struct.get_second() as usize,
      )));
    } else {
      mem_space_vec.push(None);
    }
  }
}
