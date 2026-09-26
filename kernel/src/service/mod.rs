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
use crate::mm::vmm::{
    AddressSpace, BOOT_INFO_LAYOUT, BOOT_INFO_MAGIC, BootInfo, Flags, MAX_BOOT_ARGS,
    USER_HEAP_BASE, USER_HEAP_PAGES, USER_INFO_BASE, USER_STACK_TOP,
};
use crate::obj::{Capability, DeviceObject, Object, PortRange, Rights};
use crate::sched::{self, UserEntry, task::Task};

const USER_STACK_PAGES: usize = 16;
const MAX_RESTARTS_BEFORE_BACKOFF: u32 = 3;
const MAX_SERVICES: usize = 64;

/// A capability handed to a service at start (and on every restart).
#[derive(Clone)]
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

    /// Keep an already-derived capability as a grant, so a service spawned at
    /// runtime restarts with the same slots in the same order.
    pub fn of(cap: Capability) -> Self {
        Self {
            object: cap.object,
            rights: cap.rights,
        }
    }
}

pub struct ServiceSpec {
    pub name: &'static str,
    pub image: &'static [u8],
    pub grants: Vec<Grant>,
    /// Loaded from disk at runtime rather than embedded in the kernel.
    pub dynamic: bool,
    /// Launch arguments handed to the service, kept so a restart sees the
    /// same ones. Copied into the task's boot info page.
    pub args: &'static [u8],
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
        args: b"",
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
        args: b"",
    });

    // File system + init: slot 0 = blk requests, slot 1 = spawn authority,
    // slot 2 = the file service's receiving half, slot 3 = the half it hands
    // out so a service can call the file service.
    //
    // The two halves are separate objects linked to each other, which is what
    // makes the handout work: `fs` may only receive, a client may only send, and
    // no rights have to be widened to pass authority along. `fs` is init, so the
    // manifest on disk decides which of its capabilities it may delegate.
    let (fs_serve, fs_call) = Endpoint::new_pair();
    register(ServiceSpec {
        name: "fs",
        image: FS,
        grants: alloc::vec![
            Grant::endpoint(&blk_req, Rights::SEND),
            Grant {
                object: Object::Control,
                rights: Rights::SPAWN.union(Rights::GRANT),
            },
            Grant::endpoint(&fs_serve, Rights::RECV),
            Grant::endpoint(&fs_call, Rights::SEND.union(Rights::GRANT)),
        ],
        dynamic: false,
        args: b"",
    });

    register(ServiceSpec {
        name: "pong",
        image: PONG,
        grants: alloc::vec![
            Grant::endpoint(&req, Rights::RECV),
            Grant::endpoint(&rep, Rights::SEND),
        ],
        dynamic: false,
        args: b"",
    });
    register(ServiceSpec {
        name: "ping",
        image: PING,
        grants: alloc::vec![
            Grant::endpoint(&req, Rights::SEND),
            Grant::endpoint(&rep, Rights::RECV),
        ],
        dynamic: false,
        args: b"",
    });
}

/// Register a service from an ELF image obtained at runtime and start it.
/// The image, name, capabilities and arguments are kept for the lifetime of
/// the kernel so the supervisor can restart the service with exactly the same
/// authority it was given the first time.
pub fn spawn_from_image(
    name: &str,
    image: Vec<u8>,
    grants: Vec<Grant>,
    args: &[u8],
) -> Option<sched::task::TaskId> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let name: &'static str = Box::leak(String::from(name).into_boxed_str());
    let image: &'static [u8] = Box::leak(image.into_boxed_slice());
    let args: &'static [u8] = Box::leak(args.to_vec().into_boxed_slice());
    let n_grants = grants.len();
    let idx = register(ServiceSpec {
        name,
        image,
        grants,
        dynamic: true,
        args,
    })?;
    let id = spawn(idx)?;
    klog!(
        "superv",
        "spawned '{}' from disk as task {} ({} KiB ELF, {} capability slot(s))",
        name,
        id,
        image.len() / 1024,
        n_grants
    );
    Some(id)
}

/// Build a fresh ring-3 task for service `idx` and make it runnable.
pub fn spawn(idx: usize) -> Option<sched::task::TaskId> {
    let (name, image, grants, args) = {
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
        (st.spec.name, st.spec.image, grants, st.spec.args)
    };

    // The id is reserved up front: the boot info page carries it, and the page
    // has to be in place before any CPU can enter the new task.
    let id = sched::reserve_task_id();
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

    // The service heap its runtime allocates from, and the page that tells it
    // where that heap is. Both belong to the address space, so a restart gets
    // a fresh, zeroed heap.
    asp.map_user_range(
        VirtAddr::new(USER_HEAP_BASE),
        USER_HEAP_PAGES,
        Flags::WRITABLE | Flags::NO_EXECUTE,
    )
    .ok()?;
    asp.map_user_range(
        VirtAddr::new(USER_INFO_BASE),
        1,
        Flags::WRITABLE | Flags::NO_EXECUTE,
    )
    .ok()?;
    write_boot_info(&mut asp, id, name, args)?;

    let mut t: Box<Task> = Task::new_kernel(id, name, user_task_entry, 0);
    t.addr_space = Some(asp);
    t.user = Some(UserEntry {
        rip: loaded.entry,
        rsp: USER_STACK_TOP - 16,
    });
    t.service = Some(idx);
    for cap in grants {
        t.caps.insert(cap);
    }
    sched::publish_task(t, id);
    {
        let mut s = SERVICES.lock();
        s[idx].task = Some(id);
    }
    Some(id)
}

/// Fill a new task's [`USER_INFO_BASE`] page: heap and stack bounds plus the
/// arguments its spawner attached.
fn write_boot_info(
    asp: &mut AddressSpace,
    task: sched::task::TaskId,
    name: &str,
    args: &[u8],
) -> Option<()> {
    let n = args.len().min(MAX_BOOT_ARGS);
    let head = core::mem::size_of::<BootInfo>();
    let mut page = [0u8; 4096];
    let info = BootInfo {
        magic: BOOT_INFO_MAGIC,
        layout: BOOT_INFO_LAYOUT,
        task_id: task,
        _pad: 0,
        heap_base: USER_HEAP_BASE,
        heap_size: (USER_HEAP_PAGES * 4096) as u64,
        stack_top: USER_STACK_TOP,
        arg_len: n as u64,
    };
    page[..head].copy_from_slice(unsafe {
        core::slice::from_raw_parts(&info as *const BootInfo as *const u8, head)
    });
    page[head..head + n].copy_from_slice(&args[..n]);
    page[head + n] = 0;

    let va = VirtAddr::new(USER_INFO_BASE);
    asp.write_user(va, &page[..head + n + 1]);
    if asp.translate(va).is_none() {
        klog!("superv", "WARNING: no boot info page for '{}'", name);
        return None;
    }
    Some(())
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
