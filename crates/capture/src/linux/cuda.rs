//! The CUDA driver API, loaded at run time from `libcuda.so.1`: contexts, pitched device memory
//! and OpenGL texture interop.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::{Arc, OnceLock};

use crate::{CaptureError, Result};

pub(crate) type CuResult = c_int;
pub(crate) type CuDevice = c_int;
pub(crate) type CuContext = *mut c_void;
pub(crate) type CuGraphicsResource = *mut c_void;
pub(crate) type CuArray = *mut c_void;
pub(crate) type CuDevicePtr = u64;

const CUDA_SUCCESS: CuResult = 0;
const CU_CTX_SCHED_BLOCKING_SYNC: c_uint = 0x4;
const CU_GL_DEVICE_LIST_ALL: c_uint = 1;
const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: c_uint = 1;
const CU_MEMORYTYPE_DEVICE: c_uint = 2;
const CU_MEMORYTYPE_ARRAY: c_uint = 3;
const GL_TEXTURE_2D: c_uint = 0x0de1;

/// `CUDA_MEMCPY2D`.
#[repr(C)]
#[derive(Debug)]
struct Memcpy2d {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: *const c_void,
    src_device: CuDevicePtr,
    src_array: CuArray,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: *mut c_void,
    dst_device: CuDevicePtr,
    dst_array: CuArray,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

/// The entry points used here, with signatures from `cuda.h` and `cudaGL.h`.
#[allow(non_snake_case)]
struct Lib {
    cuInit: unsafe extern "C" fn(c_uint) -> CuResult,
    cuDeviceGetCount: unsafe extern "C" fn(*mut c_int) -> CuResult,
    cuDeviceGet: unsafe extern "C" fn(*mut CuDevice, c_int) -> CuResult,
    cuDeviceGetPCIBusId: unsafe extern "C" fn(*mut c_char, c_int, CuDevice) -> CuResult,
    cuCtxCreate_v2: unsafe extern "C" fn(*mut CuContext, c_uint, CuDevice) -> CuResult,
    cuCtxDestroy_v2: unsafe extern "C" fn(CuContext) -> CuResult,
    cuCtxPushCurrent_v2: unsafe extern "C" fn(CuContext) -> CuResult,
    cuCtxPopCurrent_v2: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    cuGLGetDevices_v2: unsafe extern "C" fn(*mut c_uint, *mut CuDevice, c_uint, c_uint) -> CuResult,
    cuGraphicsGLRegisterImage:
        unsafe extern "C" fn(*mut CuGraphicsResource, c_uint, c_uint, c_uint) -> CuResult,
    cuGraphicsUnregisterResource: unsafe extern "C" fn(CuGraphicsResource) -> CuResult,
    cuGraphicsMapResources:
        unsafe extern "C" fn(c_uint, *mut CuGraphicsResource, *mut c_void) -> CuResult,
    cuGraphicsUnmapResources:
        unsafe extern "C" fn(c_uint, *mut CuGraphicsResource, *mut c_void) -> CuResult,
    cuGraphicsSubResourceGetMappedArray:
        unsafe extern "C" fn(*mut CuArray, CuGraphicsResource, c_uint, c_uint) -> CuResult,
    cuMemAllocPitch_v2:
        unsafe extern "C" fn(*mut CuDevicePtr, *mut usize, usize, usize, c_uint) -> CuResult,
    cuMemFree_v2: unsafe extern "C" fn(CuDevicePtr) -> CuResult,
    cuMemcpy2D_v2: unsafe extern "C" fn(*const Memcpy2d) -> CuResult,
    cuGetErrorString: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
}

fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load)
        .as_ref()
        .map_err(|e| CaptureError::NoEncoder(e.clone()))
}

fn load() -> std::result::Result<Lib, String> {
    // SAFETY: the NVIDIA driver library has no initializers with preconditions.
    let cuda = unsafe { libloading::Library::new("libcuda.so.1") }
        .map_err(|e| format!("NVIDIA CUDA driver not found (libcuda.so.1): {e}"))?;
    macro_rules! sym {
        ($name:ident) => {
            // SAFETY: the field's type is the function's signature in cuda.h / cudaGL.h.
            *unsafe { cuda.get(concat!(stringify!($name), "\0").as_bytes()) }
                .map_err(|e| format!("{}: {e}", stringify!($name)))?
        };
    }
    let lib = Lib {
        cuInit: sym!(cuInit),
        cuDeviceGetCount: sym!(cuDeviceGetCount),
        cuDeviceGet: sym!(cuDeviceGet),
        cuDeviceGetPCIBusId: sym!(cuDeviceGetPCIBusId),
        cuCtxCreate_v2: sym!(cuCtxCreate_v2),
        cuCtxDestroy_v2: sym!(cuCtxDestroy_v2),
        cuCtxPushCurrent_v2: sym!(cuCtxPushCurrent_v2),
        cuCtxPopCurrent_v2: sym!(cuCtxPopCurrent_v2),
        cuGLGetDevices_v2: sym!(cuGLGetDevices_v2),
        cuGraphicsGLRegisterImage: sym!(cuGraphicsGLRegisterImage),
        cuGraphicsUnregisterResource: sym!(cuGraphicsUnregisterResource),
        cuGraphicsMapResources: sym!(cuGraphicsMapResources),
        cuGraphicsUnmapResources: sym!(cuGraphicsUnmapResources),
        cuGraphicsSubResourceGetMappedArray: sym!(cuGraphicsSubResourceGetMappedArray),
        cuMemAllocPitch_v2: sym!(cuMemAllocPitch_v2),
        cuMemFree_v2: sym!(cuMemFree_v2),
        cuMemcpy2D_v2: sym!(cuMemcpy2D_v2),
        cuGetErrorString: sym!(cuGetErrorString),
    };
    // SAFETY: cuInit(0) is the documented first call.
    let r = unsafe { (lib.cuInit)(0) };
    if r != CUDA_SUCCESS {
        return Err(format!("cuInit: {}", error(&lib, r)));
    }
    std::mem::forget(cuda);
    Ok(lib)
}

fn error(lib: &Lib, r: CuResult) -> String {
    let mut s: *const c_char = std::ptr::null();
    unsafe { (lib.cuGetErrorString)(r, &mut s) };
    if s.is_null() {
        format!("CUresult {r}")
    } else {
        format!(
            "{} (CUresult {r})",
            unsafe { CStr::from_ptr(s) }.to_string_lossy()
        )
    }
}

fn check(lib: &Lib, r: CuResult, what: &str) -> Result<()> {
    if r == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(CaptureError::Platform(format!("{what}: {}", error(lib, r))))
    }
}

/// A CUDA context, destroyed with the last reference.
pub(crate) struct Context {
    lib: &'static Lib,
    ctx: CuContext,
}

// SAFETY: CUDA contexts may be used from any thread once pushed there.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Context {
    /// A context on the CUDA device behind the current OpenGL context.
    pub(crate) fn for_current_gl() -> Result<Arc<Self>> {
        let lib = lib()?;
        let mut count = 0;
        let mut device: CuDevice = 0;
        check(
            lib,
            unsafe { (lib.cuGLGetDevices_v2)(&mut count, &mut device, 1, CU_GL_DEVICE_LIST_ALL) },
            "cuGLGetDevices",
        )
        .map_err(|e| CaptureError::NoEncoder(e.to_string()))?;
        if count == 0 {
            return Err(CaptureError::NoEncoder(
                "the OpenGL context is not on a CUDA device".into(),
            ));
        }
        Self::on(lib, device)
    }

    /// A context on the CUDA device at PCI address `bus_id` (`0000:01:00.0`).
    pub(crate) fn for_pci(bus_id: &str) -> Result<Arc<Self>> {
        let lib = lib()?;
        let mut n = 0;
        check(
            lib,
            unsafe { (lib.cuDeviceGetCount)(&mut n) },
            "cuDeviceGetCount",
        )?;
        for ordinal in 0..n {
            let mut device = 0;
            if unsafe { (lib.cuDeviceGet)(&mut device, ordinal) } != CUDA_SUCCESS {
                continue;
            }
            let mut buf = [0 as c_char; 32];
            if unsafe { (lib.cuDeviceGetPCIBusId)(buf.as_mut_ptr(), buf.len() as c_int, device) }
                != CUDA_SUCCESS
            {
                continue;
            }
            let id = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
            if id.eq_ignore_ascii_case(bus_id) {
                return Self::on(lib, device);
            }
        }
        Err(CaptureError::NoEncoder(format!(
            "no CUDA device at {bus_id}"
        )))
    }

    fn on(lib: &'static Lib, device: CuDevice) -> Result<Arc<Self>> {
        let mut ctx = std::ptr::null_mut();
        check(
            lib,
            unsafe { (lib.cuCtxCreate_v2)(&mut ctx, CU_CTX_SCHED_BLOCKING_SYNC, device) },
            "cuCtxCreate",
        )?;
        // Not current until pushed.
        let mut popped = std::ptr::null_mut();
        unsafe { (lib.cuCtxPopCurrent_v2)(&mut popped) };
        Ok(Arc::new(Self { lib, ctx }))
    }

    pub(crate) fn raw(&self) -> CuContext {
        self.ctx
    }

    /// Makes the context current on the calling thread until the guard drops.
    pub(crate) fn push(&self) -> Result<Pushed<'_>> {
        check(
            self.lib,
            unsafe { (self.lib.cuCtxPushCurrent_v2)(self.ctx) },
            "cuCtxPushCurrent",
        )?;
        Ok(Pushed(self))
    }

    /// Makes the context current on the calling thread for good.
    pub(crate) fn push_forever(&self) {
        unsafe { (self.lib.cuCtxPushCurrent_v2)(self.ctx) };
    }

    /// `height` rows of `width_bytes` bytes, pitched.
    pub(crate) fn alloc_pitch(
        self: &Arc<Self>,
        width_bytes: usize,
        height: usize,
    ) -> Result<DeviceBuffer> {
        let _cur = self.push()?;
        let mut ptr = 0;
        let mut pitch = 0;
        check(
            self.lib,
            unsafe { (self.lib.cuMemAllocPitch_v2)(&mut ptr, &mut pitch, width_bytes, height, 16) },
            "cuMemAllocPitch",
        )?;
        Ok(DeviceBuffer {
            ctx: self.clone(),
            ptr,
            pitch,
        })
    }

    /// Registers a GL 2D texture for reading.
    pub(crate) fn register_texture(self: &Arc<Self>, texture: u32) -> Result<GlResource> {
        let _cur = self.push()?;
        let mut res = std::ptr::null_mut();
        check(
            self.lib,
            unsafe {
                (self.lib.cuGraphicsGLRegisterImage)(
                    &mut res,
                    texture,
                    GL_TEXTURE_2D,
                    CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
                )
            },
            "cuGraphicsGLRegisterImage",
        )?;
        Ok(GlResource {
            ctx: self.clone(),
            res,
        })
    }

    /// Copies registered textures into `dst`: each `(texture, row bytes, rows)` below the last.
    pub(crate) fn copy_textures(
        &self,
        planes: &mut [(&GlResource, usize, usize)],
        dst: &DeviceBuffer,
    ) -> Result<()> {
        let l = self.lib;
        let _cur = self.push()?;
        let mut resources: Vec<CuGraphicsResource> = planes.iter().map(|p| p.0.res).collect();
        check(
            l,
            unsafe {
                (l.cuGraphicsMapResources)(
                    resources.len() as c_uint,
                    resources.as_mut_ptr(),
                    std::ptr::null_mut(),
                )
            },
            "cuGraphicsMapResources",
        )?;
        let result = (|| {
            let mut row = 0;
            for (resource, width_bytes, height) in planes.iter() {
                let mut array = std::ptr::null_mut();
                check(
                    l,
                    unsafe {
                        (l.cuGraphicsSubResourceGetMappedArray)(&mut array, resource.res, 0, 0)
                    },
                    "cuGraphicsSubResourceGetMappedArray",
                )?;
                let copy = Memcpy2d {
                    src_x_in_bytes: 0,
                    src_y: 0,
                    src_memory_type: CU_MEMORYTYPE_ARRAY,
                    src_host: std::ptr::null(),
                    src_device: 0,
                    src_array: array,
                    src_pitch: 0,
                    dst_x_in_bytes: 0,
                    dst_y: row,
                    dst_memory_type: CU_MEMORYTYPE_DEVICE,
                    dst_host: std::ptr::null_mut(),
                    dst_device: dst.ptr,
                    dst_array: std::ptr::null_mut(),
                    dst_pitch: dst.pitch,
                    width_in_bytes: *width_bytes,
                    height: *height,
                };
                check(l, unsafe { (l.cuMemcpy2D_v2)(&copy) }, "cuMemcpy2D")?;
                row += height;
            }
            Ok(())
        })();
        unsafe {
            (l.cuGraphicsUnmapResources)(
                resources.len() as c_uint,
                resources.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        result
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { (self.lib.cuCtxDestroy_v2)(self.ctx) };
    }
}

/// Pops the context on drop.
pub(crate) struct Pushed<'a>(&'a Context);

impl Drop for Pushed<'_> {
    fn drop(&mut self) {
        let mut popped = std::ptr::null_mut();
        unsafe { (self.0.lib.cuCtxPopCurrent_v2)(&mut popped) };
    }
}

/// Pitched device memory.
pub(crate) struct DeviceBuffer {
    ctx: Arc<Context>,
    pub ptr: CuDevicePtr,
    pub pitch: usize,
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        if let Ok(_cur) = self.ctx.push() {
            unsafe { (self.ctx.lib.cuMemFree_v2)(self.ptr) };
        }
    }
}

/// A GL texture registered with CUDA.
pub(crate) struct GlResource {
    ctx: Arc<Context>,
    res: CuGraphicsResource,
}

impl Drop for GlResource {
    fn drop(&mut self) {
        if let Ok(_cur) = self.ctx.push() {
            unsafe { (self.ctx.lib.cuGraphicsUnregisterResource)(self.res) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memcpy2d_matches_cuda_h() {
        // x86_64: 16 fields of 8 bytes, the memory types padded to 8.
        assert_eq!(size_of::<Memcpy2d>(), 128);
        assert_eq!(std::mem::offset_of!(Memcpy2d, src_device), 32);
        assert_eq!(std::mem::offset_of!(Memcpy2d, dst_memory_type), 72);
        assert_eq!(std::mem::offset_of!(Memcpy2d, width_in_bytes), 112);
    }
}
