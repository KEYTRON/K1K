use alloc::boxed::Box;
use alloc::vec;

use crate::arch::x86_64::context;
use crate::mm::vmm::AddressSpace;
use crate::obj::CapTable;

pub type TaskId = u32;

pub const KSTACK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    Running,
    Blocked,
    Sleeping,
    Dead,
}

#[derive(Clone, Copy, Debug)]
pub struct UserEntry {
    pub rip: u64,
    pub rsp: u64,
}

pub struct Task {
    pub id: TaskId,
    pub name: &'static str,
    pub state: State,
    pub kstack: Box<[u8]>,
    pub ctx_sp: u64,
    pub addr_space: Option<AddressSpace>,
    pub user: Option<UserEntry>,
    pub caps: CapTable,
    pub sleep_until: u64,
    pub exit_code: Option<i64>,
    pub quantum_left: u32,
    /// Message slot for IPC: filled by a sender while we are blocked in recv.
    pub ipc_inbox: Option<crate::ipc::Message>,
    /// Which service (if any) this task instantiates — for supervision.
    pub service: Option<usize>,
    /// CPU whose stack this task is (or was until `finish_switch`) running on.
    pub on_cpu: Option<u32>,
    /// A CPU's idle task: never queued, run only when nothing else is ready.
    pub is_idle: bool,
}

impl Task {
    pub fn new_kernel(
        id: TaskId,
        name: &'static str,
        entry: extern "C" fn(u64),
        arg: u64,
    ) -> Box<Self> {
        let kstack = vec![0u8; KSTACK_SIZE].into_boxed_slice();
        let top = kstack.as_ptr() as u64 + KSTACK_SIZE as u64;
        let ctx_sp = unsafe { context::init_stack(top, entry as usize as u64, arg) };
        Box::new(Self {
            id,
            name,
            state: State::Ready,
            kstack,
            ctx_sp,
            addr_space: None,
            user: None,
            caps: CapTable::new(),
            sleep_until: 0,
            exit_code: None,
            quantum_left: 0,
            ipc_inbox: None,
            service: None,
            on_cpu: None,
            is_idle: false,
        })
    }

    /// A CPU's boot context: no owned stack, already running.
    pub fn boot(id: TaskId) -> Box<Self> {
        Box::new(Self {
            id,
            name: "idle",
            state: State::Running,
            kstack: Box::new([]),
            ctx_sp: 0,
            addr_space: None,
            user: None,
            caps: CapTable::new(),
            sleep_until: 0,
            exit_code: None,
            quantum_left: 0,
            ipc_inbox: None,
            service: None,
            on_cpu: None,
            is_idle: true,
        })
    }

    pub fn kstack_top(&self) -> u64 {
        if self.kstack.is_empty() {
            0
        } else {
            self.kstack.as_ptr() as u64 + self.kstack.len() as u64
        }
    }

    pub fn is_user(&self) -> bool {
        self.user.is_some()
    }
}
