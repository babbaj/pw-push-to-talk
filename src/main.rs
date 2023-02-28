mod keycodes;

use std::{cell::Cell, ptr, rc::Rc};
use std::cell::{RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::io::Cursor;
use std::process::exit;
use std::ptr::NonNull;

use pipewire as pw;
use pipewire::{Core, MainLoop, Properties, spa};
use pipewire::registry::{GlobalObject, Registry};
use pipewire::spa::{Direction, ForeignDict};
use pw::prelude::*;
use pw::types::ObjectType;
use pipewire_sys as sys;
use sys::pw_node_methods;
// stupid macro
use libspa_sys as spa_sys;

use clap::{Arg, arg, ArgAction, ArgMatches, Command};
use libspa::pod::{Object, Property, PropertyFlags, Value};
use libspa::pod::serialize::PodSerializer;
use libspa_sys::{spa_debug_type_find, spa_pod_builder, spa_type_param};
use pipewire::proxy::ProxyT;
use pipewire_sys::pw_node_events;
use rdev::{listen, Event, Key};


#[derive(Debug)]
struct PwNode {
    id: u32,
    proxy: pw::node::Node
}

fn parse_args() -> ArgMatches {
    let command = Command::new("multi_sink_source")
        .arg(Arg::new("node")
            .long("node")
            .value_names(["NODE_NAME", "KEY"])
            .help("see keycodes.rs for key names")
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

    let args = parse_args();
    let pairs: Vec<(&str, Key)> = args.get_occurrences::<String>("node").unwrap()
        .map(|mut it| (
            it.next().unwrap().as_str(),
            keycodes::string_to_key(it.next().unwrap().as_str()).expect("Invalid key name")
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

    let nodes = get_nodes(&registry, &core, &mainloop, &pairs[..]);
    println!("{:?}", nodes);

    let key_node = pairs.iter().map(|(name, key)| (*key, extend_lifetime(&nodes).get(*name).unwrap()))
        .collect::<Vec<_>>();

    // moving is a bit unnecessary but rust is 1984
    let callback = move |event: Event| {
        let (key, mute) = match event.event_type {
            rdev::EventType::KeyPress(key) => (key, true),
            rdev::EventType::KeyRelease(key) => (key, false),
            _ => return
        };
        let mut change = false;
        for (k, node) in &key_node {
            if *k == key {
                set_mute(*node, mute);
                change = true;
            }
        }
        if change {
            do_roundtrip(&mainloop, &core);
        }
    };
    if let Err(error) = listen(callback) {
        panic!("{:?}", error);
    }
}

fn extend_lifetime<T>(x: &T) -> &'static T {
    unsafe { std::mem::transmute(x) }
}

// requires call to do_roundtrip
fn set_mute(node: &PwNode, mute: bool) {
    unsafe {
        let pod = if mute { &MUTE_POD } else { &UNMUTE_POD };

        let ptr: &*mut sys::pw_proxy = std::mem::transmute(node.proxy.upcast_ref());
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

fn get_nodes(registry: &Registry, core: &Core, mainloop: &MainLoop, args: &[(&str, Key)]) -> HashMap<String, PwNode> {
    let mut out = HashMap::<String, PwNode>::new();
    for_each_object(registry, core, mainloop, |global| {
        if global.props.is_none() { return false }
        let props = global.props.as_ref().unwrap();
        if global.type_ == ObjectType::Node {
            if let Some(name) = props.get("node.name") {
                if args.iter().any(|(x, _)| *x == name) {
                    let node = PwNode {
                        id: global.id,
                        proxy: registry.bind(global).unwrap()
                    };
                    out.insert(name.to_owned(), node);
                }
            }
        }
        // exit early if we found our nodes
        out.len() >= args.len()
    });

    out
}

fn for_each_object<F: FnMut(&GlobalObject<ForeignDict>) -> bool>(registry: &Registry, core: &Core, mainloop: &MainLoop, mut callback: F) {
    let mainloop_clone = mainloop.clone();
    // the listener gets removed at the end of the function
    let callback_ref: *mut () = unsafe { std::mem::transmute(&callback) };
    let reg_listener = registry
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
