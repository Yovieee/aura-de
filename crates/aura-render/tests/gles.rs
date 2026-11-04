//! Compile/link both canonical GLES3 and Smithay GLES2 shader variants on a
//! real driver. Uses dynamically loaded EGL so normal CPU tests need no EGL.
use aura_render::{
    DUAL_KAWASE_DOWN_GLES_SOURCE, DUAL_KAWASE_UP_GLES_SOURCE, LIQUID_GLASS_GLES_SOURCE,
    smithay_glass_shader, smithay_kawase_down_shader, smithay_kawase_up_shader,
};
use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr;

type Handle = *mut c_void;

#[test]
#[ignore = "requires surfaceless EGL/GLES3; run explicitly in the GPU smoke job"]
fn canonical_and_smithay_shaders_compile_and_link() {
    // SAFETY: all dynamically loaded functions use EGL/GLES C ABI signatures;
    // the libraries outlive every symbol, the context stays current until all
    // shader/program objects have been deleted, and source buffers stay alive
    // through glShaderSource (which copies their bytes).
    unsafe {
        let egl = libloading::Library::new("libEGL.so.1").expect("EGL driver required");
        let gl = libloading::Library::new("libGLESv2.so.2").expect("GLES driver required");
        let get_display: libloading::Symbol<
            unsafe extern "C" fn(u32, Handle, *const isize) -> Handle,
        > = egl.get(b"eglGetPlatformDisplay\0").unwrap();
        let display = get_display(0x31DD, ptr::null_mut(), ptr::null()); // EGL_PLATFORM_SURFACELESS_MESA
        assert!(!display.is_null(), "surfaceless EGL display");
        let initialize: libloading::Symbol<
            unsafe extern "C" fn(Handle, *mut c_int, *mut c_int) -> u32,
        > = egl.get(b"eglInitialize\0").unwrap();
        assert_ne!(
            initialize(display, ptr::null_mut(), ptr::null_mut()),
            0,
            "eglInitialize"
        );
        let bind_api: libloading::Symbol<unsafe extern "C" fn(u32) -> u32> =
            egl.get(b"eglBindAPI\0").unwrap();
        assert_ne!(bind_api(0x30A0), 0); // EGL_OPENGL_ES_API
        let choose: libloading::Symbol<
            unsafe extern "C" fn(Handle, *const c_int, *mut Handle, c_int, *mut c_int) -> u32,
        > = egl.get(b"eglChooseConfig\0").unwrap();
        let attrs = [0x3033, 1, 0x3040, 0x40, 0x3038]; // pbuffer, ES3, terminator
        let mut config = ptr::null_mut();
        let mut count = 0;
        assert_ne!(
            choose(display, attrs.as_ptr(), &mut config, 1, &mut count),
            0
        );
        assert_eq!(count, 1);
        let create_context: libloading::Symbol<
            unsafe extern "C" fn(Handle, Handle, Handle, *const c_int) -> Handle,
        > = egl.get(b"eglCreateContext\0").unwrap();
        let context = create_context(
            display,
            config,
            ptr::null_mut(),
            [0x3098, 3, 0x3038].as_ptr(),
        );
        assert!(!context.is_null(), "GLES3 context");
        let create_surface: libloading::Symbol<
            unsafe extern "C" fn(Handle, Handle, *const c_int) -> Handle,
        > = egl.get(b"eglCreatePbufferSurface\0").unwrap();
        let surface = create_surface(display, config, [0x3057, 1, 0x3056, 1, 0x3038].as_ptr());
        assert!(!surface.is_null());
        let make_current: libloading::Symbol<
            unsafe extern "C" fn(Handle, Handle, Handle, Handle) -> u32,
        > = egl.get(b"eglMakeCurrent\0").unwrap();
        assert_ne!(make_current(display, surface, surface, context), 0);
        let vertex3 = "#version 300 es\nin vec2 position; out vec2 v_tex_coords; void main(){gl_Position=vec4(position,0.,1.);v_tex_coords=position*.5+.5;}";
        for fragment in [
            LIQUID_GLASS_GLES_SOURCE,
            DUAL_KAWASE_DOWN_GLES_SOURCE,
            DUAL_KAWASE_UP_GLES_SOURCE,
        ] {
            link(&gl, vertex3, fragment);
        }
        let vertex2 = "#version 100\nattribute vec2 position; varying vec2 v_coords; void main(){gl_Position=vec4(position,0.,1.);v_coords=position*.5+.5;}";
        for shader in [
            smithay_glass_shader(),
            smithay_kawase_down_shader(),
            smithay_kawase_up_shader(),
        ] {
            for defines in [
                "",
                "#define NO_ALPHA",
                "#define DEBUG_FLAGS",
                "#define EXTERNAL",
                "#define EXTERNAL\n#define DEBUG_FLAGS",
            ] {
                link(&gl, vertex2, &shader.replace("//_DEFINES_", defines));
            }
        }
        assert_ne!(
            make_current(display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut()),
            0
        );
        let destroy_surface: libloading::Symbol<unsafe extern "C" fn(Handle, Handle) -> u32> =
            egl.get(b"eglDestroySurface\0").unwrap();
        let destroy_context: libloading::Symbol<unsafe extern "C" fn(Handle, Handle) -> u32> =
            egl.get(b"eglDestroyContext\0").unwrap();
        let terminate: libloading::Symbol<unsafe extern "C" fn(Handle) -> u32> =
            egl.get(b"eglTerminate\0").unwrap();
        destroy_surface(display, surface);
        destroy_context(display, context);
        terminate(display);
    }
}

unsafe fn link(gl: &libloading::Library, vertex: &str, fragment: &str) {
    // SAFETY: caller provides current GLES3 context and a loaded GLES library.
    unsafe {
        let create_shader: libloading::Symbol<unsafe extern "C" fn(u32) -> u32> =
            gl.get(b"glCreateShader\0").unwrap();
        let source: libloading::Symbol<
            unsafe extern "C" fn(u32, c_int, *const *const c_char, *const c_int),
        > = gl.get(b"glShaderSource\0").unwrap();
        let compile: libloading::Symbol<unsafe extern "C" fn(u32)> =
            gl.get(b"glCompileShader\0").unwrap();
        let get_shader: libloading::Symbol<unsafe extern "C" fn(u32, u32, *mut c_int)> =
            gl.get(b"glGetShaderiv\0").unwrap();
        let shader_log: libloading::Symbol<
            unsafe extern "C" fn(u32, c_int, *mut c_int, *mut c_char),
        > = gl.get(b"glGetShaderInfoLog\0").unwrap();
        let delete_shader: libloading::Symbol<unsafe extern "C" fn(u32)> =
            gl.get(b"glDeleteShader\0").unwrap();
        let create_program: libloading::Symbol<unsafe extern "C" fn() -> u32> =
            gl.get(b"glCreateProgram\0").unwrap();
        let attach: libloading::Symbol<unsafe extern "C" fn(u32, u32)> =
            gl.get(b"glAttachShader\0").unwrap();
        let link_program: libloading::Symbol<unsafe extern "C" fn(u32)> =
            gl.get(b"glLinkProgram\0").unwrap();
        let get_program: libloading::Symbol<unsafe extern "C" fn(u32, u32, *mut c_int)> =
            gl.get(b"glGetProgramiv\0").unwrap();
        let program_log: libloading::Symbol<
            unsafe extern "C" fn(u32, c_int, *mut c_int, *mut c_char),
        > = gl.get(b"glGetProgramInfoLog\0").unwrap();
        let delete_program: libloading::Symbol<unsafe extern "C" fn(u32)> =
            gl.get(b"glDeleteProgram\0").unwrap();
        let program = create_program();
        for (kind, text) in [(0x8B31, vertex), (0x8B30, fragment)] {
            let shader = create_shader(kind);
            let text = CString::new(text).unwrap();
            source(shader, 1, &text.as_ptr(), ptr::null());
            compile(shader);
            let mut ok = 0;
            get_shader(shader, 0x8B81, &mut ok);
            let mut log = vec![0u8; 8192];
            shader_log(
                shader,
                log.len() as c_int,
                ptr::null_mut(),
                log.as_mut_ptr().cast(),
            );
            assert_ne!(
                ok,
                0,
                "shader compilation: {}",
                String::from_utf8_lossy(&log)
            );
            attach(program, shader);
            delete_shader(shader);
        }
        link_program(program);
        let mut ok = 0;
        get_program(program, 0x8B82, &mut ok);
        let mut log = vec![0u8; 8192];
        program_log(
            program,
            log.len() as c_int,
            ptr::null_mut(),
            log.as_mut_ptr().cast(),
        );
        assert_ne!(ok, 0, "shader linking: {}", String::from_utf8_lossy(&log));
        delete_program(program);
    }
}
