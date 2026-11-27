//! What the micro benches measure: wall time by default, or user-space
//! instructions retired under `BENCH_MEASURE=instructions`, the count the
//! regression gate compares. Wall time moves with code placement alone,
//! by 16-24% on byte-identical code; the instruction count does not.

use criterion::measurement::{Measurement, ValueFormatter};
use criterion::Throughput;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Whether `BENCH_MEASURE` asks for instructions. Anything but the two
/// known values panics: a typo must not gate on the wrong unit.
pub fn instructions_requested() -> bool {
    match std::env::var("BENCH_MEASURE").as_deref() {
        Ok("instructions") => true,
        Ok("time") | Err(std::env::VarError::NotPresent) => false,
        other => panic!("BENCH_MEASURE must be instructions or time, got {other:?}"),
    }
}

/// Whether the command line names a criterion baseline itself.
pub fn baseline_flag_given() -> bool {
    const FLAGS: [&str; 6] = [
        "-s",
        "-b",
        "--save-baseline",
        "--baseline",
        "--baseline-lenient",
        "--discard-baseline",
    ];
    std::env::args().skip(1).any(|arg| {
        FLAGS
            .iter()
            .any(|f| arg == *f || arg.starts_with(&format!("{f}=")))
    })
}

/// `main` for a gated bench binary: each target runs once, under the
/// measurement `BENCH_MEASURE` picks.
macro_rules! bench_main {
    ($config:expr; $($target:path),+ $(,)?) => {
        fn main() {
            if common::instructions_requested() {
                // No plots: samples that never vary give plotters a NaN
                // range, and it panics.
                let mut c = $config
                    .with_measurement(common::Instructions::open())
                    .without_plots()
                    .configure_from_args();
                // Its own criterion baseline, set after the args, which
                // reset it to "base": compared with the wall-time runs
                // there, every change line would be nonsense.
                if !common::baseline_flag_given() {
                    c = c.save_baseline("instructions".to_owned());
                }
                $($target(&mut c);)+
                c.final_summary();
            } else {
                let mut c = $config.configure_from_args();
                $($target(&mut c);)+
                c.final_summary();
            }
        }
    };
}

/// `perf_event_attr` up to `PERF_ATTR_SIZE_VER0`, which every kernel since
/// 2.6.31 accepts; the fields past it stay zero.
#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 2;
// perf_event_paranoid 2 admits a thread counting itself only with both.
const EXCLUDE_KERNEL: u64 = 1 << 5;
const EXCLUDE_HV: u64 = 1 << 6;
const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 1 << 3;

/// Instructions this thread retires in user space. Criterion runs every
/// routine on the thread that opened the counter.
pub struct Instructions {
    fd: OwnedFd,
}

impl Instructions {
    /// Panics where the PMU cannot be read (no counters in a VM,
    /// perf_event_paranoid above 2): a gate must not fall back silently.
    pub fn open() -> Self {
        let attr = PerfEventAttr {
            kind: PERF_TYPE_HARDWARE,
            size: std::mem::size_of::<PerfEventAttr>() as u32,
            config: PERF_COUNT_HW_INSTRUCTIONS,
            read_format: PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING,
            flags: EXCLUDE_KERNEL | EXCLUDE_HV,
            ..PerfEventAttr::default()
        };
        // SAFETY: attr outlives the call; pid 0 and cpu -1 count the calling
        // thread on any CPU, group -1 opens a lone counter.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfEventAttr,
                0 as libc::pid_t,
                -1 as libc::c_int,
                -1 as libc::c_int,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd < 0 {
            panic!(
                "perf_event_open(instructions) failed: {}; \
                 BENCH_MEASURE=instructions needs a readable PMU",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: the kernel just returned this fd, owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
        Self { fd }
    }

    fn read(&self) -> u64 {
        let mut values = [0u64; 3];
        // SAFETY: values is 24 writable bytes, the size read_format asks for.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                values.as_mut_ptr().cast(),
                std::mem::size_of_val(&values),
            )
        };
        assert_eq!(
            n,
            24,
            "perf counter read: {}",
            std::io::Error::last_os_error()
        );
        let [count, enabled, running] = values;
        // A multiplexed counter reports a scaled estimate, not a count.
        assert_eq!(
            enabled, running,
            "instruction counter was multiplexed: another perf user shares the PMU"
        );
        count
    }
}

impl Measurement for Instructions {
    type Intermediate = u64;
    type Value = u64;

    fn start(&self) -> u64 {
        self.read()
    }

    fn end(&self, start: u64) -> u64 {
        self.read() - start
    }

    fn add(&self, a: &u64, b: &u64) -> u64 {
        a + b
    }

    fn zero(&self) -> u64 {
        0
    }

    fn to_f64(&self, value: &u64) -> f64 {
        *value as f64
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &InstructionFormatter
    }
}

struct InstructionFormatter;

impl ValueFormatter for InstructionFormatter {
    fn scale_values(&self, _typical: f64, _values: &mut [f64]) -> &'static str {
        "inst"
    }

    fn scale_throughputs(
        &self,
        _typical: f64,
        throughput: &Throughput,
        values: &mut [f64],
    ) -> &'static str {
        let (per, unit) = match *throughput {
            Throughput::Bytes(n) | Throughput::BytesDecimal(n) => (n, "inst/B"),
            Throughput::Elements(n) => (n, "inst/elem"),
            Throughput::Bits(n) => (n, "inst/bit"),
        };
        for v in values.iter_mut() {
            *v /= per as f64;
        }
        unit
    }

    fn scale_for_machines(&self, _values: &mut [f64]) -> &'static str {
        "inst"
    }
}
