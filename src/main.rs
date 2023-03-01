extern crate core;

use std::{cell::Cell, ptr, rc::Rc};
use std::ffi::{c_char, c_int, c_uchar, c_ulong, CString};
use std::io::Cursor;

use pipewire as pw;
use pipewire::{Core, MainLoop, spa};
use pipewire::registry::{GlobalObject, Registry};
use pipewire::spa::{ForeignDict};
use pw::prelude::*;
use pw::types::ObjectType;
use pipewire_sys as sys;
// stupid macro
use libspa_sys as spa_sys;

use clap::{Arg, ArgAction, ArgMatches, Command};
use libspa::pod::{Object, Property, PropertyFlags, Value};
use libspa::pod::serialize::PodSerializer;
use pipewire::proxy::ProxyT;

use x11::xlib;
use x11::xinput2;

struct KeyboardListener {
    display: *mut xlib::Display,
    window: c_ulong,
    xi_opcode: c_int
}

// keysym
enum KeyEvent {
    Press(c_ulong),
    Release(c_ulong)
}

impl KeyboardListener {
    fn next_event(&self) -> KeyEvent {
        unsafe {
            loop {
                let mut event: xlib::XEvent = std::mem::zeroed();
                xlib::XNextEvent(self.display, &mut event as *mut _);
                let cookie = &mut event.generic_event_cookie;

                if xlib::XGetEventData(self.display, cookie as *mut _) != 0
                    && cookie.type_ == xlib::GenericEvent
                    && cookie.extension == self.xi_opcode
                {
                    // should always be true
                    if cookie.evtype == xinput2::XI_KeyPress || cookie.evtype == xinput2::XI_KeyRelease {
                        let event = &*(cookie.data as *const xinput2::XIDeviceEvent);
                        let repeat = event.flags & xinput2::XIKeyRepeat != 0;
                        if repeat { continue }
                        let keycode = event.detail;

                        let keysym = xlib::XKeycodeToKeysym(self.display, keycode as c_uchar, 0);

                        return if cookie.evtype == xinput2::XI_KeyPress {
                            KeyEvent::Press(keysym)
                        } else {
                            KeyEvent::Release(keysym)
                        }
                    }

                    xlib::XFreeEventData(self.display, cookie as *mut _)
                }
            }
        }
    }
}

impl Drop for KeyboardListener {
    fn drop(&mut self) {
        unsafe {
            xlib::XDestroyWindow(self.display, self.window);
            xlib::XCloseDisplay(self.display);
        }
    }
}


fn setup_keyboard_listener() -> KeyboardListener {
    unsafe {
        let display = xlib::XOpenDisplay(ptr::null());
        // https://github.com/freedesktop/xorg-xinput/blob/8cebd89a644545c91a3d1c146977fe023798ee2a/src/xinput.c#L415
        let mut xi_opcode: c_int = 0;
        // don't need these
        let mut _event: c_int = 0;
        let mut _error: c_int = 0;
        if xlib::XQueryExtension(display, "XInputExtension\0".as_ptr() as *const c_char, &mut xi_opcode as *mut _, &mut _event as *mut _, &mut _error as *mut _) == 0 {
            panic!("X Input extension not available")
        }

        let win = xlib::XDefaultRootWindow(display);
        let mask_len = (xinput2::XI_LASTEVENT >> 3) + 1;
        let mut mask_buf = vec![c_uchar::default(); mask_len as usize];
        let mut mask = xinput2::XIEventMask {
            deviceid: xinput2::XIAllDevices,
            // https://github.com/freedesktop/xorg-xinput/blob/master/src/test_xi2.c#L377
            // https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/blob/master/include/X11/extensions/XI2.h#L184
            mask_len: (xinput2::XI_LASTEVENT >> 3) + 1,
            mask: mask_buf.as_mut_ptr(),
        };
        xinput2::XISetMask(mask_buf.as_mut_slice(), xinput2::XI_KeyPress);
        xinput2::XISetMask(mask_buf.as_mut_slice(), xinput2::XI_KeyRelease);

        xinput2::XISelectEvents(display, win, &mut mask as *mut _, 1);
        xlib::XSync(display, 0);

        KeyboardListener {
            display,
            window: win,
            xi_opcode
        }
    }
}

fn name_to_keysym(name: &str) -> c_ulong {
    let cstr = CString::new(name).unwrap();
    let keysym = unsafe { xlib::XStringToKeysym(cstr.as_ptr()) };
    if keysym == 0 {
        panic!("\"{name}\" is not a valid keysym name");
    }
    keysym
}

fn parse_args() -> ArgMatches {
    let command = Command::new("multi_sink_source")
        .arg(Arg::new("node")
            .long("node")
            .value_names(["NODE_NAME", "KEYSYM"])
            .help("KEYSYM is x11 keysym name (the #define without \"XK_\")")
            .required(true)
            .action(ArgAction::Append)
        );

    command.get_matches()
}

fn create_mute_pod(mute: bool) -> Vec<u8> {
    let vec_rs: Vec<u8> = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(Object{
        type_: spa_sys::SPA_TYPE_OBJECT_Props,
        id: spa_sys::SPA_PARAM_Props,
        properties: vec! [
            Property {
                key: spa_sys::SPA_PROP_mute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(mute)
            }
        ],
    }))
    .unwrap()
    .0
    .into_inner();

    vec_rs
}

static mut MUTE_POD: Vec<u8> = Vec::new();
static mut UNMUTE_POD: Vec<u8> = Vec::new();

fn main() {
    unsafe {
        MUTE_POD = create_mute_pod(true);
        UNMUTE_POD = create_mute_pod(false);
    }
    setup_keyboard_listener();

    let args = parse_args();
    let pairs: Vec<(&str, c_ulong)> = args.get_occurrences::<String>("node").unwrap()
        .map(|mut it| (
            it.next().unwrap().as_str(),
            name_to_keysym(it.next().unwrap().as_str())
        ))
        .collect();

    for s in &pairs {
        println!("{:?}", s);
    }
    // Initialize library and get the basic structures we need.
    pw::init();
    let mainloop = pw::MainLoop::new().expect("Failed to create PipeWire Mainloop");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");
    let registry = core.get_registry().expect("Failed to get Registry");

    let node_key = get_nodes(&registry, &core, &mainloop, &pairs[..]);
    println!("{:?}", node_key);

    let listener = setup_keyboard_listener();
    loop {
        let event = listener.next_event();
        let (key, mute) = match event {
            KeyEvent::Press(key) => (key, true),
            KeyEvent::Release(key) => (key, false)
        };
        let mut change = false;
        for (node, k) in &node_key {
            if *k == key {
                set_mute(node, mute);
                change = true;
            }
        }
        if change {
            do_roundtrip(&mainloop, &core);
        }
    }
}

// requires call to do_roundtrip
fn set_mute(node: &pw::node::Node, mute: bool) {
    unsafe {
        let pod = if mute { &MUTE_POD } else { &UNMUTE_POD };

        let ptr: &*mut sys::pw_proxy = std::mem::transmute(node.upcast_ref());
        spa::spa_interface_call_method!(
            *ptr,
            sys::pw_node_methods,
            set_param,
            spa_sys::SPA_PARAM_Props,
            0,
            pod.as_ptr() as *const spa_sys::spa_pod
        );
    }
}

fn get_nodes(registry: &Registry, core: &Core, mainloop: &MainLoop, name_key: &[(&str, c_ulong)]) -> Vec<(pw::node::Node, c_ulong)> {
    let mut out = Vec::new();
    for_each_object(registry, core, mainloop, |global| {
        if global.props.is_none() { return false }
        let props = global.props.as_ref().unwrap();
        if global.type_ == ObjectType::Node {
            if let Some(name) = props.get("node.name") {
                if let Some((_, key)) = name_key.iter().find(|(name_in, _)| name == *name_in) {
                    let proxy = registry.bind(global).unwrap();
                    out.push((proxy, *key))
                }
            }
        }
        // exit early if we found our nodes
        out.len() >= name_key.len()
    });

    out
}

fn for_each_object<F: FnMut(&GlobalObject<ForeignDict>) -> bool>(registry: &Registry, core: &Core, mainloop: &MainLoop, callback: F) {
    let mainloop_clone = mainloop.clone();
    // the listener gets removed at the end of the function
    let callback_ref: *mut () = unsafe { std::mem::transmute(&callback) };
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| unsafe {
            let troll: &mut F = std::mem::transmute(callback_ref);
            if (*troll)(global) {
                mainloop_clone.quit();
            }
        })
        .register();

    do_roundtrip(&mainloop, &core);
}

/// Do a single roundtrip to process all events.
/// See the example in roundtrip.rs for more details on this.
fn do_roundtrip(mainloop: &pw::MainLoop, core: &pw::Core) {
    let done = Rc::new(Cell::new(false));
    let done_clone = done.clone();
    let loop_clone = mainloop.clone();

    // Trigger the sync event. The server's answer won't be processed until we start the main loop,
    // so we can safely do this before setting up a callback. This lets us avoid using a Cell.
    let pending = core.sync(0).expect("sync failed");

    let _listener_core = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::PW_ID_CORE && seq == pending {
                done_clone.set(true);
                loop_clone.quit();
            }
        })
        .register();

    while !done.get() {
        mainloop.run();
    }
}
