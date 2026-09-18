//! Services: ring-3 programs the kernel supervises. This is the
//! "self-healing" half of the design — a crashed service is torn down
//! (address space, stack, capabilities) and re-instantiated from its image.
//!
//! A few services are embedded in the kernel image (drivers and the file
//! system); everything else is loaded from disk by `fs` through `spawn`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::VirtAddr;

use crate::arch::x86_64::{context, gdt, interrupts as irq, irq as irqobj, pci};
use crate::ipc::Endpoint;
use crate::klog;
use crate::loader;
use crate::mm::vmm::{AddressSpace, Flags, USER_STACK_TOP};
use crate::obj::{Capability, DeviceObject, Object, PortRange, Rights};
use crate::sched::{self, UserEntry, task::Task};

const USER_STACK_PAGES: usize = 16;
const MAX_RESTARTS_BEFORE_BACKOFF: u32 = 3;
const MAX_SERVICES: usize = 64;

/// A capability handed to a service at start (and on every restart).
pub struct Grant {
    pub object: Object,
    pub rights: Rights,
}

impl Grant {
    fn endpoint(ep: &Arc<Endpoint>, rights: Rights) -> Self {
        Self {
            object: Object::Endpoint(ep.clone()),
            rights,
        }
    }
}

pub struct ServiceSpec {
    pub name: &'static str,
    pub image: &'static [u8],
    pub grants: Vec<Grant>,
    /// Loaded from disk at runtime rather than embedded in the kernel.
    pub dynamic: bool,
}

pub struct ServiceState {
    pub spec: ServiceSpec,
    pub task: Option<sched::task::TaskId>,
    pub restarts: u32,
    pub last_exit: Option<i64>,
}

pub static SERVICES: Mutex<Vec<ServiceState>> = Mutex::new(Vec::new());

macro_rules! image {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".elf"))
    };
}

static PING: &[u8] = image!("ping");
static PONG: &[u8] = image!("pong");
static KBD: &[u8] = image!("kbd");
static BLK: &[u8] = image!("blk");
static FS: &[u8] = image!("fs");

pub fn register(spec: ServiceSpec) -> Option<usize> {
    let mut s = SERVICES.lock();
    if s.len() >= MAX_SERVICES {
        return None;
    }
    s.push(ServiceState {
        spec,
        task: None,
        restarts: 0,
        last_exit: None,
    });
    Some(s.len() - 1)
}

/// Register the embedded services and wire their capabilities.
pub fn init_builtin() {
    let req = Endpoint::new();
    let rep = Endpoint::new();
    let blk_req = Endpoint::new();

    // Keyboard driver: slot 0 = ISA IRQ 1, slot 1 = the 8042 ports 0x60..0x64.
    register(ServiceSpec {
        name: "kbd",
        image: KBD,
        grants: alloc::vec![
            Grant {
                object: Object::Irq(irqobj::isa(1)),
                rights: Rights::RECV,
            },
            Grant {
                object: Object::Port(PortRange { base: 0x60, len: 5 }),
                rights: Rights::MAP_READ,
            },
        ],
        dynamic: false,
    });

    // Storage driver: slot 0 = the first NVMe controller, slot 1 = request queue.
    let mut blk_grants = Vec::new();
    if let Some(nvme) = pci::find(0x01, 0x08) {
        pci::enable(&nvme);
        blk_grants.push(Grant {
            object: Object::Device(Arc::new(DeviceObject { pci: nvme })),
            rights: Rights::MAP_READ.union(Rights::MAP_WRITE),
        });
        klog!(
            "superv",
            "nvme {:02x}:{:02x}.{} handed to service 'blk'",
            nvme.bus,
            nvme.slot,
            nvme.func
        );
    } else {
        klog!("superv", "no NVMe controller found; 'blk' will exit");
    }
    blk_grants.push(Grant::endpoint(&blk_req, Rights::RECV));
    register(ServiceSpec {
        name: "blk",
        image: BLK,
        grants: blk_grants,
        dynamic: false,
    });

    // File system + init: slot 0 = blk requests, slot 1 = spawn authority.
    register(ServiceSpec {
        name: "fs",
        image: FS,
        grants: alloc::vec![
            Grant::endpoint(&blk_req, Rights::SEND),
            Grant {
                object: Object::Control,
                rights: Rights::SPAWN,
            },
        ],
        dynamic: false,
    });

    register(ServiceSpec {
        name: "pong",
        image: PONG,
        grants: alloc::vec![
            Grant::endpoint(&req, Rights::RECV),
            Grant::endpoint(&rep, Rights::SEND),
        ],
        dynamic: false,
    });
    register(ServiceSpec {
        name: "ping",
        image: PING,
        grants: alloc::vec![
            Grant::endpoint(&req, Rights::SEND),
            Grant::endpoint(&rep, Rights::RECV),
        ],
        dynamic: false,
    });
}

/// Register a service from an ELF image obtained at runtime and start it.
/// The image and name are kept for the lifetime of the kernel so the
/// supervisor can restart the service.
pub fn spawn_from_image(name: &str, image: Vec<u8>) -> Option<sched::task::TaskId> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let name: &'static str = Box::leak(String::from(name).into_boxed_str());
    let image: &'static [u8] = Box::leak(image.into_boxed_slice());
    let idx = register(ServiceSpec {
        name,
        image,
        grants: Vec::new(),
        dynamic: true,
    })?;
    let id = spawn(idx)?;
    klog!(
        "superv",
        "spawned '{}' from disk as task {} ({} KiB ELF)",
        name,
        id,
        image.len() / 1024
    );
    Some(id)
}

/// Build a fresh ring-3 task for service `idx` and make it runnable.
pub fn spawn(idx: usize) -> Option<sched::task::TaskId> {
    let (name, image, grants) = {
        let s = SERVICES.lock();
        let st = s.get(idx)?;
        let grants: Vec<Capability> = st
            .spec
            .grants
            .iter()
            .map(|g| Capability {
                object: g.object.clone(),
                rights: g.rights,
            })
            .collect();
        (st.spec.name, st.spec.image, grants)
    };

    let mut asp = AddressSpace::new()?;
    let loaded = match loader::load(&mut asp, image) {
        Ok(l) => l,
        Err(e) => {
            klog!(
                "superv",
                "service '{}': cannot load ELF image: {:?}",
                name,
                e
            );
            return None;
        }
    };
    let stack_bottom = USER_STACK_TOP - (USER_STACK_PAGES as u64) * 4096;
    asp.map_user_range(
        VirtAddr::new(stack_bottom),
        USER_STACK_PAGES,
        Flags::WRITABLE | Flags::NO_EXECUTE,
    )
    .ok()?;

    let mut t: Box<Task> = Task::new_kernel(0, name, user_task_entry, 0);
    t.addr_space = Some(asp);
    t.user = Some(UserEntry {
        rip: loaded.entry,
        rsp: USER_STACK_TOP - 16,
    });
    t.service = Some(idx);
    for cap in grants {
        t.caps.insert(cap);
    }
    let id = sched::add_task(t);
    SERVICES.lock()[idx].task = Some(id);
    Some(id)
}

extern "C" fn user_task_entry(_: u64) {
    let entry = sched::with_current(|t| t.user.expect("user task without entry"));
    let sel = gdt::selectors();
    unsafe {
        context::enter_user(
            entry.rip,
            entry.rsp,
            sel.user_code.0 as u64,
            sel.user_data.0 as u64,
        )
    }
}

pub fn start_all() {
    let n = SERVICES.lock().len();
    for i in 0..n {
        let (name, size) = {
            let s = SERVICES.lock();
            (s[i].spec.name, s[i].spec.image.len())
        };
        match spawn(i) {
            Some(id) => klog!(
                "superv",
                "started service '{}' as task {} ({} KiB ELF)",
                name,
                id,
                size / 1024
            ),
            None => klog!("superv", "failed to start service '{}'", name),
        }
    }
}

/// The supervisor thread: reaps dead tasks and restarts services that
/// crashed or failed. A clean `exit(0)` means the service is done.
pub extern "C" fn supervisor_main(_: u64) {
    klog!(
        "superv",
        "supervisor running as task {}",
        sched::current_id()
    );
    loop {
        while let Some(dead) = sched::take_dead() {
            let name = dead.name;
            let code = dead.exit_code.unwrap_or(0);
            let svc = dead.service;
            drop(dead);

            let Some(idx) = svc else {
                klog!("superv", "reaped kernel thread '{}' (exit {})", name, code);
                continue;
            };

            if code == 0 {
                let mut s = SERVICES.lock();
                s[idx].task = None;
                s[idx].last_exit = Some(0);
                klog!(
                    "superv",
                    "service '{}' finished cleanly, not restarting",
                    name
                );
                continue;
            }

            let (restarts, delay_ms) = {
                let mut s = SERVICES.lock();
                let st = &mut s[idx];
                st.task = None;
                st.last_exit = Some(code);
                st.restarts += 1;
                let backoff = if st.restarts > MAX_RESTARTS_BEFORE_BACKOFF {
                    (200u64 * (st.restarts - MAX_RESTARTS_BEFORE_BACKOFF) as u64).min(3000)
                } else {
                    0
                };
                (st.restarts, backoff)
            };
            let reason = if code < 0 { "crashed" } else { "failed" };
            klog!(
                "superv",
                "service '{}' {} (code {}) -> restarting (restart #{}{})",
                name,
                reason,
                code,
                restarts,
                if delay_ms > 0 {
                    alloc::format!(", backoff {} ms", delay_ms)
                } else {
                    String::new()
                }
            );
            if delay_ms > 0 {
                sched::sleep_ms(delay_ms);
            }
            match spawn(idx) {
                Some(id) => klog!(
                    "superv",
                    "service '{}' back up as task {} at {} ms",
                    name,
                    id,
                    irq::uptime_ms()
                ),
                None => klog!("superv", "service '{}' failed to respawn", name),
            }
        }
        sched::sleep_ms(50);
    }
}

pub fn restarts_of(name: &str) -> u32 {
    SERVICES
        .lock()
        .iter()
        .find(|s| s.spec.name == name)
        .map(|s| s.restarts)
        .unwrap_or(0)
}

pub fn dump() {
    let s = SERVICES.lock();
    for st in s.iter() {
        klog!(
            "superv",
            "  {:<8} task={:<4} restarts={} last_exit={:?}{}",
            st.spec.name,
            st.task.map(|t| t as i64).unwrap_or(-1),
            st.restarts,
            st.last_exit,
            if st.spec.dynamic { " (from disk)" } else { "" }
        );
    }
}
