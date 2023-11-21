use std::{cell::Cell, rc::Rc, thread};
use std::collections::{HashMap};
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::{Deref};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire as pw;
use pw::prelude::*;
use pw::types::ObjectType;
use pipewire_sys as sys;
// spa_interface_call_method! needs this
use libspa_sys as spa_sys;

use clap::{Arg, ArgAction, ArgMatches, Command};
use libspa::pod::{Object, Property, PropertyFlags, Value};
use libspa::pod::serialize::PodSerializer;
use pipewire::proxy::ProxyT;

use evdev::Key;

use input::{Event, Libinput, LibinputInterface};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::{fs::OpenOptionsExt, io::OwnedFd};
use std::path::Path;
use input::event::keyboard::{KeyboardEventTrait, KeyState};
use nix::poll::{poll, PollFlags, PollFd};

extern crate libc;
use libc::{O_RDONLY, O_RDWR, O_WRONLY};

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

fn event_loop(mut input: Libinput, release_delay: u64, nodes: Arc<Mutex<Vec<(Node, (KeyType, Key))>>>) {
    let mainloop = pw::MainLoop::new().expect("Failed to create PipeWire Mainloop");
    let context = pw::Context::new(&mainloop).expect("Failed to create PipeWire Context");
    let core = context
        .connect(None)
        .expect("Failed to connect to PipeWire Core");

    let mut key_states = HashMap::<u32, bool>::new();
    // example code broke
    let fd = unsafe { BorrowedFd::borrow_raw(input.as_raw_fd()) };
    let pollfd = PollFd::new(&fd, PollFlags::POLLIN);
    while poll(&mut [pollfd], -1).is_ok() {
        input.dispatch().unwrap();
        for event in &mut input {
            if let Event::Keyboard(kb_event) = event {
                let state = kb_event.key_state();
                let event_key = Key::new(kb_event.key() as u16);

                let mut change = false;
                for (node, (key_type, k)) in nodes.lock().unwrap().deref() {
                    if event_key == *k {
                        if *key_type == KeyType::HOLD {
                            let mute = state == KeyState::Released;
                            if mute && release_delay > 0 {
                                thread::sleep(Duration::from_millis(release_delay));
                            }
                            set_mute(&node.proxy, mute);
                            change = true;
                        } else if state == KeyState::Pressed { // toggle and key down
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

struct Interface;

// pasted example code
impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_RDONLY != 0) | (flags & O_RDWR != 0))
            .write((flags & O_WRONLY != 0) | (flags & O_RDWR != 0))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
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

    let nodes2 = nodes.clone();
    let mut input = Libinput::new_with_udev(Interface);
    input.udev_assign_seat("seat0").unwrap();
    event_loop(input, release_delay, nodes2);
}

// requires call to do_roundtrip
fn set_mute(node: &pw::node::Node, mute: bool) {
    unsafe {
        let pod = if mute { &MUTE_POD } else { &UNMUTE_POD };

        let ptr: &*mut sys::pw_proxy = std::mem::transmute(node.upcast_ref());
        pw::spa::spa_interface_call_method!(
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
    let vec_copy = out.clone();
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
        .global_remove(move |id| {
            vec_copy.lock().unwrap().retain(|(node, _)| node.global_id != id);
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
