use std::{cell::Cell, ptr, rc::Rc, thread};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uchar, c_ulong, CStr, CString};
use std::io::Cursor;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire as pw;
use pipewire::spa;
use pw::prelude::*;
use pw::types::ObjectType;
use pipewire_sys as sys;
// spa_interface_call_method! needs this
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

#[derive(Debug)]
struct Node {
    global_id: u32,
    proxy: pw::node::Node
}

unsafe impl Send for Node {}

#[derive(Debug, Copy, Clone, PartialEq)]
enum KeyType {
    HOLD,
    TOGGLE
}

#[derive(Debug, PartialEq)]
enum KeyEventType {
    PRESS,
    RELEASE
}

// keysym
#[derive(Debug)]
struct KeyEvent {
    type_: KeyEventType,
    keysym: c_ulong
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
                        let type_ = match cookie.evtype {
                            xinput2::XI_KeyPress => KeyEventType::PRESS,
                            xinput2::XI_KeyRelease => KeyEventType::RELEASE,
                            _ => panic!()
                        };

                        xlib::XFreeEventData(self.display, cookie as *mut _);
                        return KeyEvent {
                            type_,
                            keysym,
                        }
                    }
                }
                xlib::XFreeEventData(self.display, cookie as *mut _)
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
    let node_arg = Arg::new("node")
        .long("node")
        .value_names(["NODE_NAME", "KEYSYM"])
        .help("KEYSYM is x11 keysym name (the #define without \"XK_\")")
        .action(ArgAction::Append);
    let toggle_node_arg = node_arg.clone()
        .id("node-toggle")
        .long("node-toggle");
    let command = Command::new("multi_sink_source")
        .arg(node_arg)
        .arg(toggle_node_arg)
        .arg(Arg::new("release-delay")
            .long("release-delay")
            .value_name("MILLIS")
            .help("time to wait after releasing to mute")
            .default_value("0")
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

fn node_args(args: &ArgMatches, id: &str, type_: KeyType) -> Vec<(String, (KeyType, c_ulong))> {
    if let Some(iter) = args.get_occurrences::<String>(id) {
        iter.map(|mut it| (
            it.next().unwrap().clone(),
            (type_, name_to_keysym(it.next().unwrap().as_str()))
        ))
        .collect()
    } else {
        Vec::new()
    }
}

fn main() {
    unsafe {
        MUTE_POD = create_mute_pod(true);
        UNMUTE_POD = create_mute_pod(false);
    }

    let args = parse_args();
    let mut pairs = node_args(&args, "node", KeyType::HOLD);
    pairs.extend(node_args(&args, "node-toggle", KeyType::TOGGLE));
    let release_delay = args.get_one::<String>("release-delay").unwrap().parse::<u64>()
        .expect("failed to parse release-delay");

    // Initialize library and get the basic structures we need.
    pw::init();
    let mainloop = pw::MainLoop::new().expect("Failed to create PipeWire Mainloop");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");

    let nodes: Arc<Mutex<Vec<(Node, (KeyType, c_ulong))>>> = Arc::new(Mutex::new(Vec::new()));
    let nodes_clone = nodes.clone();
    let _listener_thread = thread::spawn(move || listen_for_nodes(pairs, nodes_clone));

    let listener = setup_keyboard_listener();
    let mut key_states = HashMap::<u32, bool>::new();
    loop {
        let event = listener.next_event();
        let key = event.keysym;
        let mut change = false;
        for (node, (key_type, k)) in nodes.lock().unwrap().deref() {
            if key == *k {
                if *key_type == KeyType::HOLD {
                    // unmute on press, back to mute on release
                    let mute = event.type_ == KeyEventType::RELEASE;
                    if mute && release_delay > 0 {
                        thread::sleep(Duration::from_millis(release_delay));
                    }
                    set_mute(&node.proxy, mute);
                    change = true;
                } else if event.type_ == KeyEventType::PRESS { // toggle
                    let state = key_states.entry(node.global_id).or_insert(true);
                    *state = !*state;
                    set_mute(&node.proxy, *state);
                    change = true;
                }
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

fn key_name(key: c_ulong) -> &'static str {
    unsafe {
        let raw = xlib::XKeysymToString(key);
        CStr::from_ptr(raw).to_str().unwrap_or("unknown keysym")
    }
}

fn listen_for_nodes(name_key: Vec<(String, (KeyType, c_ulong))>, out: Arc<Mutex<Vec<(Node, (KeyType, c_ulong))>>>) {
    let mainloop = pw::MainLoop::new().expect("Failed to create MainLoop for listener thread");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");
    let registry = Rc::new(core.get_registry().expect("Failed to get Registry"));

    let registry_clone = registry.clone();
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.props.is_none() { return }
            let props = global.props.as_ref().unwrap();
            if global.type_ != ObjectType::Node { return }

            if let Some(name) = props.get("node.name") {
                name_key.iter().filter(|(name_in, _)| name == *name_in).for_each(|(_, key)| {
                    let proxy = registry_clone.bind(global).unwrap();
                    let node = Node {
                        global_id: global.id,
                        proxy
                    };
                    println!("Found {name} with id {} for key {}", global.id, key_name(key.1));
                    set_mute(&node.proxy, true);
                    //dbg!(&node);
                    let mut vec = out.lock().unwrap();
                    vec.push((node, *key));
                });
            }
        })
        .register();

    mainloop.run();
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
