//! EGL on a DRM device and an OpenGL converter: imports the scanout DMA-BUF as a texture and
//! renders it, cropped, rotated, mirrored, scaled and letterboxed, into NV12 planes (BT.709
//! limited range).

use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::OnceLock;

use super::kms::{Crop, Frame, FrameKey};
use super::transform::Transform;
use crate::layout::letterbox;
use crate::{CaptureError, Result};

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglDevice = *mut c_void;
type EglImage = *mut c_void;
type EglBoolean = u32;
type EglInt = i32;

const EGL_NONE: EglInt = 0x3038;
const EGL_EXTENSIONS: EglInt = 0x3055;
const EGL_OPENGL_API: u32 = 0x30a2;
const EGL_OPENGL_BIT: EglInt = 0x0008;
const EGL_RENDERABLE_TYPE: EglInt = 0x3040;
const EGL_CONTEXT_MAJOR_VERSION: EglInt = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: EglInt = 0x30fb;
const EGL_CONTEXT_OPENGL_PROFILE_MASK: EglInt = 0x30fd;
const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: EglInt = 0x1;
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313f;
const EGL_DRM_DEVICE_FILE_EXT: EglInt = 0x3233;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: EglInt = 0x3271;
/// `EGL_DMA_BUF_PLANEn_{FD,OFFSET,PITCH}_EXT` and `..._MODIFIER_{LO,HI}_EXT` for n = 0..4.
const PLANE_ATTRIBS: [[EglInt; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327a, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344a],
];
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const GL_TEXTURE_2D: u32 = 0x0de1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812f;
const GL_R8: u32 = 0x8229;
const GL_RG8: u32 = 0x822b;
const GL_FRAMEBUFFER: u32 = 0x8d40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8ce0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8cd5;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_VERTEX_SHADER: u32 = 0x8b31;
const GL_FRAGMENT_SHADER: u32 = 0x8b30;
const GL_COMPILE_STATUS: u32 = 0x8b81;
const GL_LINK_STATUS: u32 = 0x8b82;
const GL_TRIANGLES: u32 = 0x0004;
const GL_TEXTURE0: u32 = 0x84c0;

/// The EGL and GL entry points used here.
#[allow(non_snake_case)]
struct Lib {
    eglGetProcAddress: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    eglInitialize: unsafe extern "C" fn(EglDisplay, *mut EglInt, *mut EglInt) -> EglBoolean,
    eglTerminate: unsafe extern "C" fn(EglDisplay) -> EglBoolean,
    eglQueryString: unsafe extern "C" fn(EglDisplay, EglInt) -> *const c_char,
    eglBindAPI: unsafe extern "C" fn(u32) -> EglBoolean,
    eglChooseConfig: unsafe extern "C" fn(
        EglDisplay,
        *const EglInt,
        *mut *mut c_void,
        EglInt,
        *mut EglInt,
    ) -> EglBoolean,
    eglCreateContext:
        unsafe extern "C" fn(EglDisplay, *mut c_void, EglContext, *const EglInt) -> EglContext,
    eglDestroyContext: unsafe extern "C" fn(EglDisplay, EglContext) -> EglBoolean,
    eglMakeCurrent:
        unsafe extern "C" fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> EglBoolean,
    eglGetCurrentContext: unsafe extern "C" fn() -> EglContext,
    eglGetError: unsafe extern "C" fn() -> EglInt,
}

fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: libEGL has no initializers with preconditions.
        let egl = unsafe { libloading::Library::new("libEGL.so.1") }
            .map_err(|e| format!("EGL not installed (libEGL.so.1): {e}"))?;
        macro_rules! sym {
            ($name:ident) => {
                // SAFETY: the field's type is the function's signature in egl.h.
                *unsafe { egl.get(concat!(stringify!($name), "\0").as_bytes()) }
                    .map_err(|e| format!("{}: {e}", stringify!($name)))?
            };
        }
        let lib = Lib {
            eglGetProcAddress: sym!(eglGetProcAddress),
            eglInitialize: sym!(eglInitialize),
            eglTerminate: sym!(eglTerminate),
            eglQueryString: sym!(eglQueryString),
            eglBindAPI: sym!(eglBindAPI),
            eglChooseConfig: sym!(eglChooseConfig),
            eglCreateContext: sym!(eglCreateContext),
            eglDestroyContext: sym!(eglDestroyContext),
            eglMakeCurrent: sym!(eglMakeCurrent),
            eglGetCurrentContext: sym!(eglGetCurrentContext),
            eglGetError: sym!(eglGetError),
        };
        std::mem::forget(egl);
        Ok(lib)
    })
    .as_ref()
    .map_err(|e| CaptureError::NoEncoder(e.clone()))
}

/// Resolves `name` with `eglGetProcAddress` as a function of type `F`.
///
/// # Safety
/// `F` must be the function's C signature.
unsafe fn proc<F: Copy>(lib: &Lib, name: &str) -> Result<F> {
    let c = CString::new(name).map_err(|_| CaptureError::Platform(name.into()))?;
    let p = unsafe { (lib.eglGetProcAddress)(c.as_ptr()) };
    if p.is_null() {
        return Err(CaptureError::Platform(format!("{name} missing")));
    }
    debug_assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
    // SAFETY: the caller names the signature.
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

/// GL functions, resolved for one context.
#[allow(non_snake_case)]
struct Gl {
    glGenTextures: unsafe extern "C" fn(i32, *mut u32),
    glDeleteTextures: unsafe extern "C" fn(i32, *const u32),
    glBindTexture: unsafe extern "C" fn(u32, u32),
    glTexParameteri: unsafe extern "C" fn(u32, u32, i32),
    glTexStorage2D: unsafe extern "C" fn(u32, i32, u32, i32, i32),
    glActiveTexture: unsafe extern "C" fn(u32),
    glGenFramebuffers: unsafe extern "C" fn(i32, *mut u32),
    glDeleteFramebuffers: unsafe extern "C" fn(i32, *const u32),
    glBindFramebuffer: unsafe extern "C" fn(u32, u32),
    glFramebufferTexture2D: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    glCheckFramebufferStatus: unsafe extern "C" fn(u32) -> u32,
    glViewport: unsafe extern "C" fn(i32, i32, i32, i32),
    glClearColor: unsafe extern "C" fn(f32, f32, f32, f32),
    glClear: unsafe extern "C" fn(u32),
    glCreateShader: unsafe extern "C" fn(u32) -> u32,
    glDeleteShader: unsafe extern "C" fn(u32),
    glShaderSource: unsafe extern "C" fn(u32, i32, *const *const c_char, *const i32),
    glCompileShader: unsafe extern "C" fn(u32),
    glGetShaderiv: unsafe extern "C" fn(u32, u32, *mut i32),
    glGetShaderInfoLog: unsafe extern "C" fn(u32, i32, *mut i32, *mut c_char),
    glCreateProgram: unsafe extern "C" fn() -> u32,
    glDeleteProgram: unsafe extern "C" fn(u32),
    glAttachShader: unsafe extern "C" fn(u32, u32),
    glLinkProgram: unsafe extern "C" fn(u32),
    glGetProgramiv: unsafe extern "C" fn(u32, u32, *mut i32),
    glUseProgram: unsafe extern "C" fn(u32),
    glGetUniformLocation: unsafe extern "C" fn(u32, *const c_char) -> i32,
    glUniform1i: unsafe extern "C" fn(i32, i32),
    glUniform3f: unsafe extern "C" fn(i32, f32, f32, f32),
    glUniform4f: unsafe extern "C" fn(i32, f32, f32, f32, f32),
    glGenVertexArrays: unsafe extern "C" fn(i32, *mut u32),
    glDeleteVertexArrays: unsafe extern "C" fn(i32, *const u32),
    glBindVertexArray: unsafe extern "C" fn(u32),
    glDrawArrays: unsafe extern "C" fn(u32, i32, i32),
    glFlush: unsafe extern "C" fn(),
    glGetError: unsafe extern "C" fn() -> u32,
    glEGLImageTargetTexture2DOES: unsafe extern "C" fn(u32, EglImage),
    eglCreateImageKHR:
        unsafe extern "C" fn(EglDisplay, EglContext, u32, *mut c_void, *const EglInt) -> EglImage,
    eglDestroyImageKHR: unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean,
}

impl Gl {
    fn load(lib: &Lib) -> Result<Self> {
        macro_rules! gl {
            ($name:ident) => {
                // SAFETY: the field's type is the function's signature in gl.h / eglext.h.
                unsafe { proc(lib, stringify!($name))? }
            };
        }
        Ok(Self {
            glGenTextures: gl!(glGenTextures),
            glDeleteTextures: gl!(glDeleteTextures),
            glBindTexture: gl!(glBindTexture),
            glTexParameteri: gl!(glTexParameteri),
            glTexStorage2D: gl!(glTexStorage2D),
            glActiveTexture: gl!(glActiveTexture),
            glGenFramebuffers: gl!(glGenFramebuffers),
            glDeleteFramebuffers: gl!(glDeleteFramebuffers),
            glBindFramebuffer: gl!(glBindFramebuffer),
            glFramebufferTexture2D: gl!(glFramebufferTexture2D),
            glCheckFramebufferStatus: gl!(glCheckFramebufferStatus),
            glViewport: gl!(glViewport),
            glClearColor: gl!(glClearColor),
            glClear: gl!(glClear),
            glCreateShader: gl!(glCreateShader),
            glDeleteShader: gl!(glDeleteShader),
            glShaderSource: gl!(glShaderSource),
            glCompileShader: gl!(glCompileShader),
            glGetShaderiv: gl!(glGetShaderiv),
            glGetShaderInfoLog: gl!(glGetShaderInfoLog),
            glCreateProgram: gl!(glCreateProgram),
            glDeleteProgram: gl!(glDeleteProgram),
            glAttachShader: gl!(glAttachShader),
            glLinkProgram: gl!(glLinkProgram),
            glGetProgramiv: gl!(glGetProgramiv),
            glUseProgram: gl!(glUseProgram),
            glGetUniformLocation: gl!(glGetUniformLocation),
            glUniform1i: gl!(glUniform1i),
            glUniform3f: gl!(glUniform3f),
            glUniform4f: gl!(glUniform4f),
            glGenVertexArrays: gl!(glGenVertexArrays),
            glDeleteVertexArrays: gl!(glDeleteVertexArrays),
            glBindVertexArray: gl!(glBindVertexArray),
            glDrawArrays: gl!(glDrawArrays),
            glFlush: gl!(glFlush),
            glGetError: gl!(glGetError),
            glEGLImageTargetTexture2DOES: gl!(glEGLImageTargetTexture2DOES),
            eglCreateImageKHR: gl!(eglCreateImageKHR),
            eglDestroyImageKHR: gl!(eglDestroyImageKHR),
        })
    }
}

/// An OpenGL 3.3 core context without a surface, on the GPU behind a DRM node.
pub(crate) struct Egl {
    lib: &'static Lib,
    display: EglDisplay,
    context: EglContext,
}

impl Egl {
    /// Creates a context on the EGL device whose DRM node is `card` and makes it current.
    pub(crate) fn open(card: &Path) -> Result<Self> {
        let lib = lib()?;
        let no_egl = |what: &str| CaptureError::NoEncoder(format!("EGL: {what}"));
        // SAFETY: client extension entry points with the signatures in eglext.h.
        let query_devices: unsafe extern "C" fn(EglInt, *mut EglDevice, *mut EglInt) -> EglBoolean =
            unsafe { proc(lib, "eglQueryDevicesEXT") }
                .map_err(|_| no_egl("no EGL_EXT_device_enumeration"))?;
        let query_device_string: unsafe extern "C" fn(EglDevice, EglInt) -> *const c_char =
            unsafe { proc(lib, "eglQueryDeviceStringEXT") }
                .map_err(|_| no_egl("no EGL_EXT_device_query"))?;
        let platform_display: unsafe extern "C" fn(u32, *mut c_void, *const EglInt) -> EglDisplay =
            unsafe { proc(lib, "eglGetPlatformDisplayEXT") }
                .map_err(|_| no_egl("no EGL_EXT_platform_base"))?;
        let mut devices = [std::ptr::null_mut(); 16];
        let mut n = 0;
        if unsafe { query_devices(devices.len() as EglInt, devices.as_mut_ptr(), &mut n) } == 0 {
            return Err(no_egl("eglQueryDevicesEXT failed"));
        }
        let wanted = std::fs::canonicalize(card).unwrap_or_else(|_| card.to_owned());
        let device = devices[..n.clamp(0, 16) as usize]
            .iter()
            .copied()
            .find(|&d| {
                let s = unsafe { query_device_string(d, EGL_DRM_DEVICE_FILE_EXT) };
                !s.is_null() && {
                    let path = unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
                    std::fs::canonicalize(&path).is_ok_and(|p| p == wanted)
                }
            })
            .ok_or_else(|| no_egl(&format!("no EGL device for {}", card.display())))?;
        let display =
            unsafe { platform_display(EGL_PLATFORM_DEVICE_EXT, device, std::ptr::null()) };
        if display.is_null() {
            return Err(no_egl("eglGetPlatformDisplayEXT failed"));
        }
        let (mut major, mut minor) = (0, 0);
        if unsafe { (lib.eglInitialize)(display, &mut major, &mut minor) } == 0 {
            return Err(no_egl("eglInitialize failed"));
        }
        let fail = |what: &str| {
            let e = unsafe { (lib.eglGetError)() };
            unsafe { (lib.eglTerminate)(display) };
            no_egl(&format!("{what} (EGL error {e:#x})"))
        };
        let extensions = unsafe { (lib.eglQueryString)(display, EGL_EXTENSIONS) };
        let extensions = if extensions.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(extensions) }
                .to_string_lossy()
                .into_owned()
        };
        let has = |e: &str| extensions.split_ascii_whitespace().any(|x| x == e);
        if !has("EGL_EXT_image_dma_buf_import") || !has("EGL_KHR_surfaceless_context") {
            return Err(fail("no DMA-BUF import or surfaceless contexts"));
        }
        if unsafe { (lib.eglBindAPI)(EGL_OPENGL_API) } == 0 {
            return Err(fail("eglBindAPI(OpenGL)"));
        }
        let mut config = std::ptr::null_mut();
        if !has("EGL_KHR_no_config_context") {
            let attribs = [EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE];
            let mut count = 0;
            let ok = unsafe {
                (lib.eglChooseConfig)(display, attribs.as_ptr(), &mut config, 1, &mut count)
            };
            if ok == 0 || count == 0 {
                return Err(fail("no OpenGL config"));
            }
        }
        let attribs = [
            EGL_CONTEXT_MAJOR_VERSION,
            3,
            EGL_CONTEXT_MINOR_VERSION,
            3,
            EGL_CONTEXT_OPENGL_PROFILE_MASK,
            EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
            EGL_NONE,
        ];
        let context = unsafe {
            (lib.eglCreateContext)(display, config, std::ptr::null_mut(), attribs.as_ptr())
        };
        if context.is_null() {
            return Err(fail("eglCreateContext (OpenGL 3.3 core)"));
        }
        let egl = Self {
            lib,
            display,
            context,
        };
        egl.make_current()?;
        Ok(egl)
    }

    /// Makes the context current on the calling thread if it is not.
    pub(crate) fn make_current(&self) -> Result<()> {
        if unsafe { (self.lib.eglGetCurrentContext)() } == self.context {
            return Ok(());
        }
        let ok = unsafe {
            (self.lib.eglMakeCurrent)(
                self.display,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.context,
            )
        };
        if ok == 0 {
            return Err(CaptureError::Platform(format!(
                "eglMakeCurrent: EGL error {:#x}",
                unsafe { (self.lib.eglGetError)() }
            )));
        }
        Ok(())
    }
}

impl Drop for Egl {
    fn drop(&mut self) {
        unsafe {
            if (self.lib.eglGetCurrentContext)() == self.context {
                (self.lib.eglMakeCurrent)(
                    self.display,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
            (self.lib.eglDestroyContext)(self.display, self.context);
            (self.lib.eglTerminate)(self.display);
        }
    }
}

const VERTEX: &str = "#version 330 core
void main() {
    vec2 p = vec2(float((gl_VertexID & 1) * 4 - 1), float((gl_VertexID & 2) * 2 - 1));
    gl_Position = vec4(p, 0.0, 1.0);
}
";

const FRAGMENT: &str = "#version 330 core
uniform sampler2D tex;
uniform vec4 dst;
uniform vec3 tx;
uniform vec3 ty;
uniform int chroma;
out vec4 color;
void main() {
    vec3 p = vec3((gl_FragCoord.xy - dst.xy) / dst.zw, 1.0);
    vec3 rgb = texture(tex, vec2(dot(tx, p), dot(ty, p))).rgb;
    if (chroma == 0) {
        color = vec4((16.0 + 219.0 * dot(rgb, vec3(0.2126, 0.7152, 0.0722))) / 255.0, 0.0, 0.0, 1.0);
    } else {
        color = vec4((128.0 + 224.0 * dot(rgb, vec3(-0.1146, -0.3854, 0.5))) / 255.0,
                     (128.0 + 224.0 * dot(rgb, vec3(0.5, -0.4542, -0.0458))) / 255.0, 0.0, 1.0);
    }
}
";

/// Scanout buffers kept imported.
const IMPORT_CACHE: usize = 6;

struct Imported {
    key: FrameKey,
    image: EglImage,
    texture: u32,
}

/// Renders scanouts into two textures, Y (`R8`, full size) and UV (`RG8`, half size).
pub(crate) struct Converter {
    gl: Gl,
    display: EglDisplay,
    program: u32,
    vao: u32,
    /// `tex`, `dst`, `tx`, `ty`, `chroma`.
    uniforms: [i32; 5],
    pub width: u32,
    pub height: u32,
    /// Y and UV textures, and their framebuffers.
    pub planes: [u32; 2],
    framebuffers: [u32; 2],
    imported: Vec<Imported>,
}

impl Converter {
    /// A converter to `width` x `height` on `egl`'s context, which must be current.
    pub(crate) fn new(egl: &Egl, width: u32, height: u32) -> Result<Self> {
        let gl = Gl::load(egl.lib)?;
        let program = unsafe { link(&gl)? };
        let mut vao = 0;
        unsafe {
            (gl.glGenVertexArrays)(1, &mut vao);
        }
        let uniform = |name: &str| {
            let c = CString::new(name).unwrap_or_default();
            unsafe { (gl.glGetUniformLocation)(program, c.as_ptr()) }
        };
        let uniforms = [
            uniform("tex"),
            uniform("dst"),
            uniform("tx"),
            uniform("ty"),
            uniform("chroma"),
        ];
        let mut c = Self {
            gl,
            display: egl.display,
            program,
            vao,
            uniforms,
            width: 0,
            height: 0,
            planes: [0; 2],
            framebuffers: [0; 2],
            imported: Vec::new(),
        };
        c.resize(width, height)?;
        Ok(c)
    }

    /// Recreates the NV12 planes at `width` x `height`.
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let gl = &self.gl;
        unsafe {
            (gl.glDeleteFramebuffers)(2, self.framebuffers.as_ptr());
            (gl.glDeleteTextures)(2, self.planes.as_ptr());
            (gl.glGenTextures)(2, self.planes.as_mut_ptr());
            (gl.glGenFramebuffers)(2, self.framebuffers.as_mut_ptr());
            for (i, (format, w, h)) in [(GL_R8, width, height), (GL_RG8, width / 2, height / 2)]
                .into_iter()
                .enumerate()
            {
                (gl.glBindTexture)(GL_TEXTURE_2D, self.planes[i]);
                (gl.glTexStorage2D)(GL_TEXTURE_2D, 1, format, w as i32, h as i32);
                (gl.glBindFramebuffer)(GL_FRAMEBUFFER, self.framebuffers[i]);
                (gl.glFramebufferTexture2D)(
                    GL_FRAMEBUFFER,
                    GL_COLOR_ATTACHMENT0,
                    GL_TEXTURE_2D,
                    self.planes[i],
                    0,
                );
                if (gl.glCheckFramebufferStatus)(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                    return Err(CaptureError::Platform("NV12 framebuffer incomplete".into()));
                }
            }
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, 0);
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Renders `frame` into the planes; the GPU work is queued, not waited for.
    pub(crate) fn convert(&mut self, frame: &Frame) -> Result<()> {
        let texture = self.import(frame)?;
        let gl = &self.gl;
        let c = frame.crop;
        let (upright_w, upright_h) = frame.transform.size(c.width, c.height);
        let r = letterbox(upright_w, upright_h, self.width, self.height);
        let [tx, ty] = texture_map(c, frame.width, frame.height, frame.transform);
        unsafe {
            (gl.glUseProgram)(self.program);
            (gl.glBindVertexArray)(self.vao);
            (gl.glActiveTexture)(GL_TEXTURE0);
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            (gl.glUniform1i)(self.uniforms[0], 0);
            (gl.glUniform3f)(self.uniforms[2], tx[0], tx[1], tx[2]);
            (gl.glUniform3f)(self.uniforms[3], ty[0], ty[1], ty[2]);
            for (i, (div, clear)) in [(1u32, [16.0f32, 0.0]), (2, [128.0, 128.0])]
                .into_iter()
                .enumerate()
            {
                (gl.glBindFramebuffer)(GL_FRAMEBUFFER, self.framebuffers[i]);
                (gl.glViewport)(0, 0, (self.width / div) as i32, (self.height / div) as i32);
                (gl.glClearColor)(clear[0] / 255.0, clear[1] / 255.0, 0.0, 1.0);
                (gl.glClear)(GL_COLOR_BUFFER_BIT);
                let (x, y) = (r.left / div, r.top / div);
                let (w, h) = ((r.right - r.left) / div, (r.bottom - r.top) / div);
                (gl.glViewport)(x as i32, y as i32, w as i32, h as i32);
                (gl.glUniform4f)(self.uniforms[1], x as f32, y as f32, w as f32, h as f32);
                (gl.glUniform1i)(self.uniforms[4], i as i32);
                (gl.glDrawArrays)(GL_TRIANGLES, 0, 3);
            }
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, 0);
            (gl.glFlush)();
            let e = (gl.glGetError)();
            if e != 0 {
                return Err(CaptureError::Platform(format!("OpenGL error {e:#x}")));
            }
        }
        Ok(())
    }

    /// The scanout buffer as a texture, imported once per buffer.
    fn import(&mut self, frame: &Frame) -> Result<u32> {
        let key = frame.key();
        if let Some(i) = self.imported.iter().position(|e| e.key == key) {
            let entry = self.imported.remove(i);
            let texture = entry.texture;
            self.imported.push(entry);
            return Ok(texture);
        }
        let attribs = dmabuf_attribs(frame);
        let gl = &self.gl;
        let image = unsafe {
            (gl.eglCreateImageKHR)(
                self.display,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(CaptureError::Platform(format!(
                "eglCreateImage could not import the scanout ({}x{}, modifier {:#x})",
                frame.width, frame.height, frame.modifier
            )));
        }
        let mut texture = 0;
        unsafe {
            (gl.glGenTextures)(1, &mut texture);
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            (gl.glEGLImageTargetTexture2DOES)(GL_TEXTURE_2D, image);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        }
        if self.imported.len() >= IMPORT_CACHE {
            let old = self.imported.remove(0);
            self.release(&old);
        }
        self.imported.push(Imported {
            key,
            image,
            texture,
        });
        Ok(texture)
    }

    fn release(&self, e: &Imported) {
        unsafe {
            (self.gl.glDeleteTextures)(1, &e.texture);
            (self.gl.eglDestroyImageKHR)(self.display, e.image);
        }
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        for e in &self.imported {
            self.release(e);
        }
        let gl = &self.gl;
        unsafe {
            (gl.glDeleteFramebuffers)(2, self.framebuffers.as_ptr());
            (gl.glDeleteTextures)(2, self.planes.as_ptr());
            (gl.glDeleteVertexArrays)(1, &self.vao);
            (gl.glDeleteProgram)(self.program);
        }
    }
}

/// The texture coordinate of each point of the output picture (unit square, rows from the top),
/// as `[tx, ty]` with `coord = (dot(tx, (x, y, 1)), dot(ty, (x, y, 1)))`: the inverse of
/// `transform`, into `crop` of a `width` x `height` buffer.
fn texture_map(crop: Crop, width: u32, height: u32, transform: Transform) -> [[f32; 3]; 2] {
    let m = transform.inverse().affine();
    let axis = |row: [i32; 3], offset: u32, extent: u32, size: u32| {
        let (extent, size) = (extent as f32, size as f32);
        [
            row[0] as f32 * extent / size,
            row[1] as f32 * extent / size,
            (offset as f32 + row[2] as f32 * extent) / size,
        ]
    };
    [
        axis(m[0], crop.x, crop.width, width),
        axis(m[1], crop.y, crop.height, height),
    ]
}

/// `eglCreateImageKHR` attributes for a scanout DMA-BUF.
fn dmabuf_attribs(frame: &Frame) -> Vec<EglInt> {
    let mut a = vec![
        EGL_WIDTH,
        frame.width as EglInt,
        EGL_HEIGHT,
        frame.height as EglInt,
        EGL_LINUX_DRM_FOURCC_EXT,
        frame.fourcc as EglInt,
    ];
    for (i, p) in frame.planes.iter().enumerate().take(4) {
        let names = PLANE_ATTRIBS[i];
        a.extend_from_slice(&[
            names[0],
            frame.objects[p.object].as_raw_fd(),
            names[1],
            p.offset as EglInt,
            names[2],
            p.pitch as EglInt,
        ]);
        if frame.modifier != MOD_INVALID {
            a.extend_from_slice(&[
                names[3],
                (frame.modifier & 0xffff_ffff) as u32 as EglInt,
                names[4],
                (frame.modifier >> 32) as u32 as EglInt,
            ]);
        }
    }
    a.push(EGL_NONE);
    a
}

/// Compiles and links the conversion program.
///
/// # Safety
/// A GL context must be current.
unsafe fn link(gl: &Gl) -> Result<u32> {
    let compile = |kind: u32, source: &str| -> Result<u32> {
        let shader = unsafe { (gl.glCreateShader)(kind) };
        let c = CString::new(source).unwrap_or_default();
        let ptr = c.as_ptr();
        unsafe {
            (gl.glShaderSource)(shader, 1, &ptr, std::ptr::null());
            (gl.glCompileShader)(shader);
        }
        let mut ok = 0;
        unsafe { (gl.glGetShaderiv)(shader, GL_COMPILE_STATUS, &mut ok) };
        if ok == 0 {
            let mut log = [0 as c_char; 1024];
            let mut len = 0;
            unsafe {
                (gl.glGetShaderInfoLog)(shader, log.len() as i32, &mut len, log.as_mut_ptr());
                (gl.glDeleteShader)(shader);
            }
            let text = unsafe { CStr::from_ptr(log.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(CaptureError::Platform(format!("shader: {text}")));
        }
        Ok(shader)
    };
    let vs = compile(GL_VERTEX_SHADER, VERTEX)?;
    let fs = match compile(GL_FRAGMENT_SHADER, FRAGMENT) {
        Ok(fs) => fs,
        Err(e) => {
            unsafe { (gl.glDeleteShader)(vs) };
            return Err(e);
        }
    };
    unsafe {
        let program = (gl.glCreateProgram)();
        (gl.glAttachShader)(program, vs);
        (gl.glAttachShader)(program, fs);
        (gl.glLinkProgram)(program);
        (gl.glDeleteShader)(vs);
        (gl.glDeleteShader)(fs);
        let mut ok = 0;
        (gl.glGetProgramiv)(program, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            (gl.glDeleteProgram)(program);
            return Err(CaptureError::Platform(
                "shader program failed to link".into(),
            ));
        }
        Ok(program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_limited_coefficients() {
        // White and black land on the limited range ends; grey has no chroma.
        let y = |r: f32, g: f32, b: f32| 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b);
        let u = |r: f32, g: f32, b: f32| 128.0 + 224.0 * (-0.1146 * r - 0.3854 * g + 0.5 * b);
        let v = |r: f32, g: f32, b: f32| 128.0 + 224.0 * (0.5 * r - 0.4542 * g - 0.0458 * b);
        assert!((y(1.0, 1.0, 1.0) - 235.0).abs() < 0.01);
        assert!((y(0.0, 0.0, 0.0) - 16.0).abs() < 0.01);
        assert!((u(0.5, 0.5, 0.5) - 128.0).abs() < 0.01);
        assert!((v(0.5, 0.5, 0.5) - 128.0).abs() < 0.01);
        assert!((u(0.0, 0.0, 1.0) - 240.0).abs() < 0.01);
        assert!((v(1.0, 0.0, 0.0) - 240.0).abs() < 0.01);
        assert!(FRAGMENT.contains("0.2126, 0.7152, 0.0722"));
    }

    #[test]
    fn texture_coordinates_undo_every_transform() {
        use crate::linux::transform::apply;
        let crop = Crop {
            x: 100,
            y: 50,
            width: 400,
            height: 300,
        };
        let (w, h) = (1000.0, 500.0);
        let texel = |(x, y): (f64, f64)| ((100.0 + x * 400.0) / w, (50.0 + y * 300.0) / h);
        for t in Transform::all() {
            let [tx, ty] = texture_map(crop, 1000, 500, t);
            for q in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.3, 0.8)] {
                let (px, py) = apply(t.affine(), q);
                let got = (
                    f64::from(tx[0]) * px + f64::from(tx[1]) * py + f64::from(tx[2]),
                    f64::from(ty[0]) * px + f64::from(ty[1]) * py + f64::from(ty[2]),
                );
                let want = texel(q);
                assert!(
                    (got.0 - want.0).abs() < 1e-6 && (got.1 - want.1).abs() < 1e-6,
                    "{t:?} at {q:?}: {got:?} != {want:?}"
                );
            }
        }
        assert_eq!(
            texture_map(crop, 1000, 500, Transform::IDENTITY),
            [[0.4, 0.0, 0.1], [0.0, 0.6, 0.1]]
        );
    }

    #[test]
    fn plane_attributes_follow_eglext() {
        assert_eq!(PLANE_ATTRIBS[1][0], 0x3275);
        assert_eq!(PLANE_ATTRIBS[3][4], 0x344a);
    }
}
