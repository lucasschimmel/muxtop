//! `IOReport` access for the Apple Silicon backend — the *optional* half.
//!
//! The IORegistry gives utilisation and memory but says nothing about power or
//! clocks. Those counters exist only behind `IOReport`, the subsystem
//! `powermetrics` reads. Two things about it are worth stating plainly,
//! because the v0.5 release notes got one of them wrong:
//!
//! * **It is a private framework.** There is no header, no stability promise
//!   and no documentation. Every symbol here is resolved with `dlopen` at
//!   runtime, exactly as the NVIDIA backend resolves NVML, so a macOS release
//!   that renames or removes them costs muxtop the POWER and CLK columns and
//!   nothing else. The tab keeps working on the IORegistry half alone.
//! * **It does not need root.** The v0.5 notes deferred Apple Silicon on the
//!   grounds that `powermetrics` requires root, and inferred the same of its
//!   data source. It does not: the GPU energy and performance-state channels
//!   are readable by any user. `powermetrics` needs root because it also reads
//!   channels that muxtop never subscribes to.
//!
//! # Counters, not gauges
//!
//! Every channel used here accumulates. `GPU Energy` is joules since boot and
//! the performance states are tick counts since boot, so a single sample means
//! nothing on its own — power and clock are only defined over an interval. The
//! sampler therefore keeps the previous sample and reports `None` until it has
//! two, which costs exactly one tick of `—` at startup.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::array::{
    CFArrayAppendValue, CFArrayCreateMutable, CFArrayGetCount, kCFTypeArrayCallBacks,
};
use core_foundation_sys::base::{CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{
    CFDictionaryCreateMutable, CFDictionaryGetValue, CFDictionaryRef, CFDictionarySetValue,
    CFMutableDictionaryRef, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use core_foundation_sys::string::CFStringRef;

use crate::gpu_engine::GpuError;

/// Where the OS keeps the library. Resolved at runtime, never linked.
const LIB_IOREPORT: &str = "/usr/lib/libIOReport.dylib";

/// Group holding the SoC energy counters.
const GROUP_ENERGY: &str = "Energy Model";
/// Group holding the GPU's own statistics.
const GROUP_GPU_STATS: &str = "GPU Stats";
/// Subgroup naming the GPU's hardware DVFS residency channel.
const SUBGROUP_PERF_STATES: &str = "GPU Performance States";

/// Preferred energy channel — nanojoule resolution.
const CHANNEL_GPU_ENERGY: &str = "GPU Energy";
/// Fallback energy channel, millijoules, present on older chips.
const CHANNEL_GPU: &str = "GPU";

/// The only channels of the `Energy Model` group this backend reads.
///
/// The group carries one channel per power rail on the SoC — 149 of them on an
/// M3, almost all CPU clusters and their SRAM. Subscribing to the group copies
/// every one of them into every sample.
const WANTED_ENERGY_CHANNELS: [&str; 2] = [CHANNEL_GPU_ENERGY, CHANNEL_GPU];

/// Key under which `IOReport` dictionaries carry their channel array.
const KEY_CHANNELS: &str = "IOReportChannels";

/// Shortest interval a delta is reported over.
///
/// The collector runs this at 1 Hz, so the guard never fires in normal
/// operation. It exists for the paths that do not: a `--refresh` far below the
/// documented floor, or a test loop. Well under the 1 s cadence so a tick that
/// arrives early on a loaded machine is still honoured.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

type SubscriptionRef = *const c_void;

/// Raw entry points, resolved once at open.
///
/// Held as plain function pointers rather than `libloading::Symbol` so the
/// struct is not self-referential. They stay valid for as long as `library`
/// is alive, which is guaranteed by field order: Rust drops fields in
/// declaration order, so `library` is declared last and unloaded last.
struct Symbols {
    create_samples:
        unsafe extern "C" fn(SubscriptionRef, CFMutableDictionaryRef, CFTypeRef) -> CFDictionaryRef,
    create_delta:
        unsafe extern "C" fn(CFDictionaryRef, CFDictionaryRef, CFTypeRef) -> CFDictionaryRef,
    channel_group: unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef,
    channel_subgroup: unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef,
    channel_name: unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef,
    channel_unit: unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef,
    simple_value: unsafe extern "C" fn(CFDictionaryRef, i32) -> i64,
    state_count: unsafe extern "C" fn(CFDictionaryRef) -> i32,
    state_name: unsafe extern "C" fn(CFDictionaryRef, i32) -> CFStringRef,
    state_residency: unsafe extern "C" fn(CFDictionaryRef, i32) -> i64,
}

/// One interval's worth of GPU counters.
#[derive(Debug, Clone, Default)]
pub struct IntervalReport {
    /// Energy consumed over the interval and the unit the channel reported it
    /// in — the unit is per-channel metadata, not a constant, so it travels
    /// with the value. See `metrics::energy_delta_to_watts`.
    pub energy: Option<(i64, String)>,
    /// Per-state tick deltas from the hardware DVFS residency channel.
    pub residencies: Vec<(String, u64)>,
    /// How long the interval actually lasted, measured rather than assumed.
    pub elapsed: Duration,
}

/// A live subscription to the GPU's `IOReport` channels.
///
/// Not `Sync`, and deliberately so: sampling mutates the held previous sample.
/// The engine owns exactly one behind a `Mutex`, which is what makes the
/// `Send` implementation below sound.
pub struct IoReportSampler {
    symbols: Symbols,
    subscription: SubscriptionRef,
    channels: CFMutableDictionaryRef,
    previous: Option<(CFDictionaryRef, Instant)>,
    /// Declared last so it is dropped last — see [`Symbols`].
    ///
    /// Never read: its only job is to keep the library mapped for as long as
    /// the function pointers in `symbols` exist. Dropping it calls `dlclose`
    /// and invalidates every one of them.
    #[allow(dead_code, reason = "held to keep the dlopen'd library mapped")]
    library: libloading::Library,
}

// SAFETY: the fields are raw Core Foundation pointers, which are not `Send` by
// default because CF objects have no thread affinity guarantee in general. The
// objects held here (a subscription, a channel dictionary and a sample) are
// plain immutable CF containers with no run-loop or main-thread association,
// and every access goes through `&mut self`. The engine keeps the single
// instance behind a `Mutex`, so at most one thread ever touches it at a time —
// which is exactly the invariant `Send` without `Sync` expresses.
unsafe impl Send for IoReportSampler {}

impl IoReportSampler {
    /// Open the library and subscribe to the GPU channels.
    ///
    /// Fails — rather than panicking or retrying — when the library is absent,
    /// a symbol has been renamed, or the subscription is refused. The engine
    /// treats that as "no power or clock on this host" and carries on.
    pub fn open() -> Result<Self, GpuError> {
        // SAFETY: `Library::new` runs the library's initialisers, and every
        // symbol below is checked for presence before its pointer is stored,
        // so a renamed entry point fails the open rather than calling into a
        // null pointer. The signatures are transcribed from the ABI every
        // IOReport consumer relies on; the loader cannot verify them, which is
        // why the values they produce are range-checked downstream instead of
        // trusted.
        unsafe {
            let library = libloading::Library::new(LIB_IOREPORT).map_err(|e| {
                GpuError::DriverUnavailable {
                    vendor: "Apple",
                    reason: format!("{LIB_IOREPORT} could not be loaded: {e}"),
                }
            })?;

            macro_rules! symbol {
                ($name:literal, $signature:ty) => {{
                    let resolved: libloading::Symbol<$signature> =
                        library
                            .get($name)
                            .map_err(|e| GpuError::DriverUnavailable {
                                vendor: "Apple",
                                reason: format!(
                                    "{} is missing from {LIB_IOREPORT}: {e}",
                                    String::from_utf8_lossy(&$name[..$name.len() - 1])
                                ),
                            })?;
                    *resolved
                }};
            }

            let copy_channels = symbol!(
                b"IOReportCopyChannelsInGroup\0",
                unsafe extern "C" fn(
                    CFStringRef,
                    CFStringRef,
                    u64,
                    u64,
                    u64,
                ) -> CFMutableDictionaryRef
            );
            // `IOReportMergeChannels` is deliberately not resolved. The channel
            // set is now assembled directly (see `select_channels`), so one
            // fewer private symbol has to exist for the backend to start.
            let create_subscription = symbol!(
                b"IOReportCreateSubscription\0",
                unsafe extern "C" fn(
                    *const c_void,
                    CFMutableDictionaryRef,
                    *mut CFMutableDictionaryRef,
                    u64,
                    CFTypeRef,
                ) -> SubscriptionRef
            );

            let symbols = Symbols {
                create_samples: symbol!(
                    b"IOReportCreateSamples\0",
                    unsafe extern "C" fn(
                        SubscriptionRef,
                        CFMutableDictionaryRef,
                        CFTypeRef,
                    ) -> CFDictionaryRef
                ),
                create_delta: symbol!(
                    b"IOReportCreateSamplesDelta\0",
                    unsafe extern "C" fn(
                        CFDictionaryRef,
                        CFDictionaryRef,
                        CFTypeRef,
                    ) -> CFDictionaryRef
                ),
                channel_group: symbol!(
                    b"IOReportChannelGetGroup\0",
                    unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef
                ),
                channel_subgroup: symbol!(
                    b"IOReportChannelGetSubGroup\0",
                    unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef
                ),
                channel_name: symbol!(
                    b"IOReportChannelGetChannelName\0",
                    unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef
                ),
                channel_unit: symbol!(
                    b"IOReportChannelGetUnitLabel\0",
                    unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef
                ),
                simple_value: symbol!(
                    b"IOReportSimpleGetIntegerValue\0",
                    unsafe extern "C" fn(CFDictionaryRef, i32) -> i64
                ),
                state_count: symbol!(
                    b"IOReportStateGetCount\0",
                    unsafe extern "C" fn(CFDictionaryRef) -> i32
                ),
                state_name: symbol!(
                    b"IOReportStateGetNameForIndex\0",
                    unsafe extern "C" fn(CFDictionaryRef, i32) -> CFStringRef
                ),
                state_residency: symbol!(
                    b"IOReportStateGetResidency\0",
                    unsafe extern "C" fn(CFDictionaryRef, i32) -> i64
                ),
            };

            // The GPU Stats group is narrowed by subgroup, which keeps its
            // latency histograms — hundreds of buckets — out of every sample.
            // The Energy Model group cannot be narrowed the same way: its
            // channels carry no subgroup at all, so `IOReportCopyChannelsInGroup`
            // has nothing to filter on and returns all 149 of them. The channel
            // set is therefore assembled here instead, and only what `decode`
            // actually reads goes into it.
            let energy_group = CFString::new(GROUP_ENERGY);
            let gpu_group = CFString::new(GROUP_GPU_STATS);
            let perf_subgroup = CFString::new(SUBGROUP_PERF_STATES);

            let energy_all = copy_channels(
                energy_group.as_concrete_TypeRef(),
                std::ptr::null(),
                0,
                0,
                0,
            );
            if energy_all.is_null() {
                return Err(GpuError::Query(format!(
                    "IOReport has no '{GROUP_ENERGY}' group on this host"
                )));
            }

            let perf_all = copy_channels(
                gpu_group.as_concrete_TypeRef(),
                perf_subgroup.as_concrete_TypeRef(),
                0,
                0,
                0,
            );

            let channels = select_channels(energy_all, perf_all, symbols.channel_name);
            CFRelease(energy_all as CFTypeRef);
            if !perf_all.is_null() {
                CFRelease(perf_all as CFTypeRef);
            }
            let Some(channels) = channels else {
                return Err(GpuError::Query(format!(
                    "IOReport exposes no GPU channel in '{GROUP_ENERGY}' or \
                     '{GROUP_GPU_STATS}/{SUBGROUP_PERF_STATES}' on this host"
                )));
            };

            // The out-parameter is the subscription's own view of the channel
            // set. It is deliberately not released: the ownership convention
            // for it is undocumented, the sampler is created once per process,
            // and a possible one-off leak of a single dictionary is a better
            // trade than a possible double free.
            let mut subscribed: CFMutableDictionaryRef = std::ptr::null_mut();
            let subscription = create_subscription(
                std::ptr::null(),
                channels,
                &mut subscribed,
                0,
                std::ptr::null(),
            );
            if subscription.is_null() {
                CFRelease(channels as CFTypeRef);
                return Err(GpuError::Query(
                    "IOReportCreateSubscription was refused".into(),
                ));
            }

            Ok(Self {
                symbols,
                subscription,
                channels,
                previous: None,
                library,
            })
        }
    }

    /// Sample the GPU channels and return the delta since the previous call.
    ///
    /// Returns `None` on the first call — which establishes the baseline — and
    /// whenever the library refuses a sample. The baseline is deliberately
    /// *not* taken at construction: the gap between connecting and the first
    /// tick is unbounded and can be a millisecond, and dividing an energy
    /// counter that updates on its own schedule by a millisecond produces a
    /// number with no relationship to the GPU's power draw. Waiting for the
    /// collector's own cadence costs one tick of `—` and gets a real interval.
    pub fn sample(&mut self) -> Option<IntervalReport> {
        // SAFETY: every pointer below either comes from a Create-rule call
        // whose result is released on all paths, or is a borrowed channel item
        // read only while its owning sample dictionary is alive. `&mut self`
        // means no other thread is inside this object.
        unsafe {
            let current = self.take_sample();
            if current.is_null() {
                return None;
            }
            let now = Instant::now();

            let Some((previous, taken)) = self.previous.take() else {
                self.previous = Some((current, now));
                return None;
            };

            // Too short a window to divide by. Keep the *older* baseline and
            // discard the sample just taken, so the window widens instead of
            // resetting — a caller polling faster than the counters update
            // then gets a valid reading late rather than a wrong one now.
            if now.duration_since(taken) < MIN_SAMPLE_INTERVAL {
                CFRelease(current as CFTypeRef);
                self.previous = Some((previous, taken));
                return None;
            }

            let delta = (self.symbols.create_delta)(previous, current, std::ptr::null());
            CFRelease(previous as CFTypeRef);
            self.previous = Some((current, now));

            if delta.is_null() {
                return None;
            }
            let mut report = self.decode(delta);
            report.elapsed = now.duration_since(taken);
            CFRelease(delta as CFTypeRef);
            Some(report)
        }
    }

    /// # Safety
    /// Caller must hold `&mut self`; the returned reference follows the Create
    /// rule and must be released.
    unsafe fn take_sample(&self) -> CFDictionaryRef {
        unsafe { (self.symbols.create_samples)(self.subscription, self.channels, std::ptr::null()) }
    }

    /// Walk a delta dictionary's channels and pick out the two we subscribed
    /// for. Unknown channels are skipped rather than guessed at.
    ///
    /// # Safety
    /// `delta` must be a live `IOReport` sample dictionary.
    unsafe fn decode(&self, delta: CFDictionaryRef) -> IntervalReport {
        let mut report = IntervalReport::default();
        let mut have_preferred_energy = false;

        // SAFETY: the array and every item in it are borrowed from `delta`,
        // which the caller keeps alive across this call.
        unsafe {
            let key = CFString::new("IOReportChannels");
            let raw = CFDictionaryGetValue(delta, key.as_CFTypeRef().cast());
            if raw.is_null() {
                return report;
            }
            let items: CFArray<CFDictionary> = CFArray::wrap_under_get_rule(raw.cast());

            for item in items.iter() {
                let channel = item.as_concrete_TypeRef();
                let group = cf_string((self.symbols.channel_group)(channel));

                if group == GROUP_ENERGY {
                    let name = cf_string((self.symbols.channel_name)(channel));
                    // `GPU Energy` counts in nanojoules and `GPU` in
                    // millijoules; both are present on an M3 and agree to
                    // within rounding. Prefer the finer one and let the other
                    // stand in on chips that only expose it.
                    let preferred = name == CHANNEL_GPU_ENERGY;
                    if preferred || (name == CHANNEL_GPU && !have_preferred_energy) {
                        let value = (self.symbols.simple_value)(channel, 0);
                        let unit = cf_string((self.symbols.channel_unit)(channel));
                        report.energy = Some((value, unit));
                        have_preferred_energy |= preferred;
                    }
                    continue;
                }

                if group == GROUP_GPU_STATS
                    && cf_string((self.symbols.channel_subgroup)(channel)) == SUBGROUP_PERF_STATES
                {
                    let count = (self.symbols.state_count)(channel);
                    for index in 0..count {
                        let name = cf_string((self.symbols.state_name)(channel, index));
                        let ticks = (self.symbols.state_residency)(channel, index);
                        // A negative residency means the counter was reset
                        // under us; treating it as zero keeps the rest of the
                        // interval usable.
                        report.residencies.push((name, ticks.max(0) as u64));
                    }
                }
            }
        }

        report
    }
}

impl Drop for IoReportSampler {
    fn drop(&mut self) {
        // SAFETY: each pointer was obtained from a Create/Copy-rule call and
        // is released exactly once here. `previous` is taken so a second drop
        // cannot see it.
        unsafe {
            if let Some((sample, _)) = self.previous.take() {
                CFRelease(sample as CFTypeRef);
            }
            if !self.channels.is_null() {
                CFRelease(self.channels as CFTypeRef);
            }
            if !self.subscription.is_null() {
                CFRelease(self.subscription as CFTypeRef);
            }
        }
    }
}

/// Assemble the subscription's channel set from just the channels `decode`
/// reads: the GPU energy counter, and the GPU's DVFS residency channel.
///
/// `IOReportCopyChannelsInGroup` can narrow a group by subgroup, which is how
/// the GPU Stats group is trimmed to its performance states. That lever does
/// not exist for the `Energy Model` group: every one of its channels reports an
/// empty subgroup, so the call returns the whole group — 149 channels on an
/// M3, one per power rail on the SoC, overwhelmingly CPU clusters and their
/// SRAM. The subscription is therefore built here, and holds 3 channels rather
/// than 150.
///
/// The gain is CPU, not memory. `decode` walks every channel in every delta
/// and turns each one's group name into an owned `String`, so a wide
/// subscription paid ~150 allocations and comparisons per tick to find two
/// channels. Narrowing it takes `AppleEngine::snapshot` from 3.63 ms to
/// 3.06 ms on an M3 (median of 60, −16 %). Peak RSS and physical footprint do
/// **not** move measurably — that was the expected win and it did not
/// materialise, so it is not claimed. Whatever the Apple backend's ~1.5 MiB of
/// footprint is, it is not these channels.
///
/// Returns `None` when nothing matched, which is the honest signal that this
/// host publishes neither counter under a name we know — the caller turns that
/// into "no power or clock here" rather than subscribing to something useless.
///
/// # Safety
/// `energy` must be a live channel dictionary; `perf` may be null. Neither is
/// consumed — the caller still owns both and must release them. The returned
/// dictionary follows the Create rule.
unsafe fn select_channels(
    energy: CFMutableDictionaryRef,
    perf: CFMutableDictionaryRef,
    channel_name: unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef,
) -> Option<CFMutableDictionaryRef> {
    // SAFETY: the array retains every channel appended to it (it is created
    // with the CF type callbacks), so the picked channels outlive the source
    // dictionaries the caller releases immediately after this returns.
    unsafe {
        let key = CFString::new(KEY_CHANNELS);
        let picked = CFArrayCreateMutable(kCFAllocatorDefault, 0, &kCFTypeArrayCallBacks);

        let append_matching = |source: CFMutableDictionaryRef, filter: bool| {
            if source.is_null() {
                return;
            }
            let raw = CFDictionaryGetValue(source as CFDictionaryRef, key.as_CFTypeRef().cast());
            if raw.is_null() {
                return;
            }
            let items: CFArray<CFDictionary> = CFArray::wrap_under_get_rule(raw.cast());
            for item in items.iter() {
                let channel = item.as_concrete_TypeRef();
                let keep = !filter || {
                    let name = cf_string(channel_name(channel));
                    WANTED_ENERGY_CHANNELS.contains(&name.as_str())
                };
                if keep {
                    CFArrayAppendValue(picked, channel.cast());
                }
            }
        };

        // The energy group is filtered by name; the performance-state subgroup
        // is already exactly one channel and is taken whole.
        append_matching(energy, true);
        append_matching(perf, false);

        if CFArrayGetCount(picked) == 0 {
            CFRelease(picked as CFTypeRef);
            return None;
        }

        let selected = CFDictionaryCreateMutable(
            kCFAllocatorDefault,
            0,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        CFDictionarySetValue(selected, key.as_CFTypeRef().cast(), picked.cast());
        CFRelease(picked as CFTypeRef);
        Some(selected)
    }
}

/// Borrowed `CFStringRef` to `String`, empty for null.
///
/// # Safety
/// `raw` must be null or a live `CFString`.
unsafe fn cf_string(raw: CFStringRef) -> String {
    if raw.is_null() {
        return String::new();
    }
    unsafe { CFString::wrap_under_get_rule(raw).to_string() }
}
