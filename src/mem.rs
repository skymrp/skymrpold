//! Types related to the virtual memory of the emulated application, or the
//! "guest memory".
//!
//! The virtual address space is 32-bit, as is the pointer size.
//!
//! No attempt is made to do endianness conversion for reads and writes to
//! memory, because all supported emulated and host platforms are little-endian.

// use crate::libc::wchar::wchar_t;

mod allocator;
mod host;

use std::ptr::NonNull;

/// Equivalent of `usize` for guest memory.
pub type GuestUSize = u32;

/// Equivalent of `isize` for guest memory.
pub type GuestISize = i32;

/// [std::mem::size_of], but returning a [GuestUSize].
pub const fn guest_size_of<T: Sized>() -> GuestUSize {
    assert!(std::mem::size_of::<T>() <= u32::MAX as usize);
    std::mem::size_of::<T>() as u32
}

/// Internal type for representing an untyped virtual address.
type VAddr = GuestUSize;

/// Pointer type for guest memory, or the "guest pointer" type.
///
/// The `MUT` type parameter determines whether this is mutable or not.
/// Don't write it out explicitly, use [ConstPtr], [MutPtr], [ConstVoidPtr] or
/// [MutVoidPtr] instead instead.
///
/// The implemented methods try to mirror the Rust [pointer] type's methods,
/// where possible.
#[repr(transparent)]
pub struct Ptr<T, const MUT: bool>(VAddr, std::marker::PhantomData<T>);

// #[derive(...)] doesn't work for this type because it expects T to have the
// trait we want implemented
impl<T, const MUT: bool> Clone for Ptr<T, MUT> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, const MUT: bool> Copy for Ptr<T, MUT> {}
impl<T, const MUT: bool> PartialEq for Ptr<T, MUT> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<T, const MUT: bool> Eq for Ptr<T, MUT> {}
impl<T, const MUT: bool> std::hash::Hash for Ptr<T, MUT> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// Constant guest pointer type (like Rust's `*const T`).
pub type ConstPtr<T> = Ptr<T, false>;
/// Mutable guest pointer type (like Rust's `*mut T`).
pub type MutPtr<T> = Ptr<T, true>;
#[allow(dead_code)]
/// Constant guest pointer-to-void type (like C's `const void *`)
pub type ConstVoidPtr = ConstPtr<std::ffi::c_void>;
/// Mutable guest pointer-to-void type (like C's `void *`)
pub type MutVoidPtr = MutPtr<std::ffi::c_void>;

impl<T, const MUT: bool> Ptr<T, MUT> {
    pub const fn null() -> Self {
        Ptr(0, std::marker::PhantomData)
    }

    pub fn to_bits(self) -> VAddr {
        self.0
    }
    pub const fn from_bits(bits: VAddr) -> Self {
        Ptr(bits, std::marker::PhantomData)
    }

    pub fn cast<U>(self) -> Ptr<U, MUT> {
        Ptr::<U, MUT>::from_bits(self.to_bits())
    }

    pub fn cast_void(self) -> Ptr<std::ffi::c_void, MUT> {
        self.cast()
    }

    pub fn is_null(self) -> bool {
        self.to_bits() == 0
    }
}

impl<T> ConstPtr<T> {
    #[allow(dead_code)]
    pub fn cast_mut(self) -> MutPtr<T> {
        Ptr::from_bits(self.to_bits())
    }
}
impl<T> MutPtr<T> {
    pub fn cast_const(self) -> ConstPtr<T> {
        Ptr::from_bits(self.to_bits())
    }
}

impl<T, const MUT: bool> Default for Ptr<T, MUT> {
    fn default() -> Self {
        Self::null()
    }
}

impl<T, const MUT: bool> std::fmt::Debug for Ptr<T, MUT> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_null() {
            write!(f, "(null)")
        } else {
            write!(f, "{:#x}", self.to_bits())
        }
    }
}

// C-like pointer arithmetic
impl<T, const MUT: bool> std::ops::Add<GuestUSize> for Ptr<T, MUT> {
    type Output = Self;

    fn add(self, other: GuestUSize) -> Self {
        let size: GuestUSize = guest_size_of::<T>();
        assert_ne!(size, 0);
        Self::from_bits(
            self.to_bits()
                .checked_add(other.checked_mul(size).unwrap())
                .unwrap(),
        )
    }
}
impl<T, const MUT: bool> std::ops::AddAssign<GuestUSize> for Ptr<T, MUT> {
    fn add_assign(&mut self, rhs: GuestUSize) {
        *self = *self + rhs;
    }
}
impl<T, const MUT: bool> std::ops::Sub<GuestUSize> for Ptr<T, MUT> {
    type Output = Self;

    fn sub(self, other: GuestUSize) -> Self {
        let size: GuestUSize = guest_size_of::<T>();
        assert_ne!(size, 0);
        Self::from_bits(
            self.to_bits()
                .checked_sub(other.checked_mul(size).unwrap())
                .unwrap(),
        )
    }
}
impl<T, const MUT: bool> std::ops::SubAssign<GuestUSize> for Ptr<T, MUT> {
    fn sub_assign(&mut self, rhs: GuestUSize) {
        *self = *self - rhs;
    }
}

/// Marker trait for types that can be safely read from guest memory.
///
/// See also [SafeWrite] and [crate::abi].
///
/// # Safety
/// Reading from guest memory is essentially doing a [std::mem::transmute],
/// which is notoriously unsafe in Rust. Only types for which all possible bit
/// patterns are legal (e.g. integers) should have this trait.
pub unsafe trait SafeRead: Sized {}
// bool is one byte in size and has 0 as false, 1 as true in both Rust and ObjC
unsafe impl SafeRead for bool {}
unsafe impl SafeRead for i8 {}
unsafe impl SafeRead for u8 {}
unsafe impl SafeRead for i16 {}
unsafe impl SafeRead for u16 {}
unsafe impl SafeRead for i32 {}
unsafe impl SafeRead for u32 {}
unsafe impl SafeRead for i64 {}
unsafe impl SafeRead for u64 {}
unsafe impl SafeRead for f32 {}
unsafe impl SafeRead for f64 {}
unsafe impl<T, const MUT: bool> SafeRead for Ptr<T, MUT> {}

/// Marker trait for types that can be written to guest memory.
///
/// Unlike for [SafeRead], there is no (Rust) safety consideration here; it's
/// just a way to catch accidental use of types unintended for guest use.
/// This was added after discovering that `()` is "[Sized]" and therefore a
/// single stray semicolon can wreak havoc...
///
/// Especially for structs, be careful that the type matches the expected ABI.
/// At minimum you should have `#[repr(C, packed)]` and appropriate padding
/// members.
///
/// See also [SafeRead] and [crate::abi].
pub trait SafeWrite: Sized {}
impl<T: SafeRead> SafeWrite for T {}

pub const PAGE_SIZE: GuestUSize = 4096;
pub const PAGE_SIZE_ALIGN_MASK: GuestUSize = 0xfff;
const DEFAULT_REGION_SIZE: GuestUSize = 128 * 1024 * 1024;

pub struct MemRegion {
    guest_base: VAddr,
    len: GuestUSize,
    host_base: NonNull<u8>,
    owned_len: Option<usize>,
}

impl MemRegion {
    pub fn new_borrowed(guest_base: VAddr, host_base: NonNull<u8>, len: GuestUSize) -> MemRegion {
        assert!(len > 0);
        MemRegion {
            guest_base,
            len,
            host_base,
            owned_len: None,
        }
    }

    fn new_owned(guest_base: VAddr, len: GuestUSize) -> MemRegion {
        let host_base =
            unsafe { crate::mem::host::allocate_memory(len as usize).unwrap() }.cast::<u8>();
        let host_base = NonNull::new(host_base).expect("host memory allocation returned null");
        MemRegion {
            guest_base,
            len,
            host_base,
            owned_len: Some(len as usize),
        }
    }

    pub fn guest_base(&self) -> VAddr {
        self.guest_base
    }

    pub fn len(&self) -> GuestUSize {
        self.len
    }

    fn contains_range(&self, addr: VAddr, len: GuestUSize) -> bool {
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        let region_end = self.guest_base + self.len;
        addr >= self.guest_base && end <= region_end
    }

    fn offset(&self, addr: VAddr) -> usize {
        (addr - self.guest_base) as usize
    }
}

/// The type that owns the guest memory and provides accessors for it.
pub struct Mem {
    regions: Vec<MemRegion>,

    /// The size of the __PAGE_ZERO segment, where pointer accesses are trapped
    /// to prevent null pointer derefrences.
    ///
    /// We don't have full memory protection, but we can check accesses in that
    /// range.
    null_segment_size: VAddr,

    allocator: allocator::Allocator,

    /// The flag to control if memory is zeroed out on free (`true`, default)
    /// or on alloc (`false`).
    ///
    /// Right now only one game, Spore Origin, is setting this value to `false`
    /// via a game-specific hack. See [crate::Environment] for more info.
    pub(super) zero_memory_on_free: bool,
}

impl Drop for Mem {
    fn drop(&mut self) {
        for region in &mut self.regions {
            if let Some(size) = region.owned_len.take() {
                unsafe {
                    crate::mem::host::free_memory(region.host_base.as_ptr().cast(), size).unwrap();
                }
            }
        }
    }
}

impl Mem {
    /// [According to Apple](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/CreatingThreads/CreatingThreads.html)
    /// among others, the iPhone OS main thread stack size is 1MiB.
    pub const MAIN_THREAD_STACK_SIZE: GuestUSize = 1024 * 1024;

    /// Address of the lowest byte (not the base) of the main thread's stack.
    ///
    /// We are arbitrarily putting the stack at the top of the virtual address
    /// space (see also: stack.rs), I have no idea if this matches iPhone OS.
    pub const MAIN_THREAD_STACK_LOW_END: VAddr = 0u32.wrapping_sub(Self::MAIN_THREAD_STACK_SIZE);

    /// iPhone OS secondary thread stack size.
    pub const SECONDARY_THREAD_DEFAULT_STACK_SIZE: GuestUSize = 512 * 1024;

    /// Create a fresh instance of guest memory.
    pub fn new() -> Mem {
        Self::with_owned_region(0, DEFAULT_REGION_SIZE)
    }

    pub fn with_owned_region(guest_base: VAddr, len: GuestUSize) -> Mem {
        Self::from_regions(vec![MemRegion::new_owned(guest_base, len)])
    }

    /// Create guest memory backed by existing host memory mapped into a CPU backend.
    ///
    /// The caller must keep borrowed regions alive while this Mem exists.
    pub fn from_regions(mut regions: Vec<MemRegion>) -> Mem {
        assert!(!regions.is_empty());
        regions.sort_by_key(|region| region.guest_base);
        let first = &regions[0];
        let allocator_base = first.guest_base;
        let allocator_len = first.len;
        Self::from_sorted_regions_with_allocator_range(regions, allocator_base, allocator_len)
    }

    pub fn from_regions_with_allocator_range(
        mut regions: Vec<MemRegion>,
        allocator_base: VAddr,
        allocator_len: GuestUSize,
    ) -> Mem {
        assert!(!regions.is_empty());
        regions.sort_by_key(|region| region.guest_base);
        Self::from_sorted_regions_with_allocator_range(regions, allocator_base, allocator_len)
    }

    pub fn from_regions_with_allocator_range_and_alignment(
        mut regions: Vec<MemRegion>,
        allocator_base: VAddr,
        allocator_len: GuestUSize,
        small_alignment: GuestUSize,
        large_alignment: GuestUSize,
    ) -> Mem {
        assert!(!regions.is_empty());
        regions.sort_by_key(|region| region.guest_base);
        Self::from_sorted_regions_with_allocator_range_and_large_alignment(
            regions,
            allocator_base,
            allocator_len,
            small_alignment,
            large_alignment,
        )
    }

    fn from_sorted_regions_with_allocator_range(
        regions: Vec<MemRegion>,
        allocator_base: VAddr,
        allocator_len: GuestUSize,
    ) -> Mem {
        Self::from_sorted_regions_with_allocator_range_and_large_alignment(
            regions,
            allocator_base,
            allocator_len,
            16,
            PAGE_SIZE,
        )
    }

    fn from_sorted_regions_with_allocator_range_and_large_alignment(
        regions: Vec<MemRegion>,
        allocator_base: VAddr,
        allocator_len: GuestUSize,
        small_alignment: GuestUSize,
        large_alignment: GuestUSize,
    ) -> Mem {
        for window in regions.windows(2) {
            let left_end = window[0].guest_base + window[0].len;
            assert!(
                left_end <= window[1].guest_base,
                "guest memory regions must not overlap"
            );
        }
        assert!(
            regions
                .iter()
                .any(|region| region.contains_range(allocator_base, allocator_len)),
            "allocator range must be contained in a guest memory region"
        );

        let allocator = allocator::Allocator::new_with_range_and_alignment(
            allocator_base,
            allocator_len,
            small_alignment,
            large_alignment,
        );

        Mem {
            regions,
            null_segment_size: 0,
            allocator,
            zero_memory_on_free: true,
        }
    }

    /// Sets up the null segment of the given size. There's no reason to call
    /// this outside of binary loading, and it won't be respected even if you
    /// do. The size must not have been set already, and must be page aligned.
    pub fn set_null_segment_size(&mut self, new_null_segment_size: VAddr) {
        // TODO?: Maybe this should be replaced with a per-page rwx/callback
        //        setting? Currently we don't properly follow segment
        //        protections, which means that applications can write into
        //        segments they shouldn't be able to. Adding that would fix
        //        this, along with removing this special case.
        assert!(self.null_segment_size == 0);
        assert!(new_null_segment_size.is_multiple_of(0x1000));
        self.allocator
            .reserve(allocator::Chunk::new(0, new_null_segment_size));
        self.null_segment_size = new_null_segment_size;
    }

    pub fn null_segment_size(&self) -> VAddr {
        self.null_segment_size
    }

    /// Get a pointer to the first contiguous memory region.
    ///
    /// Safety: You must ensure that this pointer does not outlive the instance
    /// of [Mem]. You must not use it while a `&mut` is held on some region of
    /// guest memory.
    pub unsafe fn direct_memory_access_ptr(&mut self) -> *mut std::ffi::c_void {
        self.regions[0].host_base.as_ptr().cast()
    }

    fn find_region(&self, addr: VAddr, count: GuestUSize) -> Option<&MemRegion> {
        self.regions
            .iter()
            .find(|region| region.contains_range(addr, count))
    }

    fn find_region_mut(&mut self, addr: VAddr, count: GuestUSize) -> Option<&mut MemRegion> {
        self.regions
            .iter_mut()
            .find(|region| region.contains_range(addr, count))
    }

    fn bytes_for_range<const MUT: bool>(
        &self,
        ptr: Ptr<u8, MUT>,
        count: GuestUSize,
    ) -> Option<&[u8]> {
        let region = self.find_region(ptr.to_bits(), count)?;
        let offset = region.offset(ptr.to_bits());
        Some(unsafe {
            std::slice::from_raw_parts(region.host_base.as_ptr().add(offset), count as usize)
        })
    }

    fn bytes_for_range_mut(&mut self, ptr: MutPtr<u8>, count: GuestUSize) -> Option<&mut [u8]> {
        let region = self.find_region_mut(ptr.to_bits(), count)?;
        let offset = region.offset(ptr.to_bits());
        Some(unsafe {
            std::slice::from_raw_parts_mut(region.host_base.as_ptr().add(offset), count as usize)
        })
    }

    // the performance characteristics of this hasn't been profiled, but it
    // seems like a good idea to help the compiler optimise for the fast path
    #[cold]
    fn null_check_fail(at: VAddr, size: GuestUSize) {
        panic!("Attempted null-page access at {at:#x} ({size:#x} bytes)")
    }

    /// Special version of [Self::bytes_at] that returns [None] rather than
    /// panicking on failure. Only for debug tooling.
    pub fn get_bytes_fallible(&self, addr: ConstVoidPtr, count: GuestUSize) -> Option<&[u8]> {
        if addr.to_bits() < self.null_segment_size {
            return None;
        }
        self.bytes_for_range(addr.cast(), count)
    }
    /// Special version of [Self::bytes_at_mut] that returns [None] rather than
    /// panicking on failure. Only for debug tooling.
    pub fn get_bytes_fallible_mut(
        &mut self,
        addr: ConstVoidPtr,
        count: GuestUSize,
    ) -> Option<&mut [u8]> {
        if addr.to_bits() < self.null_segment_size {
            return None;
        }
        self.bytes_for_range_mut(addr.cast_mut().cast(), count)
    }

    /// Get a slice for reading `count` bytes. This is the basic primitive for
    /// safe read-only memory access.
    ///
    /// This will panic when `ptr` is within the null page, even if `count` is
    /// 0. This may be inconvenient in some cases, but it makes the behavior
    /// when deriving a pointer from the slice consistent (though you should use
    /// [Self::ptr_at] for that).
    pub fn bytes_at<const MUT: bool>(&self, ptr: Ptr<u8, MUT>, count: GuestUSize) -> &[u8] {
        if ptr.to_bits() < self.null_segment_size {
            Self::null_check_fail(ptr.to_bits(), count)
        }
        self.bytes_for_range(ptr, count).unwrap_or_else(|| {
            panic!(
                "Attempted guest memory access at {:#x} ({count:#x} bytes)",
                ptr.to_bits()
            )
        })
    }
    /// Get a slice for reading `count` bytes without a null-page check.
    ///
    /// This **doesn't** panic at access within the null page.
    ///
    /// You shall have a good reason to use it instead of [Self::bytes_at]
    pub fn unchecked_bytes_at<const MUT: bool>(
        &self,
        ptr: Ptr<u8, MUT>,
        count: GuestUSize,
    ) -> &[u8] {
        self.bytes_for_range(ptr, count).unwrap_or_else(|| {
            panic!(
                "Attempted guest memory access at {:#x} ({count:#x} bytes)",
                ptr.to_bits()
            )
        })
    }
    /// Get a slice for reading or writing `count` bytes. This is the basic
    /// primitive for safe read-write memory access.
    ///
    /// This will panic when `ptr` is within the null page, even if `count` is
    /// 0. This may be inconvenient in some cases, but it makes the behavior
    /// when deriving a pointer from the slice consistent (though you should use
    /// [Self::ptr_at_mut] for that).
    pub fn bytes_at_mut(&mut self, ptr: MutPtr<u8>, count: GuestUSize) -> &mut [u8] {
        if ptr.to_bits() < self.null_segment_size {
            Self::null_check_fail(ptr.to_bits(), count)
        }
        self.bytes_for_range_mut(ptr, count).unwrap_or_else(|| {
            panic!(
                "Attempted guest memory access at {:#x} ({count:#x} bytes)",
                ptr.to_bits()
            )
        })
    }

    /// Get a pointer for reading an array of `count` elements of type `T`.
    /// Only use this for interfacing with unsafe C-like APIs.
    ///
    /// The `count` argument is purely for bounds-checking and does not affect
    /// the result.
    ///
    /// No guarantee is made about the alignment of the resulting pointer!
    /// Pointers that are well-aligned for the guest are not necessarily
    /// well-aligned for the host. Rust strictly requires pointers to be
    /// well-aligned when dereferencing them, or when constructing references or
    /// slices from them, so **be very careful**.
    pub fn ptr_at<T, const MUT: bool>(&self, ptr: Ptr<T, MUT>, count: GuestUSize) -> *const T
    where
        T: SafeRead,
    {
        let size = count.checked_mul(guest_size_of::<T>()).unwrap();
        self.bytes_at(ptr.cast(), size).as_ptr().cast()
    }
    /// A variation of [Self::ptr_at] without a null-page check.
    ///
    /// This **doesn't** panic at access within the null page.
    ///
    /// You shall have a good reason to use it instead of [Self::ptr_at]
    pub fn unchecked_ptr_at<T, const MUT: bool>(
        &self,
        ptr: Ptr<T, MUT>,
        count: GuestUSize,
    ) -> *const T
    where
        T: SafeRead,
    {
        let size = count.checked_mul(guest_size_of::<T>()).unwrap();
        self.unchecked_bytes_at(ptr.cast(), size).as_ptr().cast()
    }
    /// Get a pointer for reading or writing to an array of `count` elements of
    /// type `T`. Only use this for interfacing with unsafe C-like APIs.
    ///
    /// The `count` argument is purely for bounds-checking and does not affect
    /// the result.
    ///
    /// No guarantee is made about the alignment of the resulting pointer!
    /// Pointers that are well-aligned for the guest are not necessarily
    /// well-aligned for the host. Rust strictly requires pointers to be
    /// well-aligned when dereferencing them, or when constructing references or
    /// slices from them, so **be very careful**.
    pub fn ptr_at_mut<T>(&mut self, ptr: MutPtr<T>, count: GuestUSize) -> *mut T
    where
        T: SafeRead + SafeWrite,
    {
        let size = count.checked_mul(guest_size_of::<T>()).unwrap();
        self.bytes_at_mut(ptr.cast(), size).as_mut_ptr().cast()
    }

    /// Transform a host pointer addressing a location in guest memory back into
    /// a guest pointer. This exists solely to deal with OpenGL `glGetPointerv`.
    /// You should never have another reason to use this.
    ///
    /// Panics if the host pointer is not addressing a location in guest memory.
    pub fn host_ptr_to_guest_ptr(&self, host_ptr: *const std::ffi::c_void) -> ConstVoidPtr {
        let host_ptr = host_ptr.cast::<u8>();
        let host_addr = host_ptr as usize;
        for region in &self.regions {
            let start = region.host_base.as_ptr() as usize;
            let end = start + region.len as usize;
            if (start..end).contains(&host_addr) {
                let offset = host_addr - start;
                return Ptr::from_bits(region.guest_base + u32::try_from(offset).unwrap());
            }
        }
        panic!("host pointer {:p} is not inside guest memory", host_ptr)
    }

    /// Read a value for memory. This is the preferred way to read memory in
    /// most cases.
    pub fn read<T, const MUT: bool>(&self, ptr: Ptr<T, MUT>) -> T
    where
        T: SafeRead,
    {
        // This is unsafe unless we are careful with which types SafeRead is
        // implemented for!
        // This would also be unsafe if the non-unaligned method was used.
        unsafe { self.ptr_at(ptr, 1).read_unaligned() }
    }
    /// Write a value to memory. This is the preferred way to write memory in
    /// most cases.
    pub fn write<T>(&mut self, ptr: MutPtr<T>, value: T)
    where
        T: SafeWrite,
    {
        let size = guest_size_of::<T>();
        assert!(size > 0);
        let slice = self.bytes_at_mut(ptr.cast(), size);
        let ptr: *mut T = slice.as_mut_ptr().cast();
        // It's unaligned because what is well-aligned for the guest is not
        // necessarily well-aligned for the host.
        // This would be unsafe if the non-unaligned method was used.
        unsafe { ptr.write_unaligned(value) }
    }

    /// C-style `memmove`.
    pub fn memmove(&mut self, dest: MutVoidPtr, src: ConstVoidPtr, size: GuestUSize) {
        let temp = self.bytes_at(src.cast(), size).to_vec();
        self.bytes_at_mut(dest.cast(), size).copy_from_slice(&temp);
    }

    /// Allocate `size` bytes.
    pub fn alloc(&mut self, size: GuestUSize) -> MutVoidPtr {
        let ptr = Ptr::from_bits(self.allocator.alloc(size));
        if !self.zero_memory_on_free {
            self.bytes_at_mut(ptr.cast(), size).fill(0);
        }
        ptr
    }

    /// Allocate `size` bytes initialized to 0.
    pub fn calloc(&mut self, size: GuestUSize) -> MutVoidPtr {
        let ptr = self.alloc(size);
        self.bytes_at_mut(ptr.cast(), size).fill(0);
        ptr
    }

    pub fn malloc_size(&mut self, ptr: ConstVoidPtr) -> GuestUSize {
        self.allocator.find_allocated_size(ptr.to_bits())
    }

    pub fn realloc(&mut self, old_ptr: MutVoidPtr, size: GuestUSize) -> MutVoidPtr {
        if old_ptr.is_null() {
            return self.alloc(size);
        }
        // TODO: for a moment we always assume that we do not have enough size
        //       to realloc inplace
        let old_size = self.allocator.find_allocated_size(old_ptr.to_bits());
        if old_size >= size {
            return old_ptr;
        }
        let new_ptr = self.alloc(size);
        self.memmove(new_ptr, old_ptr.cast_const(), old_size);
        self.free(old_ptr);
        new_ptr
    }

    /// Free an allocation made with one of the `alloc` methods on this type.
    pub fn free(&mut self, ptr: MutVoidPtr) {
        let size = self.allocator.free(ptr.to_bits());
        if self.zero_memory_on_free {
            self.bytes_at_mut(ptr.cast(), size).fill(0);
        }
    }

    /// Allocate memory large enough for a value of type `T` and write the value
    /// to it. Equivalent to [Self::alloc] + [Self::write].
    pub fn alloc_and_write<T>(&mut self, value: T) -> MutPtr<T>
    where
        T: SafeWrite,
    {
        let ptr = self.alloc(guest_size_of::<T>()).cast();
        self.write(ptr, value);
        ptr
    }

    /// Allocate and write a C string. This method will add a null terminator,
    /// so it is optimal if the input slice does not already contain one.
    pub fn alloc_and_write_cstr(&mut self, str_bytes: &[u8]) -> MutPtr<u8> {
        let len = str_bytes.len().try_into().unwrap();
        let ptr = self.alloc(len + 1).cast();
        self.bytes_at_mut(ptr, len).copy_from_slice(str_bytes);
        self.write(ptr + len, b'\0');
        ptr
    }

    /// Get a C string (null-terminated) as a slice. The null terminator is not
    /// included in the slice.
    pub fn cstr_at<const MUT: bool>(&self, ptr: Ptr<u8, MUT>) -> &[u8] {
        let mut len = 0;
        while self.read(ptr + len) != b'\0' {
            len += 1;
        }
        self.bytes_at(ptr, len)
    }

    /// Get a C string (null-terminated) as a string slice, if it is valid
    /// UTF-8, otherwise returning a byte slice. The null terminator is not
    /// included in the slice.
    pub fn cstr_at_utf8<const MUT: bool>(&self, ptr: Ptr<u8, MUT>) -> Result<&str, &[u8]> {
        let bytes = self.cstr_at(ptr);
        std::str::from_utf8(bytes).map_err(|_| bytes)
    }

    // pub fn wcstr_at<const MUT: bool>(&self, ptr: Ptr<wchar_t, MUT>) -> String {
    //     let mut len = 0;
    //     while self.read(ptr + len) != wchar_t::default() {
    //         len += 1;
    //     }
    //     let iter = self
    //         .bytes_at(ptr.cast(), len * guest_size_of::<wchar_t>())
    //         .chunks(4)
    //         .map(|chunk| char::from_u32(u32::from_le_bytes(chunk.try_into().unwrap())).unwrap());
    //     String::from_iter(iter)
    // }

    /// Permanently mark a region of address space as being unusable to the
    /// memory allocator.
    pub fn reserve(&mut self, base: VAddr, size: GuestUSize) {
        self.allocator.reserve(allocator::Chunk::new(base, size));
    }
}
