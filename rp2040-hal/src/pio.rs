//! Programmable IO (PIO)
/// See [Chapter 3](https://rptl.io/pico-datasheet) for more details.

const PIO_INSTRUCTION_COUNT: usize = 32;

/// PIO Instance
pub trait Instance: core::ops::Deref<Target = rp2040_pac::pio0::RegisterBlock> {}

/// Programmable IO Block
pub struct PIO<P: Instance> {
    used_instruction_space: core::cell::Cell<u32>, // bit for each PIO_INSTRUCTION_COUNT
    pio: P,
    state_machines: [StateMachine<P>; 4],
}

impl<P: Instance> core::fmt::Debug for PIO<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("PIO")
            .field("used_instruction_space", &self.used_instruction_space)
            .field("pio", &"PIO { .. }")
            .finish()
    }
}

impl<P: Instance> PIO<P> {
    /// Create a new PIO wrapper.
    pub fn new(pio: P) -> Self {
        PIO {
            used_instruction_space: core::cell::Cell::new(0),
            state_machines: [
                StateMachine {
                    id: 0,
                    block: pio.deref(),
                    _phantom: core::marker::PhantomData,
                },
                StateMachine {
                    id: 1,
                    block: pio.deref(),
                    _phantom: core::marker::PhantomData,
                },
                StateMachine {
                    id: 2,
                    block: pio.deref(),
                    _phantom: core::marker::PhantomData,
                },
                StateMachine {
                    id: 3,
                    block: pio.deref(),
                    _phantom: core::marker::PhantomData,
                },
            ],
            pio,
        }
    }

    /// Free this instance.
    pub fn free(self) -> P {
        self.pio
    }

    /// This PIO's state machines.
    pub fn state_machines(&self) -> &[StateMachine<P>; 4] {
        &self.state_machines
    }

    fn find_offset_for_instructions(&self, i: &[u16], origin: Option<u8>) -> Option<usize> {
        if i.len() > PIO_INSTRUCTION_COUNT {
            None
        } else {
            let mask = (1 << i.len()) - 1;
            if let Some(origin) = origin {
                if origin as usize > PIO_INSTRUCTION_COUNT - i.len()
                    || self.used_instruction_space.get() & (mask << origin) != 0
                {
                    None
                } else {
                    Some(origin as usize)
                }
            } else {
                for i in (32 - i.len())..=0 {
                    if self.used_instruction_space.get() & (mask << i) == 0 {
                        return Some(i);
                    }
                }
                None
            }
        }
    }

    fn add_program(&self, instructions: &[u16], origin: Option<u8>) -> Option<usize> {
        if let Some(offset) = self.find_offset_for_instructions(instructions, origin) {
            for (i, instr) in instructions.iter().enumerate() {
                self.pio.instr_mem[i + offset].write(|w| unsafe { w.bits(*instr as u32) })
            }
            self.used_instruction_space
                .set(self.used_instruction_space.get() | ((1 << instructions.len()) - 1));
            Some(offset)
        } else {
            None
        }
    }
}

/// PIO State Machine.
#[derive(Debug)]
pub struct StateMachine<P: Instance> {
    id: u8,
    block: *const rp2040_pac::pio0::RegisterBlock,
    _phantom: core::marker::PhantomData<P>,
}

impl<P: Instance> StateMachine<P> {
    /// Start and stop the state machine.
    pub fn set_enabled(&self, enabled: bool) {
        let bits = self.block().ctrl.read().sm_enable().bits();
        let bits = (bits & !(1 << self.id)) | ((enabled as u8) << self.id);
        self.block()
            .ctrl
            .write(|w| unsafe { w.sm_enable().bits(bits) });
    }

    fn restart(&self) {
        let bits = self.block().ctrl.read().sm_restart().bits() | 1 << self.id;
        self.block()
            .ctrl
            .write(|w| unsafe { w.sm_restart().bits(bits) });
    }

    fn reset_clock(&self) {
        let bits = self.block().ctrl.read().clkdiv_restart().bits() | 1 << self.id;
        self.block()
            .ctrl
            .write(|w| unsafe { w.clkdiv_restart().bits(bits) });
    }

    fn set_clock_divisor(&self, divisor: f32) {
        // sm frequency = clock freq / (CLKDIV_INT + CLKDIV_FRAC / 256)
        let int = divisor as u16;
        let frac = (divisor.fract() * 256.0) as u8;

        self.sm().sm_clkdiv.write(|w| {
            unsafe {
                w.int().bits(int);
                w.frac().bits(frac);
            }

            w
        });
    }

    /// The address of the instruction currently being executed.
    pub fn instruction_address(&self) -> u32 {
        self.sm().sm_addr.read().bits()
    }

    /// Set the current instruction.
    pub fn set_instruction(&self, instruction: u16) {
        self.sm()
            .sm_instr
            .write(|w| unsafe { w.sm0_instr().bits(instruction) })
    }

    /// Pull a word from the RX FIFO
    pub fn pull(&self) -> u32 {
        self.block().rxf[self.id as usize].read().bits()
    }

    /// Push a word into the TX FIFO
    pub fn push(&self, word: u32) {
        self.block().txf[self.id as usize].write(|w| unsafe { w.bits(word) })
    }

    /// Check if the current instruction is stalled.
    pub fn stalled(&self) -> bool {
        self.sm().sm_execctrl.read().exec_stalled().bits()
    }

    fn block(&self) -> &rp2040_pac::pio0::RegisterBlock {
        unsafe { &*self.block }
    }

    fn sm(&self) -> &rp2040_pac::pio0::SM {
        &self.block().sm[self.id as usize]
    }
}

/// Builder to deploy a fully configured PIO program on one of the state
/// machines.
#[derive(Debug)]
pub struct PIOBuilder<'a> {
    instructions: &'a [u16],
    instruction_offset: Option<u8>,
    // wrap program from top to bottom
    wrap_top: u8,
    wrap_bottom: u8,
    // sideset is optional
    side_en: bool,
    // sideset sets pindirs
    side_pindir: bool,
    // gpio pin used by `jmp pin` instr
    jmp_pin: u8,
    // continuously assert the most recent OUT/SET to the pins.
    out_sticky: bool,
    // use a bit of OUT data as an auxilary write enable.
    // when OUT_STICKY is enabled, setting the bit to 0 deasserts for that instr.
    inline_out_en: bool,
    // which bit to use
    out_en_sel: u8,
    // for `mov x, status`.
    // false -> all ones if tx fifo level < N
    // true -> all ones if rx fifo level < N
    status_sel: bool,
    // base = starting pin
    // count = number of pins
    in_base: u8,
    out_base: u8,
    out_count: u8,
    set_base: u8,
    set_count: u8,
    sideset_base: u8,
    sideset_count: u8,
    // rx fifo steals tx fifo storage to be twice as deep
    fjoin_rx: bool,
    // tx fifo steals rx fifo storage to be twice as deep
    fjoin_tx: bool,
    // enable autopull
    autopull: bool,
    // enable autopush
    autopush: bool,
    // threhold for autopull
    pull_thresh: u8,
    // threshold for autopush
    push_thresh: u8,
    // true = shift out of OSR to right
    in_shiftdir: bool,
    // true = shift into ISR from right
    out_shiftdir: bool,
    clock_divisor: f32,
}

impl<'a> Default for PIOBuilder<'a> {
    fn default() -> Self {
        PIOBuilder {
            instructions: &[],
            instruction_offset: None,
            wrap_top: 0,
            wrap_bottom: 31,
            side_en: false,
            side_pindir: false,
            jmp_pin: 0,
            out_sticky: false,
            inline_out_en: false,
            out_en_sel: 0,
            status_sel: false,
            in_base: 0,
            out_base: 0,
            out_count: 32,
            set_base: 0,
            set_count: 0,
            sideset_base: 0,
            sideset_count: 0,
            fjoin_rx: false,
            fjoin_tx: false,
            autopull: false,
            autopush: false,
            pull_thresh: 32,
            push_thresh: 32,
            in_shiftdir: true,
            out_shiftdir: true,
            clock_divisor: 1.0,
        }
    }
}

/// Buffer sharing configuration.
#[derive(Debug)]
pub enum Buffers {
    /// No sharing.
    RxTx,
    /// The memory of the RX FIFO is given to the TX FIFO to double its depth.
    OnlyTx,
    /// The memory of the TX FIFO is given to the RX FIFO to double its depth.
    OnlyRx,
}

/// Errors that occured during `PIOBuilder::build`.
#[derive(Debug)]
pub enum BuildError {
    /// There was not enough space for the instructions on the selected PIO.
    NoSpace,
}

impl<'a> PIOBuilder<'a> {
    /// Set config settings based on information from the given `pio::Program`.
    /// Additional configuration may be needed in addition to this.
    pub fn with_program<P>(&mut self, p: &'a pio::Program<P>) -> &mut Self {
        self.instructions(p.code());

        self.wrap(p.wrap().0, p.wrap().1);

        self.side_en = p.side_set().optional();
        self.side_pindir = p.side_set().pindirs();

        self.sideset_count = p.side_set().bits();

        self
    }

    /// Set the instructions of the program.
    pub fn instructions(&mut self, instructions: &'a [u16]) -> &mut Self {
        self.instructions = instructions;
        self
    }

    /// Set the wrap top and bottom. The program will automatically jump from the wrap bottom to
    /// the wrap top in 0 cycles.
    pub fn wrap(&mut self, top: u8, bottom: u8) -> &mut Self {
        self.wrap_top = top;
        self.wrap_bottom = bottom;
        self
    }

    /// Set buffer sharing. See `Buffers` for more information.
    pub fn buffers(&mut self, buffers: Buffers) -> &mut Self {
        match buffers {
            Buffers::RxTx => {
                self.fjoin_tx = false;
                self.fjoin_rx = false;
            }
            Buffers::OnlyTx => {
                self.fjoin_rx = false;
                self.fjoin_tx = true;
            }
            Buffers::OnlyRx => {
                self.fjoin_rx = true;
                self.fjoin_tx = false;
            }
        }
        self
    }

    /// 1 for full speed. A clock divisor of `n` will cause the state macine to run 1 cycle every
    /// `n`. For a small `n`, a fractional divisor may introduce unacceptable jitter.
    pub fn clock_divisor(&mut self, divisor: f32) -> &mut Self {
        self.clock_divisor = divisor;
        self
    }

    /// Build the config and deploy it to a StateMachine.
    pub fn build<P: Instance>(self, pio: &PIO<P>, sm: &StateMachine<P>) -> Result<(), BuildError> {
        let offset = match pio.add_program(self.instructions, self.instruction_offset) {
            Some(o) => o,
            None => return Err(BuildError::NoSpace),
        };

        // ### STOP SM ####
        sm.set_enabled(false);

        // ### CONFIGURE SM ###
        sm.sm().sm_execctrl.write(|w| {
            unsafe {
                w.wrap_top().bits(offset as u8 + self.wrap_top);
                w.wrap_bottom().bits(offset as u8 + self.wrap_bottom);
            }

            w.side_en().bit(self.side_en);
            w.side_pindir().bit(self.side_pindir);

            unsafe {
                w.jmp_pin().bits(self.jmp_pin);
            }

            w.out_sticky().bit(self.out_sticky);

            w.inline_out_en().bit(self.inline_out_en);
            unsafe {
                w.out_en_sel().bits(self.out_en_sel);
            }

            w.status_sel().bit(self.status_sel);

            w
        });

        sm.sm().sm_pinctrl.write(|w| {
            unsafe {
                w.in_base().bits(self.in_base);
            }

            unsafe {
                w.out_base().bits(self.out_base);
                w.out_count().bits(self.out_count);
            }

            unsafe {
                w.set_base().bits(self.set_base);
                w.set_count().bits(self.set_count);
            }

            unsafe {
                w.sideset_base().bits(self.sideset_base);
                w.sideset_count().bits(self.sideset_count);
            }

            w
        });

        sm.sm().sm_shiftctrl.write(|w| {
            w.fjoin_rx().bit(self.fjoin_rx);
            w.fjoin_tx().bit(self.fjoin_tx);

            w.autopull().bit(self.autopull);
            w.autopush().bit(self.autopush);

            unsafe {
                w.pull_thresh().bits(self.pull_thresh);
                w.push_thresh().bits(self.push_thresh);
            }

            w.out_shiftdir().bit(self.out_shiftdir);
            w.in_shiftdir().bit(self.in_shiftdir);

            w
        });

        sm.set_clock_divisor(self.clock_divisor);

        // ### RESTART SM & RESET SM CLOCK ###
        sm.restart();
        sm.reset_clock();

        // ### SET SM PC ###
        // set starting location by setting the state machine to execute a jmp
        // to the beginning of the program we loaded in.
        #[allow(clippy::unusual_byte_groupings)]
        let mut instr = 0b000_00000_000_00000; // JMP 0
        instr |= offset as u16;
        sm.set_instruction(instr);

        // ### ENABLE SM ###
        sm.set_enabled(true);

        Ok(())
    }
}
