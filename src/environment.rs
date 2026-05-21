mod nullable_box;
use std::time::Instant;

use crate::{bootstrap, cpu, environment::nullable_box::NullableBox, gdb, mem, options, syscall};

/// The struct containing the entire emulator state. Methods are provided for
/// execution and management of threads.
pub struct Environment {
    /// Reference point for various timing functions.
    pub startup_time: Instant,
    pub mem: NullableBox<mem::Mem>,
    pub cpu: NullableBox<Box<dyn cpu::CpuBackend>>,
    pub syscall: NullableBox<syscall::Syscall>,
    pub bootstrap: Option<bootstrap::Bootstrap>,
    remaining_ticks: Option<u64>,
    gdb_server: Option<Box<gdb::GdbServer>>,
}

/// What to do next.
enum NextAction {
    /// Continue CPU emulation.
    Continue,
    /// Return to host.
    ReturnToHost,
    /// Debug the current CPU error.
    DebugCpuError(cpu::CpuError),
}

impl Environment {
    /// Loads the binary and sets up the emulator.
    pub fn new(mut options: options::Options) -> Result<Environment, String> {
        let startup_time = Instant::now();

        let mut mem = mem::Mem::new();

        let cpu: Box<dyn cpu::CpuBackend> = Box::new(cpu::UnicornCpu::new(match options.direct_memory_access {
            true => Some(&mut mem),
            false => None,
        }));

        let syscall = syscall::Syscall::new();

        let mut env = Environment {
            startup_time,
            mem: NullableBox::new(mem),
            cpu: NullableBox::new(cpu),
            syscall: NullableBox::new(syscall),
            bootstrap: None,
            gdb_server: None,
            remaining_ticks: None,
        };

        env.cpu.set_cpsr(cpu::Cpu::CPSR_USER_MODE);

        Ok(env)
    }

    /// Run the emulator. This is the main loop and won't return until app exit.
    /// Only `main.rs` should call this.
    pub fn run(mut self) {
        loop {}
    }

    pub fn start(&mut self) -> Result<(), String> {
        let bootstrap = bootstrap::Bootstrap::start()
            .map_err(|code| format!("bootstrap start failed with code {code}"))?;
        self.bootstrap = Some(bootstrap);
        Ok(())
    }

    pub fn event(&mut self, code: i32, p1: i32, p2: i32) -> i32 {
        self.bootstrap
            .as_mut()
            .map_or(-1, |bootstrap| bootstrap.event(code, p1, p2))
    }

    pub fn timer(&mut self) -> i32 {
        self.bootstrap
            .as_mut()
            .map_or(-1, bootstrap::Bootstrap::timer)
    }

    /// Run the emulator until the app returns control to the host. This is for
    /// host-to-guest function calls (see [abi::CallFromHost::call_from_host]).
    ///
    /// Note that this might execute code from other threads while waiting for
    /// the app to return control on the original thread!
    pub fn run_call(&mut self) {
        self.run_inner();
    }

    #[cold]
    /// Let the debugger handle a CPU error, or panic if there's no debugger
    /// connected. Returns [true] if the CPU should step and then resume
    /// debugging, or [false] if it should resume normal execution.
    fn debug_cpu_error(&mut self, error: cpu::CpuError) {
        if matches!(error, cpu::CpuError::UndefinedInstruction)
            || matches!(error, cpu::CpuError::Breakpoint)
        {
            // Rewind the PC so that it's at the instruction where the error
            // occurred, rather than the next instruction. This is necessary for
            // GDB to detect its software breakpoints. For some reason this
            // isn't correct for memory errors however.
            let instruction_len = if (self.cpu.cpsr() & cpu::Cpu::CPSR_THUMB) != 0 {
                2
            } else {
                4
            };
            self.cpu.regs_mut()[cpu::Cpu::PC] -= instruction_len;
        }

        if self.gdb_server.is_none() {
            panic!("Error during CPU execution: {error:?}");
        }

        echo!("Debuggable error during CPU execution: {:?}.", error);
        self.enter_debugger(Some(error))
    }

    /// Suspend execution and hand control to the connected debugger.
    /// You should precede this call with a log message that explains why the
    /// debugger is being invoked. The return value is the same as
    /// [gdb::GdbServer::wait_for_debugger]'s.
    ///
    /// Note that this also yields the thread - take care!
    pub fn enter_debugger(&mut self, reason: Option<cpu::CpuError>) {
        // GDB doesn't seem to manage to produce a useful stack trace, so
        // let's print our own.
        // self.stack_trace_current();

        // self.yield_thread(ThreadBlock::WaitingForDebugger(reason));
    }

    // pub fn stack_for_longjmp(&self, mut lr: u32, fp: u32) -> Vec<u32> {
    //     let stack_range = self.threads[self.current_thread].stack.clone().unwrap();
    //     let mut frames = Vec::new();
    //     let mut fp: mem::ConstPtr<u8> = mem::Ptr::from_bits(fp);
    //     let return_to_host_routine_addr = self.syscall.return_to_host_routine().addr_with_thumb_bit();
    //     while stack_range.contains(&fp.to_bits()) && lr != return_to_host_routine_addr {
    //         frames.push(lr);
    //         lr = self.mem.read((fp + 4).cast());
    //         fp = self.mem.read(fp.cast());
    //     }
    //     frames
    // }

    // fn dump_all_regs(&self) {
    //     echo_no_panic!(
    //         "Dumping registers for current thread (#{})",
    //         self.current_thread
    //     );
    //     self.cpu.dump_regs();
    //     for (tid, thread) in self.threads.iter().enumerate() {
    //         if thread.active && tid != self.current_thread {
    //             echo_no_panic!("Dumping registers for thread #{}", tid);
    //             let Some(ctx) = thread.guest_context.as_ref() else {
    //                 echo_no_panic!("Could not get registers for thread {}!", tid);
    //                 return;
    //             };
    //             cpu::Cpu::echo_regs(&ctx.regs);
    //         }
    //     }
    // }

    // fn stack_trace_current(&self) {
    //     if self.current_thread == 0 {
    //         echo_no_panic!("Attempting to produce stack trace for main thread:");
    //     } else {
    //         echo_no_panic!(
    //             "Attempting to produce stack trace for thread {}:",
    //             self.current_thread
    //         );
    //     }
    //     self.stack_trace_for_thread(self.current_thread);
    // }

    #[inline(always)]
    /// Respond to the new CPU state (do nothing, execute an SVC or enter
    /// debugging) and decide what to do next.
    fn handle_cpu_state(&mut self, state: cpu::CpuState) -> NextAction {
        match state {
            cpu::CpuState::Normal => NextAction::Continue,
            cpu::CpuState::Svc(svc) => {
                // The program counter is pointing at the
                // instruction after the SVC, but we want the
                // address of the SVC itself.
                let svc_pc = self.cpu.regs()[cpu::Cpu::PC] - 4;
                match svc {
                    syscall::Syscall::SVC_RETURN_TO_HOST => {
                        assert!(
                            svc_pc
                                == self
                                    .syscall
                                    .return_to_host_routine()
                                    .addr_without_thumb_bit()
                        );
                        // Normal return from host-to-guest call.
                        NextAction::ReturnToHost
                    }
                    syscall::Syscall::SVC_THREAD_EXIT => {
                        unimplemented!("TODO: implement exit routines for threads!")
                    }
                    syscall::Syscall::SVC_LINKED_FUNCTIONS_BASE.. => {
                        self.cpu.regs_mut()[cpu::Cpu::PC] = svc_pc;
                        NextAction::Continue
                    }
                }
            }
            cpu::CpuState::Error(e) => NextAction::DebugCpuError(e),
        }
    }

    fn run_inner(&mut self) {
        loop {
            while self
                .remaining_ticks
                .is_none_or(|remaining_ticks| remaining_ticks > 0)
            {
                let state = self
                    .cpu
                    .run_or_step(&mut self.mem, self.remaining_ticks.as_mut());

                match self.handle_cpu_state(state) {
                    NextAction::Continue => {}
                    NextAction::ReturnToHost => return,
                    NextAction::DebugCpuError(e) => {
                        self.debug_cpu_error(e);
                    }
                }
                if self.remaining_ticks.is_none() {
                    break;
                }
            }
        }
    }
}
