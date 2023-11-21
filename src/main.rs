use std::{cell::Cell, rc::Rc, thread};
use std::collections::{HashMap};
use std::ffi::{c_char, c_int, c_uchar, c_ulong, CStr, CString};
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use pipewire as pw;
use pipewire::{Core, MainLoop, spa};
use pw::prelude::*;
use pw::types::ObjectType;
use pipewire_sys as sys;
// spa_interface_call_method! needs this
use libspa_sys as spa_sys;

use clap::{Arg, ArgAction, ArgMatches, Command};
use libspa::pod::{Object, Property, PropertyFlags, Value};
use libspa::pod::serialize::PodSerializer;
use pipewire::proxy::ProxyT;


use evdev::{Device, enumerate, InputEventKind, Key};


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

fn parse_args() -> ArgMatches {
    let node_arg = Arg::new("node")
        .long("node")
        .value_names(["NODE_NAME", "EVENT_CODE"])
        .help("EVENT_CODE is the linux #define from input-event-codes.h")
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

fn node_args(args: &ArgMatches, id: &str, type_: KeyType) -> Vec<(String, (KeyType, Key))> {
    if let Some(iter) = args.get_occurrences::<String>(id) {
        iter.map(|mut it| (
            it.next().unwrap().clone(),
            (type_, Key::from_str(it.next().unwrap().as_str()).unwrap())
        ))
        .collect()
    } else {
        Vec::new()
    }
}

fn supports_keys(dev: &evdev::Device) -> bool {
    dev.supported_keys()
        .map(|attrs| attrs.contains(evdev::Key(evdev::Key::KEY_F1.code())))
        .unwrap_or(false)
}

fn get_keyboards() -> Vec<(PathBuf, Device)> {
    return evdev::enumerate()
        .filter(|(_, dev)| supports_keys(dev))
        .collect()
}

fn event_loop(mut dev: Device, path: PathBuf, release_delay: u64, nodes: Arc<Mutex<Vec<(Node, (KeyType, Key))>>>) {
    let mainloop = pw::MainLoop::new().expect("Failed to create PipeWire Mainloop");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");
    let dev_name = String::from(dev.name().unwrap_or(dev.physical_path().unwrap_or("unknown name")));

    let mut key_states = HashMap::<u32, bool>::new();
    loop {
        let events = dev.fetch_events();
        if let Err(err) = &events {
            if let Some(libc::ENODEV) = err.raw_os_error() {
                // device was removed
                return;
            }
            panic!("Unexpected error fetching events from \"{}\"({}), {}", dev_name, path.display(), err);
        }
        for event in events.unwrap() {
            if let InputEventKind::Key(event_key) = event.kind() {
                dbg!(event.value(), event_key);
                let mut change = false;
                for (node, (key_type, k)) in nodes.lock().unwrap().deref() {
                    if event_key == *k {
                        if *key_type == KeyType::HOLD {
                            let mute = match event.value() {
                                0 => true, // release
                                1 => false, // down
                                _ => continue
                            };
                            if mute && release_delay > 0 {
                                thread::sleep(Duration::from_millis(release_delay));
                            }
                            set_mute(&node.proxy, mute);
                            change = true;
                        } else if event.value() == 1 { // toggle and key down
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

    let nodes: Arc<Mutex<Vec<(Node, (KeyType, Key))>>> = Arc::new(Mutex::new(Vec::new()));
    let nodes_clone = nodes.clone();
    let _listener_thread = thread::spawn(move || listen_for_nodes(pairs, nodes_clone));


    let mut threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    for (path, mut dev) in evdev::enumerate() {
        if !supports_keys(&mut dev) {
            continue;
        }
        println!("{} {}", path.display(), dev.physical_path().unwrap());
        let nodes2 = nodes.clone();
        threads.lock().unwrap().push(thread::spawn(move || {
            event_loop(dev, path, release_delay, nodes2);
        }));
    }
    loop {
        let mut guard = threads.lock().unwrap();
        let handle = guard.pop();
        if let Some(h) = handle {
            drop(guard);
            h.join().unwrap();
        } else {
            return;
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

fn listen_for_nodes(name_key: Vec<(String, (KeyType, Key))>, out: Arc<Mutex<Vec<(Node, (KeyType, Key))>>>) {
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
                    println!("Found {name} with id {} for key {:?}", global.id, key.1);
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
