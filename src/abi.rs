//! Traits for application binary interface (ABI) translation, in particular
//! calling conventions.
//!
//! See also: [crate::mem::SafeRead] and [crate::mem::SafeWrite].

use crate::mem::{ConstVoidPtr, GuestUSize, Mem, MutVoidPtr, Ptr};

/// ARM AAPCS registers used by guest ABI translation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AbiReg {
    R0,
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
    R7,
    R8,
    R9,
    R10,
    R11,
    R12,
    SP,
    LR,
    PC,
}

/// Minimal register access needed for guest ABI translation.
pub trait RegisterContext {
    fn read_reg(&mut self, reg: AbiReg) -> u32;
    fn write_reg(&mut self, reg: AbiReg, value: u32);
}

/// Stack memory access used when arguments overflow r0-r3.
pub trait StackMemoryContext {
    fn read_stack_u32(&mut self, sp: u32, word_offset: usize) -> u32;
    fn write_stack_u32(&mut self, sp: u32, word_offset: usize, value: u32);
}

/// Calling convention translation for a function argument type.
pub trait GuestArg: std::fmt::Debug + Sized {
    /// How many registers does this argument type consume?
    const REG_COUNT: usize;

    /// Read the argument from registers. Only `&regs[0..Self::REG_COUNT]` may
    /// be accessed.
    fn from_regs(regs: &[u32]) -> Self;

    /// Write the argument to registers. Only '&mut regs[0..Self::REG_COUNT]`
    /// may be accessed.
    fn to_regs(self, regs: &mut [u32]);
}

fn arg_reg(n: usize) -> AbiReg {
    match n {
        0 => AbiReg::R0,
        1 => AbiReg::R1,
        2 => AbiReg::R2,
        3 => AbiReg::R3,
        _ => panic!("argument register index out of range: {n}"),
    }
}

/// Read an argument from a generic register/stack context.
pub fn read_arg_from_context<T, C>(ctx: &mut C, n: usize) -> T
where
    T: GuestArg,
    C: RegisterContext + StackMemoryContext,
{
    let mut fake_regs = [0u32; 16];
    let fake_regs = &mut fake_regs[0..T::REG_COUNT];
    let sp = ctx.read_reg(AbiReg::SP);

    for (i, fake_reg) in fake_regs.iter_mut().enumerate() {
        let reg_offset = n + i;
        *fake_reg = if reg_offset < 4 {
            ctx.read_reg(arg_reg(reg_offset))
        } else {
            ctx.read_stack_u32(sp, reg_offset - 4)
        };
    }

    T::from_regs(fake_regs)
}

/// Write a return value to a generic register context.
pub fn write_ret_to_context<T, C>(ctx: &mut C, value: T)
where
    T: GuestRet,
    C: RegisterContext,
{
    assert!(
        T::SIZE_IN_MEM.is_none(),
        "large struct returns need guest memory access"
    );

    let mut fake_regs = [0u32; 16];
    value.to_regs(&mut fake_regs);
    match std::mem::size_of::<T>() {
        0 => {}
        1..=4 => ctx.write_reg(AbiReg::R0, fake_regs[0]),
        _ => {
            ctx.write_reg(AbiReg::R0, fake_regs[0]);
            ctx.write_reg(AbiReg::R1, fake_regs[1]);
        }
    }
}

macro_rules! impl_GuestArg_with {
    ($for:ty, $with:ty) => {
        impl GuestArg for $for {
            const REG_COUNT: usize = <$with as GuestArg>::REG_COUNT;
            fn from_regs(regs: &[u32]) -> Self {
                <$with as GuestArg>::from_regs(regs) as $for
            }
            fn to_regs(self, regs: &mut [u32]) {
                <$with as GuestArg>::to_regs(self as $with, regs)
            }
        }
    };
}

impl GuestArg for u32 {
    const REG_COUNT: usize = 1;
    fn from_regs(regs: &[u32]) -> Self {
        regs[0]
    }
    fn to_regs(self, regs: &mut [u32]) {
        regs[0] = self;
    }
}

impl_GuestArg_with!(i32, u32);
impl_GuestArg_with!(u16, u32);
impl_GuestArg_with!(i16, u32);
impl_GuestArg_with!(u8, u32);
impl_GuestArg_with!(i8, u32);

impl GuestArg for bool {
    const REG_COUNT: usize = 1;
    fn from_regs(regs: &[u32]) -> Self {
        <u32 as GuestArg>::from_regs(regs) != 0
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestArg>::to_regs(self as u32, regs)
    }
}

impl GuestArg for f32 {
    const REG_COUNT: usize = <u32 as GuestArg>::REG_COUNT;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestArg>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestArg>::to_regs(self.to_bits(), regs)
    }
}

impl<T, const MUT: bool> GuestArg for Ptr<T, MUT> {
    const REG_COUNT: usize = <u32 as GuestArg>::REG_COUNT;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestArg>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestArg>::to_regs(self.to_bits(), regs)
    }
}

impl GuestArg for u64 {
    const REG_COUNT: usize = 2;
    fn from_regs(regs: &[u32]) -> Self {
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&regs[0].to_le_bytes());
        bytes[4..8].copy_from_slice(&regs[1].to_le_bytes());
        u64::from_le_bytes(bytes)
    }
    fn to_regs(self, regs: &mut [u32]) {
        let bytes = self.to_le_bytes();
        regs[0] = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        regs[1] = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    }
}

impl_GuestArg_with!(i64, u64);

impl GuestArg for f64 {
    const REG_COUNT: usize = <u64 as GuestArg>::REG_COUNT;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u64 as GuestArg>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u64 as GuestArg>::to_regs(self.to_bits(), regs)
    }
}

/// Calling convention translation for a function return type.
pub trait GuestRet: std::fmt::Debug + Sized {
    /// If this is `None`, then the return value is passed directly in
    /// registers and the `to_regs` and `from_regs` methods should be used.
    /// If this is `Some(size)`, then the return value is stored in memory at
    /// the location specified by an implicit pointer argument in r0.
    const SIZE_IN_MEM: Option<GuestUSize> = None;

    /// Read the return value from registers.
    fn from_regs(regs: &[u32]) -> Self {
        let _ = regs;
        panic!()
    }
    /// Write the return value to registers.
    fn to_regs(self, regs: &mut [u32]) {
        let _ = regs;
        panic!()
    }

    /// Read the return value from memory.
    fn from_mem(ptr: ConstVoidPtr, mem: &Mem) -> Self {
        let _ = (ptr, mem);
        panic!()
    }
    /// Write the return value to memory.
    fn to_mem(self, ptr: MutVoidPtr, mem: &mut Mem) {
        let _ = (ptr, mem);
        panic!()
    }
}

macro_rules! impl_GuestRet_with {
    ($for:ty, $with:ty) => {
        impl GuestRet for $for {
            fn from_regs(regs: &[u32]) -> Self {
                <$with as GuestRet>::from_regs(regs) as $for
            }
            fn to_regs(self, regs: &mut [u32]) {
                <$with as GuestRet>::to_regs(self as $with, regs)
            }
        }
    };
}

/// Generates a trait implementation of [GuestRet] for a struct type that is
/// larger than 4 bytes and returned via an implicit pointer parameter.
#[macro_export]
macro_rules! impl_GuestRet_for_large_struct {
    ($for:ty) => {
        impl $crate::abi::GuestRet for $for {
            const SIZE_IN_MEM: Option<$crate::mem::GuestUSize> =
                Some($crate::mem::guest_size_of::<$for>());

            fn from_mem(ptr: $crate::mem::ConstVoidPtr, mem: &$crate::mem::Mem) -> Self {
                let ptr = ptr.cast::<Self>();
                mem.read(ptr)
            }
            fn to_mem(self, ptr: $crate::mem::MutVoidPtr, mem: &mut $crate::mem::Mem) {
                let ptr = ptr.cast::<Self>();
                mem.write(ptr, self)
            }
        }
    };
}
pub use crate::impl_GuestRet_for_large_struct;

impl GuestRet for () {
    fn to_regs(self, _regs: &mut [u32]) {}
    fn from_regs(_regs: &[u32]) -> Self {}
}

impl GuestRet for u32 {
    fn from_regs(regs: &[u32]) -> Self {
        regs[0]
    }
    fn to_regs(self, regs: &mut [u32]) {
        regs[0] = self;
    }
}

impl_GuestRet_with!(i32, u32);
impl_GuestRet_with!(u16, u32);
impl_GuestRet_with!(i16, u32);
impl_GuestRet_with!(u8, u32);
impl_GuestRet_with!(i8, u32);

impl GuestRet for bool {
    fn from_regs(regs: &[u32]) -> Self {
        <u32 as GuestRet>::from_regs(regs) != 0
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestRet>::to_regs(self as u32, regs)
    }
}

impl GuestRet for f32 {
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestRet>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestRet>::to_regs(self.to_bits(), regs)
    }
}

impl<T, const MUT: bool> GuestRet for Ptr<T, MUT> {
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestRet>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestRet>::to_regs(self.to_bits(), regs)
    }
}

impl GuestRet for u64 {
    fn from_regs(regs: &[u32]) -> Self {
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&regs[0].to_le_bytes());
        bytes[4..8].copy_from_slice(&regs[1].to_le_bytes());
        u64::from_le_bytes(bytes)
    }
    fn to_regs(self, regs: &mut [u32]) {
        let bytes = self.to_le_bytes();
        regs[0] = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        regs[1] = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    }
}

impl_GuestRet_with!(i64, u64);

impl GuestRet for f64 {
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u64 as GuestRet>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u64 as GuestRet>::to_regs(self.to_bits(), regs)
    }
}
