//! A window that draws like a game, to test the game capture hook on: red top
//! half, blue bottom half (so a picture upside down shows), a white bar
//! moving across (so frames differ), with Direct3D 11 or (`GL=1`) OpenGL,
//! the way osu! and Geometry Dash draw. Closes after `SECS` seconds (10).
//! `FPS=n`: exactly n pictures a second, the bar 4 px further in each (to
//! check frame blending: each recorded frame should show the bar's
//! positions in its time, evenly). `NOVSYNC=1` (OpenGL): faster than the
//! display's refresh rate. `SIZE=WxH`: draw that big (800×600).
//! `FULLSCREEN=1`: a borderless window exactly covering the main display, as
//! a game in fullscreen is (OpenGL's then goes straight to the display, past
//! Windows' compositor, with some drivers: issue #11). `HIDE_CURSOR=1`: no
//! cursor over it, and kept in its middle, as a game being played does;
//! `HIDE_CURSOR=null`: the same, hidden the way GLFW (Minecraft 1.13 on) does,
//! with no cursor image rather than none shown. `TIMES=1`: how far apart its
//! pictures came, at the end. `PROTECT=1`: no other program may open its
//! process, as anti-cheat keeps every other program out of its game's
//! (issue #62); nor end it, so it closes only after `SECS`.
//!
//! `REMAKE=<what>`: every `REMAKE_EVERY` seconds (3) it makes its picture
//! anew, as games do on a map change or a video restart (issue #74):
//! - `context` (OpenGL): a new GL context in the same window, the old one
//!   deleted.
//! - `window`: a new window, the old one destroyed: in OpenGL its GL context
//!   deleted first (as id Tech 3's own window code does), in Direct3D 11
//!   with a new device and swap chain.
//! - `window-leak`: the same, but what drew in the old window is let go of
//!   without being deleted: in OpenGL its GL context, as SDL does when a
//!   game restarts its video (ioquake3's `vid_restart`), in Direct3D 11 its
//!   device and swap chain.
//! - `child`: drawn in a child window filling the window (as some engines
//!   and emulators draw), the child made anew (the old one destroyed, its GL
//!   context not deleted) while the window stays.
//! - `swapchain` (Direct3D 11): a new swap chain on the same device.
//! - `device` (Direct3D 11): a new device and swap chain.
//!
//! Each says so on stdout. `ASIDE=1` (for tests run while someone's at the
//! PC): its windows go in the bottom right corner of a display other than
//! the main one (if there's one), over the other windows, never taking focus.
//!
//!   cargo run -p capture --example hook_target
//!   APP=hook_target.exe HOOK=1 SECS=5 HEIGHT=0 cargo run -p capture --example record
#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::Graphics::Gdi::{GetDC, HDC, ReleaseDC};
    use windows::Win32::Graphics::OpenGL::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{Interface, w};

    static NO_CURSOR_IMAGE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_SETCURSOR && (lp.0 & 0xFFFF) as u32 == HTCLIENT && NO_CURSOR_IMAGE.load(std::sync::atomic::Ordering::Relaxed) {
            unsafe { SetCursor(None) };
            return LRESULT(1);
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    /// What draws its pictures, in the window it draws in.
    enum Drawer {
        Gl { window: HWND, dc: HDC, context: HGLRC },
        D3d { device: ID3D11Device, context: ID3D11DeviceContext, context1: ID3D11DeviceContext1, swap: IDXGISwapChain, rtv: ID3D11RenderTargetView, view: ID3D11View },
    }

    /// A GL context for `dc`, made current.
    fn gl_context(dc: HDC, novsync: bool) -> windows::core::Result<HGLRC> {
        unsafe {
            let context = wglCreateContext(dc)?;
            wglMakeCurrent(dc, context)?;
            // NOVSYNC=1: not held to the display's refresh, like a game with
            // vsync off (else `FPS` above the refresh rate isn't reached).
            if novsync && let Some(f) = wglGetProcAddress(windows::core::s!("wglSwapIntervalEXT")) {
                let swap_interval: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
                swap_interval(0);
            }
            Ok(context)
        }
    }

    fn swap_chain_desc(window: HWND, w: i32, h: i32) -> DXGI_SWAP_CHAIN_DESC {
        DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC { Width: w as u32, Height: h as u32, Format: DXGI_FORMAT_B8G8R8A8_UNORM, ..Default::default() },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: window,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: 0,
        }
    }

    impl Drawer {
        fn new(gl: bool, window: HWND, w: i32, h: i32, novsync: bool) -> windows::core::Result<Self> {
            if gl {
                unsafe {
                    let dc = GetDC(Some(window));
                    let pfd = PIXELFORMATDESCRIPTOR {
                        nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
                        nVersion: 1,
                        dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                        iPixelType: PFD_TYPE_RGBA,
                        cColorBits: 32,
                        cDepthBits: 24,
                        ..Default::default()
                    };
                    let format = ChoosePixelFormat(dc, &pfd);
                    SetPixelFormat(dc, format, &pfd)?;
                    let context = gl_context(dc, novsync)?;
                    return Ok(Drawer::Gl { window, dc, context });
                }
            }
            let desc = swap_chain_desc(window, w, h);
            let (mut swap, mut device, mut context) = (None, None, None);
            unsafe {
                D3D11CreateDeviceAndSwapChain(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    Default::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&desc),
                    Some(&mut swap),
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )?;
            }
            let (swap, device, context): (IDXGISwapChain, ID3D11Device, ID3D11DeviceContext) = (swap.unwrap(), device.unwrap(), context.unwrap());
            Self::d3d(device, context, swap)
        }

        fn d3d(device: ID3D11Device, context: ID3D11DeviceContext, swap: IDXGISwapChain) -> windows::core::Result<Self> {
            let context1: ID3D11DeviceContext1 = context.cast()?;
            let back: ID3D11Texture2D = unsafe { swap.GetBuffer(0)? };
            let mut rtv = None;
            unsafe { device.CreateRenderTargetView(&back, None, Some(&mut rtv))? };
            let rtv = rtv.unwrap();
            let view: ID3D11View = rtv.cast()?;
            Ok(Drawer::D3d { device, context, context1, swap, rtv, view })
        }

        fn draw(&self, t: f32, w: i32, h: i32) {
            match self {
                Drawer::Gl { dc, .. } => unsafe {
                    glViewport(0, 0, w, h);
                    glClearColor(0.0, 0.0, 1.0, 1.0);
                    glClear(GL_COLOR_BUFFER_BIT);
                    glEnable(GL_SCISSOR_TEST);
                    // OpenGL counts rows from the bottom: this is the top half.
                    glScissor(0, h / 2, w, h / 2);
                    glClearColor(1.0, 0.0, 0.0, 1.0);
                    glClear(GL_COLOR_BUFFER_BIT);
                    glScissor(((t * 200.0) as i32) % w, 0, 40, h);
                    glClearColor(1.0, 1.0, 1.0, 1.0);
                    glClear(GL_COLOR_BUFFER_BIT);
                    glDisable(GL_SCISSOR_TEST);
                    let _ = SwapBuffers(*dc);
                },
                Drawer::D3d { context, context1, swap, rtv, view, .. } => unsafe {
                    context.ClearRenderTargetView(rtv, &[0.0, 0.0, 1.0, 1.0]);
                    context1.ClearView(view, &[1.0, 0.0, 0.0, 1.0], Some(&[RECT { left: 0, top: 0, right: w, bottom: h / 2 }]));
                    let x = ((t * 200.0) as i32) % w;
                    context1.ClearView(view, &[1.0, 1.0, 1.0, 1.0], Some(&[RECT { left: x, top: 0, right: x + 40, bottom: h }]));
                    let _ = swap.Present(1, Default::default());
                },
            }
        }

        /// Done with: deleted (`delete`), or let go of as it is (a GL
        /// context left undeleted, a device and swap chain never released).
        fn close(self, delete: bool) {
            match self {
                Drawer::Gl { window, dc, context } => unsafe {
                    let _ = wglMakeCurrent(HDC::default(), HGLRC::default());
                    if delete {
                        let _ = wglDeleteContext(context);
                    }
                    ReleaseDC(Some(window), dc);
                },
                Drawer::D3d { context, .. } if delete => unsafe {
                    // Nothing left holding the swap chain, so another can be
                    // made for the window at once.
                    context.ClearState();
                    context.Flush();
                },
                d3d => std::mem::forget(d3d),
            }
        }

        /// A new GL context in the same window, the old one deleted.
        fn new_context(&mut self, novsync: bool) -> windows::core::Result<()> {
            if let Drawer::Gl { dc, context, .. } = self {
                unsafe {
                    let _ = wglMakeCurrent(HDC::default(), HGLRC::default());
                    wglDeleteContext(*context)?;
                    *context = gl_context(*dc, novsync)?;
                }
            }
            Ok(())
        }

        /// A new swap chain on the same device and window, the old one
        /// released.
        fn new_swap_chain(self, window: HWND, w: i32, h: i32) -> windows::core::Result<Self> {
            let Drawer::D3d { device, context, swap, rtv, view, context1 } = self else { return Ok(self) };
            drop((swap, rtv, view, context1));
            unsafe {
                context.ClearState();
                context.Flush();
            }
            let factory: IDXGIFactory = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()? };
            let mut swap = None;
            unsafe { factory.CreateSwapChain(&device, &swap_chain_desc(window, w, h), &mut swap).ok()? };
            Self::d3d(device, context, swap.unwrap())
        }
    }

    let secs: f64 = std::env::var("SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let gl = std::env::var("GL").is_ok_and(|v| v == "1");
    // STATIC=1: the same picture every frame (still presented each frame).
    let still = std::env::var("STATIC").is_ok_and(|v| v == "1");
    let fullscreen = std::env::var("FULLSCREEN").is_ok_and(|v| v == "1");
    let novsync = std::env::var("NOVSYNC").is_ok_and(|v| v == "1");
    let aside = std::env::var("ASIDE").is_ok_and(|v| v == "1");
    let remake = std::env::var("REMAKE").ok();
    let remake_every: f64 = std::env::var("REMAKE_EVERY").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let hide_cursor = std::env::var("HIDE_CURSOR").ok().filter(|v| v == "1" || v == "null");
    NO_CURSOR_IMAGE.store(hide_cursor.as_deref() == Some("null"), std::sync::atomic::Ordering::Relaxed);
    if std::env::var("PROTECT").is_ok_and(|v| v == "1") {
        // Its process's access list emptied: no one is let in (its owner may
        // still change the list, and nothing else).
        use windows::Win32::Security::{
            ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl, InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR,
            SetKernelObjectSecurity, SetSecurityDescriptorDacl,
        };
        unsafe {
            let mut acl = ACL::default();
            InitializeAcl(&mut acl, std::mem::size_of::<ACL>() as u32, ACL_REVISION)?;
            let mut sd = SECURITY_DESCRIPTOR::default();
            let psd = PSECURITY_DESCRIPTOR((&raw mut sd).cast());
            // SECURITY_DESCRIPTOR_REVISION.
            InitializeSecurityDescriptor(psd, 1)?;
            SetSecurityDescriptorDacl(psd, true, Some(&raw const acl), false)?;
            SetKernelObjectSecurity(windows::Win32::System::Threading::GetCurrentProcess(), DACL_SECURITY_INFORMATION, psd)?;
        }
    }
    // Fullscreen, the display's real pixels, however it's scaled.
    if fullscreen || aside {
        let _ = unsafe { windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }
    // ASIDE=1: the bottom right of a display other than the main one, if
    // there's one (the main one's has the taskbar and what you work in).
    let corner = aside.then(|| unsafe {
        use windows::Win32::Foundation::{LPARAM, RECT};
        use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO};
        unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> windows::core::BOOL {
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if unsafe { GetMonitorInfoW(m, &mut info) }.as_bool() {
                unsafe { &mut *(out.0 as *mut Vec<(bool, RECT)>) }.push((info.dwFlags & 1 != 0, info.rcWork));
            }
            true.into()
        }
        let mut all: Vec<(bool, RECT)> = Vec::new();
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut all as *mut _ as isize));
        all.sort_by_key(|(primary, _)| *primary);
        all.first().map(|(_, r)| *r).unwrap_or_default()
    });
    let display = unsafe {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint};
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY), &mut info);
        info.rcMonitor
    };
    let (w, h) = if fullscreen {
        (display.right - display.left, display.bottom - display.top)
    } else {
        // SIZE=WxH: the window's drawing size (800×600).
        std::env::var("SIZE").ok().and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))).unwrap_or((800, 600))
    };
    let instance = unsafe { GetModuleHandleW(None)? };
    unsafe {
        // The arrow over it, as a game shows (none would leave Windows'
        // animated busy one, a new frame for Windows' capture each turn).
        let arrow = LoadCursorW(None, IDC_ARROW)?;
        // OpenGL's windows have a device context of their own, as SDL's,
        // GLFW's and LWJGL's do: one from the shared ones could be the same
        // handle again for a window made anew.
        let style = if gl { CS_OWNDC } else { Default::default() };
        let class = WNDCLASSW { style, lpfnWndProc: Some(proc), hInstance: instance.into(), lpszClassName: w!("hook_target"), hCursor: arrow, ..Default::default() };
        RegisterClassW(&class);
        let class = WNDCLASSW { style: CS_OWNDC, lpfnWndProc: Some(proc), hInstance: instance.into(), lpszClassName: w!("hook_target_child"), hCursor: arrow, ..Default::default() };
        RegisterClassW(&class);
    }
    let make_window = || -> windows::core::Result<HWND> {
        unsafe {
            let (style, x, y, r) = if fullscreen {
                (WS_POPUP | WS_VISIBLE, display.left, display.top, RECT { left: 0, top: 0, right: w, bottom: h })
            } else {
                let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
                AdjustWindowRect(&mut r, WS_OVERLAPPEDWINDOW, false)?;
                let (x, y) = corner.map_or((100, 100), |c| (c.right - (r.right - r.left) - 24, c.bottom - (r.bottom - r.top) - 24));
                (WS_OVERLAPPEDWINDOW | WS_VISIBLE, x, y, r)
            };
            // Out of the way: never taking focus, and over other windows
            // (OpenGL doesn't draw what's covered, and the hook would copy
            // the same picture each time).
            CreateWindowExW(
                if aside { WS_EX_NOACTIVATE | WS_EX_TOPMOST } else { Default::default() },
                w!("hook_target"),
                if gl { w!("hook target (OpenGL)") } else { w!("hook target (Direct3D 11)") },
                style,
                x,
                y,
                r.right - r.left,
                r.bottom - r.top,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
    };
    // REMAKE=child: drawn in a child window filling it.
    let child = remake.as_deref() == Some("child");
    let make_surface = |window: HWND| -> windows::core::Result<HWND> {
        if !child {
            return Ok(window);
        }
        unsafe { CreateWindowExW(Default::default(), w!("hook_target_child"), w!(""), WS_CHILD | WS_VISIBLE, 0, 0, w, h, Some(window), None, Some(instance.into()), None) }
    };
    let mut hwnd = make_window()?;
    let mut surface = make_surface(hwnd)?;
    if hide_cursor.as_deref() == Some("1") {
        unsafe { ShowCursor(false) };
    }
    let hide_cursor = hide_cursor.is_some();

    let start = std::time::Instant::now();
    let cap: Option<f64> = std::env::var("FPS").ok().and_then(|s| s.parse().ok());
    let mut shown = 0u64;
    let mut late = 0u64;
    let mut drawer = Some(Drawer::new(gl, surface, w, h, novsync)?);
    let mut next_remake = remake_every;

    let mut msg = MSG::default();
    // TIMES=1: how far apart the pictures came (each drawn as soon as the
    // last was taken): a hitch shows as a long gap.
    let mut gaps = std::env::var("TIMES").is_ok_and(|v| v == "1").then(Vec::<f64>::new);
    let mut last_drawn = 0.0;
    while start.elapsed().as_secs_f64() < secs {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !IsWindow(Some(hwnd)).as_bool() {
                break;
            }
            // Back to the middle, as a game turning the mouse into camera
            // movement keeps it.
            if hide_cursor && GetForegroundWindow() == hwnd {
                let _ = SetCursorPos(display.left + w / 2, display.top + h / 2);
            }
        }
        if let Some(what) = remake.as_deref()
            && start.elapsed().as_secs_f64() >= next_remake
        {
            next_remake += remake_every;
            let old = drawer.take().unwrap();
            drawer = Some(match what {
                "context" => {
                    let mut d = old;
                    d.new_context(novsync)?;
                    d
                }
                "swapchain" => old.new_swap_chain(surface, w, h)?,
                "device" => {
                    old.close(true);
                    Drawer::new(gl, surface, w, h, novsync)?
                }
                "window" | "window-leak" => {
                    old.close(what == "window");
                    unsafe { DestroyWindow(hwnd)? };
                    hwnd = make_window()?;
                    surface = make_surface(hwnd)?;
                    Drawer::new(gl, surface, w, h, novsync)?
                }
                "child" => {
                    old.close(!gl);
                    unsafe { DestroyWindow(surface)? };
                    surface = make_surface(hwnd)?;
                    Drawer::new(gl, surface, w, h, novsync)?
                }
                other => panic!("REMAKE={other}: context, window, window-leak, child, swapchain or device"),
            });
            println!("{:.1} s: made its {what} anew", start.elapsed().as_secs_f64());
        }
        let t = match cap {
            // The bar moves 200 px a second of `t`: 4 px a picture.
            Some(fps) => {
                let due = shown as f64 / fps;
                while start.elapsed().as_secs_f64() < due {
                    std::hint::spin_loop();
                }
                shown += 1;
                shown as f32 / 50.0
            }
            None => start.elapsed().as_secs_f32(),
        };
        drawer.as_ref().unwrap().draw(if still { 0.0 } else { t }, w, h);
        if let Some(gaps) = gaps.as_mut() {
            let now = start.elapsed().as_secs_f64();
            gaps.push(now - last_drawn);
            last_drawn = now;
        }
        // How many pictures were shown late: more than half a picture after
        // their time (then the recording rightly shows them in a later frame).
        if let Some(fps) = cap {
            let due = (shown - 1) as f64 / fps;
            if start.elapsed().as_secs_f64() - due > 0.5 / fps {
                late += 1;
            }
        }
    }
    if let Some(fps) = cap {
        println!("{shown} pictures at {fps} fps, {late} shown more than half a picture late");
    }
    if let Some(mut gaps) = gaps.filter(|g| g.len() > 1) {
        gaps.remove(0);
        gaps.sort_by(f64::total_cmp);
        let at = |q: f64| gaps[((gaps.len() - 1) as f64 * q) as usize] * 1000.0;
        let long = gaps.iter().filter(|&&g| g * 1000.0 > 1.5 * at(0.5)).count();
        println!("{} pictures, ms apart: median {:.2}, 99% {:.2}, 99.9% {:.2}, longest {:.2}; {long} over 1.5× the median", gaps.len() + 1, at(0.5), at(0.99), at(0.999), at(1.0));
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
